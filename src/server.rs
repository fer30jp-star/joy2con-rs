// SPDX-License-Identifier: GPL-3.0-or-later
//! Newline-delimited JSON over a Unix socket, readable only by this user.
//!
//! Clients receive {"type": "state", ...} snapshots at 60 Hz (with any new
//! log events attached) and send {"action": name, "args": {...}} requests,
//! each answered by {"type": "result", ...}. Same protocol as the Python joy2con.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::daemon::Manager;
use crate::util::socket_path;

pub struct Server {
    inode: u64,
}

type Clients = Arc<Mutex<Vec<SyncSender<Arc<String>>>>>;

impl Server {
    pub fn start(mgr: Arc<Manager>) -> anyhow::Result<Server> {
        let path = socket_path();
        if UnixStream::connect(&path).is_ok() {
            anyhow::bail!("joy2con is already running (socket {})", path.display());
        }
        let _ = std::fs::remove_file(&path);
        let old = unsafe { libc::umask(0o177) };
        let listener = UnixListener::bind(&path);
        unsafe { libc::umask(old) };
        let listener = listener?;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        let inode = std::fs::metadata(&path)?.ino();
        log::info!("listening on {}", path.display());

        let clients: Clients = Arc::new(Mutex::new(Vec::new()));
        {
            let (mgr, clients) = (mgr.clone(), clients.clone());
            std::thread::Builder::new().name("accept".into()).spawn(move || {
                for stream in listener.incoming().flatten() {
                    let (tx, rx) = sync_channel::<Arc<String>>(8);
                    clients.lock().push(tx.clone());
                    let mgr = mgr.clone();
                    let _ =
                        std::thread::Builder::new().name("client".into()).spawn(move || client(mgr, stream, tx, rx));
                }
            })?;
        }
        std::thread::Builder::new().name("broadcast".into()).spawn(move || broadcast(mgr, clients))?;
        Ok(Server { inode })
    }

    /// Delete the socket file, unless it now belongs to another instance.
    pub fn remove_socket(&self) {
        let path = socket_path();
        if let Ok(m) = std::fs::metadata(&path)
            && m.ino() == self.inode
        {
            let _ = std::fs::remove_file(&path);
        }
    }
}

fn client(mgr: Arc<Manager>, stream: UnixStream, tx: SyncSender<Arc<String>>, rx: Receiver<Arc<String>>) {
    let Ok(mut writer) = stream.try_clone() else { return };
    let _ = writer.set_write_timeout(Some(Duration::from_secs(5)));
    let w = std::thread::spawn(move || {
        while let Ok(line) = rx.recv() {
            if writer.write_all(line.as_bytes()).is_err() {
                break;
            }
        }
        let _ = writer.shutdown(std::net::Shutdown::Both);
    });
    let (events, _) = mgr.events_since(0);
    let _ = tx.send(Arc::new(json!({"type": "events", "events": events}).to_string() + "\n"));
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let res = match serde_json::from_str::<Value>(&line) {
            Ok(m) => {
                mgr.action(m.get("action").and_then(|v| v.as_str()).unwrap_or(""), m.get("args").unwrap_or(&json!({})))
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        };
        if tx.send(Arc::new(json!({"type": "result", "result": res}).to_string() + "\n")).is_err() {
            break;
        }
    }
    drop(tx);
    let _ = w.join();
}

fn broadcast(mgr: Arc<Manager>, clients: Clients) {
    let (_, mut last_seq) = mgr.events_since(u64::MAX);
    let mut last_consumers = Instant::now() - Duration::from_secs(10);
    loop {
        std::thread::sleep(Duration::from_micros(16_667));
        if clients.lock().is_empty() {
            continue;
        }
        if last_consumers.elapsed() > Duration::from_secs(2) {
            last_consumers = Instant::now();
            mgr.refresh_consumers();
        }
        let mut msg = json!({"type": "state", "state": mgr.snapshot()});
        let (events, seq) = mgr.events_since(last_seq);
        if !events.is_empty() {
            msg["events"] = Value::Array(events);
        }
        last_seq = seq;
        let line = Arc::new(msg.to_string() + "\n");
        // A stalled client is skipped rather than queued without bound.
        clients.lock().retain(|c| !matches!(c.try_send(line.clone()), Err(TrySendError::Disconnected(_))));
    }
}
