// SPDX-License-Identifier: GPL-3.0-or-later
//! joy2con daemon: Switch 2 Joy-Cons as original Switch controllers on Linux.
//!
//! The daemon watches for Joy-Con 2 advertisements, connects to each
//! controller (pairing it to this PC the first time, so later a button press
//! reconnects it), and feeds the halves into emulated original Switch
//! controllers: one Pro Controller for the pair, or one Joy-Con device per
//! half. A Joy-Con in mouse mode drives a virtual mouse instead. A local
//! client API on a Unix socket (see `server`) feeds the app.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Weak};
use std::time::Duration;

use futures::StreamExt;
use parking_lot::{Condvar, Mutex};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::mouse::{MouseConfig, MouseTracker, VirtualMouse};
use crate::ns2::{self, JoyCon2, State};
use crate::switch1::{self, HostEvent, Inputs, Rumble, SwitchController};
use crate::util::{config_path, mac_from_le, now, wall};

const NINTENDO: u16 = 0x0553;
const GRACE_S: f64 = 4.0; // keep an emulated device this long after its Joy-Cons drop
const RUMBLE_PERIOD: Duration = Duration::from_millis(10);
const RUMBLE_MAX: f64 = 450.0; // of 1023; full scale is uncomfortably strong

// ------------------------------------------------------------------- config

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub mode: String,
    pub controllers: Map<String, Value>,
    pub gyro_autocal: bool,
    pub imu_source: String,
    pub mouse: MouseConfig,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            mode: "pro".into(),
            controllers: Map::new(),
            gyro_autocal: true,
            imu_source: "right".into(),
            mouse: MouseConfig::default(),
            extra: Map::new(),
        }
    }
}

fn load_config() -> Config {
    std::fs::read_to_string(config_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn save_config(cfg: &Config) {
    let path = config_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("tmp");
    if let Ok(s) = serde_json::to_string_pretty(cfg)
        && std::fs::write(&tmp, s).is_ok()
    {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Original-Switch rumble frequency -> Switch 2 10-bit frequency code
/// (0x112 = 160 Hz, 117 codes per octave).
fn freq_code(hz: f64) -> u16 {
    if hz <= 0.0 {
        return 0x112;
    }
    (0x112 as f64 + 117.0 * (hz / 160.0).log2()).round().clamp(0.0, 1023.0) as u16
}

// ------------------------------------------------------------------ rumble

/// Streams HD rumble packets to one Joy-Con while the host asks for rumble.
pub struct Rumbler {
    state: Mutex<(Rumble, f64)>, // (state, timed-test deadline)
    cv: Condvar,
    alive: AtomicBool,
}

impl Rumbler {
    fn start(jc: Arc<JoyCon2>) -> Arc<Rumbler> {
        let r = Arc::new(Rumbler {
            state: Mutex::new(((320.0, 0.0, 160.0, 0.0), 0.0)),
            cv: Condvar::new(),
            alive: AtomicBool::new(true),
        });
        let rr = r.clone();
        let _ = std::thread::Builder::new().name(format!("rumble-{}", jc.side())).spawn(move || rr.run(jc));
        r
    }

    fn set(&self, state: Rumble, duration: Option<f64>) {
        let mut g = self.state.lock();
        *g = (state, duration.map(|d| now() + d).unwrap_or(0.0));
        self.cv.notify_one();
    }

    pub fn level(&self) -> f64 {
        let g = self.state.lock();
        g.0.1.max(g.0.3)
    }

    fn run(&self, jc: Arc<JoyCon2>) {
        let mut sent_zero = true;
        while self.alive.load(Ordering::Relaxed) {
            let (hi_hz, hi_amp, lo_hz, lo_amp) = {
                let mut g = self.state.lock();
                if g.1 > 0.0 && now() > g.1 {
                    *g = ((320.0, 0.0, 160.0, 0.0), 0.0);
                }
                let active = g.0.1 > 0.0 || g.0.3 > 0.0;
                if !active && sent_zero {
                    self.cv.wait(&mut g);
                    continue;
                }
                g.0
            };
            let active = hi_amp > 0.0 || lo_amp > 0.0;
            jc.rumble(freq_code(hi_hz), (hi_amp * RUMBLE_MAX) as u16, freq_code(lo_hz), (lo_amp * RUMBLE_MAX) as u16);
            sent_zero = !active;
            std::thread::sleep(RUMBLE_PERIOD);
        }
    }

    fn stop(&self) {
        self.alive.store(false, Ordering::Relaxed);
        let _g = self.state.lock();
        self.cv.notify_one();
    }
}

// ----------------------------------------------------------------- manager

enum Msg {
    LinkLost(Arc<JoyCon2>),
    Host(String, HostEvent),
    RetireDue,
    Feedback(Arc<JoyCon2>, u8),
}

struct EventLog {
    events: VecDeque<Value>,
    seq: u64,
}

pub struct Inner {
    pub config: Config,
    pub joycons: HashMap<String, Arc<JoyCon2>>,
    rumblers: HashMap<String, Arc<Rumbler>>,
    emulated: HashMap<String, Arc<SwitchController>>,
    retire: HashMap<String, f64>,
    connecting: HashSet<String>,
    ignore_until: HashMap<String, f64>,
    takeover: HashSet<String>,
    seen: BTreeMap<String, Value>,
    pub scanning: &'static str,
    consumers: HashMap<String, Value>,
    nodes: HashMap<String, Value>,
    foreign_discovery: bool,
    trackers: HashMap<String, MouseTracker>,
    vmouse: Option<VirtualMouse>,
    vmouse_error: Option<String>,
    conn: ConnStatus,
}

/// What the app's "Intentar conectar" summary is built from.
#[derive(Clone, Debug, Default, Serialize)]
struct ConnStatus {
    attempts: u32,
    failures: u32,
    adapter_resets: u32,
    last_error: Option<String>,
    last_error_t: f64,
    requested_at: f64,
    connecting_side: Option<String>,
    last_connected_t: f64,
    adapter_present: bool,
    adapter_powered: bool,
}

/// Identity of the Bluetooth adapter: its hci name and its USB device number,
/// which changes whenever the adapter drops off the bus and comes back.
fn adapter_ident() -> Option<(String, String)> {
    let mut names: Vec<String> = std::fs::read_dir("/sys/class/bluetooth")
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with("hci") && !n.contains(':'))
        .collect();
    names.sort();
    let name = names.into_iter().next()?;
    let dev = std::fs::canonicalize(format!("/sys/class/bluetooth/{name}/device")).ok();
    let devnum = dev
        .and_then(|d| d.parent().map(|p| p.join("devnum")))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    Some((name, devnum))
}

pub struct Manager {
    pub host: String,
    key: [u8; 16],
    pub inner: Mutex<Inner>,
    log: Mutex<EventLog>,
    tx: Mutex<Sender<Msg>>,
    pub scan_wake: tokio::sync::Notify,
    connect_delay: f64,
    adapter: Mutex<Option<(String, String)>>,
    bt: Mutex<Option<(tokio::runtime::Handle, bluer::Session)>>,
    me: Weak<Manager>,
}

fn pairing_key(host: &str) -> [u8; 16] {
    let h = Sha256::digest(format!("joy2con pairing key {host}").as_bytes());
    let mut k = [0u8; 16];
    k.copy_from_slice(&h[..16]);
    k
}

impl Manager {
    pub fn new(host: String) -> Arc<Manager> {
        let (tx, rx) = channel::<Msg>();
        let connect_delay = std::env::var("JOY2CON_CONNECT_DELAY").ok().and_then(|v| v.parse().ok()).unwrap_or(0.0);
        let mgr = Arc::new_cyclic(|me| Manager {
            key: pairing_key(&host),
            host: host.clone(),
            inner: Mutex::new(Inner {
                config: load_config(),
                joycons: HashMap::new(),
                rumblers: HashMap::new(),
                emulated: HashMap::new(),
                retire: HashMap::new(),
                connecting: HashSet::new(),
                ignore_until: HashMap::new(),
                takeover: HashSet::new(),
                seen: BTreeMap::new(),
                scanning: "off",
                consumers: HashMap::new(),
                nodes: HashMap::new(),
                foreign_discovery: false,
                trackers: HashMap::new(),
                vmouse: None,
                vmouse_error: None,
                conn: ConnStatus { adapter_present: true, adapter_powered: true, ..Default::default() },
            }),
            log: Mutex::new(EventLog { events: VecDeque::new(), seq: 0 }),
            tx: Mutex::new(tx),
            scan_wake: tokio::sync::Notify::new(),
            connect_delay,
            adapter: Mutex::new(adapter_ident()),
            bt: Mutex::new(None),
            me: me.clone(),
        });
        let worker = Arc::downgrade(&mgr);
        std::thread::Builder::new()
            .name("manager".into())
            .spawn(move || {
                while let Ok(msg) = rx.recv() {
                    let Some(m) = worker.upgrade() else { break };
                    m.handle(msg);
                }
            })
            .expect("manager thread");
        mgr.event(&format!("joy2con started; Bluetooth adapter {host}"), "info");
        mgr
    }

    fn send(&self, msg: Msg) {
        let _ = self.tx.lock().send(msg);
    }

    fn handle(&self, msg: Msg) {
        match msg {
            Msg::LinkLost(jc) => self.handle_lost(&jc),
            Msg::Host(key, ev) => match ev {
                HostEvent::Rumble { block, state } => self.host_rumble(&key, block, state),
                HostEvent::Leds { on, flash } => self.host_leds(&key, on, flash),
                HostEvent::PowerOff => {
                    self.event("host asked the controller to power off", "info");
                    let sides: Vec<&str> = if key == "pro" { vec!["left", "right"] } else { vec![key.as_str()] };
                    for s in sides {
                        self.disconnect(s);
                    }
                }
            },
            Msg::RetireDue => self.retire_due(),
            Msg::Feedback(jc, sample) => {
                let _ = jc.play_sample(sample);
            }
        }
    }

    pub fn event(&self, text: &str, level: &str) {
        if level == "warn" {
            log::warn!("{text}");
        } else {
            log::info!("{text}");
        }
        let mut l = self.log.lock();
        l.seq += 1;
        let seq = l.seq;
        l.events.push_back(json!({"seq": seq, "t": wall(), "text": text, "level": level}));
        if l.events.len() > 200 {
            l.events.pop_front();
        }
    }

    pub fn events_since(&self, seq: u64) -> (Vec<Value>, u64) {
        let l = self.log.lock();
        let out = l.events.iter().filter(|e| e["seq"].as_u64().unwrap_or(0) > seq).cloned().collect();
        (out, l.seq)
    }

    // ------------------------------------------------------------ scanning

    pub fn wanted_sides(inner: &Inner) -> Vec<&'static str> {
        ["left", "right"].into_iter().filter(|s| !inner.joycons.contains_key(*s)).collect()
    }

    /// One advertisement (or advertisement update) from a device.
    pub fn on_advertisement(&self, mac: &str, data: &[u8], rssi: i16, addr_type: &str) {
        if data.len() < 16 {
            return;
        }
        let pid = u16::from_le_bytes([data[5], data[6]]);
        let Some(side) = ns2::side_by_pid(pid).filter(|s| *s != "pro") else { return };
        let mac = mac.to_uppercase();
        let host = mac_from_le(&data[10..16]);
        let kind = if host == "00:00:00:00:00:00" {
            "sync"
        } else if host == self.host {
            "reconnect"
        } else {
            "other-host"
        };
        let mut inner = self.inner.lock();
        let known = inner.config.controllers.contains_key(&mac);
        inner.seen.insert(
            mac.clone(),
            json!({"mac": mac, "side": side, "kind": kind, "host": host, "rssi": rssi, "t": wall(), "known": known}),
        );
        if inner.joycons.contains_key(side) || inner.connecting.contains(&mac) {
            return;
        }
        if now() < inner.ignore_until.get(&mac).copied().unwrap_or(0.0) {
            return;
        }
        if kind == "other-host" && !inner.takeover.contains(&mac) {
            return;
        }
        inner.connecting.insert(mac.clone());
        drop(inner);
        self.scan_wake.notify_one(); // stop scanning now; see connect
        let me = self.me.upgrade().expect("manager");
        let (side, kind, addr_type) = (side.to_string(), kind.to_string(), addr_type.to_string());
        let _ = std::thread::Builder::new()
            .name(format!("connect-{side}"))
            .spawn(move || me.connect(mac, side, kind, addr_type));
    }

    /// Count adapter resets: its USB device number changes when it drops off the bus.
    pub fn check_adapter(&self) {
        let now_id = adapter_ident();
        let changed = {
            let mut cur = self.adapter.lock();
            let changed = matches!((&*cur, &now_id), (Some(a), Some(b)) if a != b);
            if now_id.is_some() {
                *cur = now_id.clone();
            }
            changed
        };
        let mut inner = self.inner.lock();
        inner.conn.adapter_present = now_id.is_some();
        if changed {
            inner.conn.adapter_resets += 1;
            drop(inner);
            self.event("the Bluetooth adapter reset itself (it dropped off USB and came back)", "warn");
        }
    }

    /// The app's "Intentar conectar": forget recent failures, make sure
    /// Bluetooth is on and scan right away.
    fn connect_now(&self) {
        {
            let mut inner = self.inner.lock();
            inner.ignore_until.clear();
            let present = inner.conn.adapter_present;
            inner.conn = ConnStatus {
                requested_at: wall(),
                adapter_present: present,
                adapter_powered: true,
                ..Default::default()
            };
        }
        if let Some((rt, session)) = self.bt.lock().clone() {
            rt.spawn(async move {
                if let Ok(a) = session.default_adapter().await
                    && !a.is_powered().await.unwrap_or(true)
                {
                    let _ = a.set_powered(true).await;
                }
            });
        }
        self.event("connect requested from the app; looking for Joy-Cons", "info");
        self.scan_wake.notify_one();
    }

    fn expire_seen(&self) {
        let t = wall();
        self.inner.lock().seen.retain(|_, v| t - v["t"].as_f64().unwrap_or(0.0) <= 15.0);
    }

    // ---------------------------------------------------------- connections

    fn connect(&self, mac: String, side: String, kind: String, addr_type: String) {
        // An LE connection can't be created while discovery holds the controller.
        for _ in 0..40 {
            if self.inner.lock().scanning == "off" {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        // Some adapters (e.g. UGREEN BT5.4) reset if the connection starts while
        // the controller is still leaving scan mode; give it time to settle.
        if self.connect_delay > 0.0 {
            std::thread::sleep(Duration::from_secs_f64(self.connect_delay));
        }
        let what = match kind.as_str() {
            "sync" => "in sync mode",
            "reconnect" => "reconnecting",
            _ => "taking over",
        };
        self.event(&format!("{side} Joy-Con {mac} {what}; connecting"), "info");
        {
            let mut inner = self.inner.lock();
            inner.conn.attempts += 1;
            inner.conn.connecting_side = Some(side.clone());
        }
        let w1 = self.me.clone();
        let w2 = self.me.clone();
        let jc = JoyCon2::new(
            &mac,
            &side,
            &self.host,
            self.key,
            &addr_type,
            Box::new(move |jc, s| {
                if let Some(m) = w1.upgrade() {
                    m.on_state(jc, s);
                }
            }),
            Box::new(move |jc| {
                if let Some(m) = w2.upgrade() {
                    m.send(Msg::LinkLost(jc.clone()));
                }
            }),
        );
        jc.live.lock().gyro_cal.enabled = self.inner.lock().config.gyro_autocal;
        if let Err(e) = jc.start() {
            self.event(&format!("{side} Joy-Con {mac}: connection failed: {e}"), "warn");
            jc.disconnect();
            let mut inner = self.inner.lock();
            inner.connecting.remove(&mac);
            inner.ignore_until.insert(mac.clone(), now() + 1.0);
            inner.conn.failures += 1;
            inner.conn.last_error = Some(format!("{side}: {e}"));
            inner.conn.last_error_t = wall();
            inner.conn.connecting_side = None;
            drop(inner);
            // A failed attempt often takes the adapter down with it; notice that now.
            std::thread::sleep(Duration::from_millis(500));
            self.check_adapter();
            self.scan_wake.notify_one();
            return;
        }
        let side = jc.side();
        let info = jc.info();
        {
            let mut inner = self.inner.lock();
            inner.connecting.remove(&mac);
            inner.takeover.remove(&mac);
            inner.conn.connecting_side = None;
            inner.conn.last_connected_t = wall();
            if let Some(old) = inner.joycons.get(&side)
                && !Arc::ptr_eq(old, &jc)
            {
                old.disconnect();
            }
            inner.joycons.insert(side.clone(), jc.clone());
            if let Some(r) = inner.rumblers.insert(side.clone(), Rumbler::start(jc.clone())) {
                r.stop();
            }
            inner.trackers.entry(side.clone()).or_default().reset();
            let entry = inner.config.controllers.entry(mac.clone()).or_insert_with(|| json!({}));
            if let Some(o) = entry.as_object_mut() {
                o.insert("side".into(), json!(side));
                o.insert("mac".into(), json!(mac));
                for k in ["serial", "colors", "firmware"] {
                    if let Some(v) = info.get(k) {
                        o.insert(k.into(), v.clone());
                    }
                }
                o.insert("paired".into(), json!(info.get("paired").and_then(|v| v.as_bool()).unwrap_or(false)));
                o.insert("last_seen".into(), json!(wall()));
            }
            save_config(&inner.config);
            inner.seen.remove(&mac);
        }
        let paired = info.get("paired").and_then(|v| v.as_bool()).unwrap_or(false);
        self.event(
            &format!(
                "{side} Joy-Con connected ({}, firmware {}{})",
                info.get("serial").and_then(|v| v.as_str()).unwrap_or("?"),
                info.get("firmware").and_then(|v| v.as_str()).unwrap_or("?"),
                if paired { ", paired to this PC" } else { "" }
            ),
            "info",
        );
        self.ensure_emulated();
        self.greet(&jc);
        self.scan_wake.notify_one();
    }

    /// Connection feedback like the console's: player LEDs and a click.
    fn greet(&self, jc: &Arc<JoyCon2>) {
        let leds = {
            let inner = self.inner.lock();
            let emu = inner.emulated.get("pro").or_else(|| inner.emulated.get(&jc.side())).cloned();
            emu.map(|e| e.st.lock().player_leds & 0x0F).filter(|l| *l != 0).unwrap_or(0x01)
        };
        let _ = jc.set_leds(leds, false);
        let _ = jc.play_sample(3);
    }

    fn handle_lost(&self, jc: &Arc<JoyCon2>) {
        let side = jc.side();
        {
            let mut inner = self.inner.lock();
            match inner.joycons.get(&side) {
                Some(cur) if Arc::ptr_eq(cur, jc) => {}
                _ => return,
            }
            inner.joycons.remove(&side);
            if let Some(r) = inner.rumblers.remove(&side) {
                r.stop();
            }
            let Inner { trackers, vmouse, .. } = &mut *inner;
            if let (Some(t), Some(m)) = (trackers.get_mut(&side), vmouse.as_mut()) {
                t.release(m);
            }
            self.release(&mut inner, &side);
        }
        let paired = jc.info().get("paired").and_then(|v| v.as_bool()).unwrap_or(false);
        self.event(
            &if paired {
                format!("{side} Joy-Con disconnected; press any button on it to reconnect")
            } else {
                format!("{side} Joy-Con disconnected")
            },
            "info",
        );
        self.schedule_retire();
        self.scan_wake.notify_one();
    }

    pub fn disconnect(&self, side: &str) {
        let jc = self.inner.lock().joycons.get(side).cloned();
        if let Some(jc) = jc {
            self.inner.lock().ignore_until.insert(jc.mac.clone(), now() + 8.0);
            jc.disconnect();
            // A link we close ourselves reports no loss, so remove it here.
            self.handle_lost(&jc);
        }
    }

    // ------------------------------------------------------- emulated devices

    fn emulated_keys(inner: &Inner) -> Vec<String> {
        if inner.config.mode == "pro" {
            if inner.joycons.is_empty() { vec![] } else { vec!["pro".into()] }
        } else {
            inner.joycons.keys().cloned().collect()
        }
    }

    fn pro_mac(&self) -> String {
        let h = Sha256::digest(format!("joy2con pro {}", self.host).as_bytes());
        format!("98:B6:E9:{:02X}:{:02X}:{:02X}", h[0], h[1], h[2]) // Nintendo-style prefix, stable per PC
    }

    fn colors(inner: &Inner, key: &str) -> Value {
        let c = |side: &str, part: &str| -> Option<String> {
            let from_jc = inner.joycons.get(side).and_then(|jc| {
                jc.info().get("colors").and_then(|c| c.get(part)).and_then(|v| v.as_str()).map(String::from)
            });
            from_jc.or_else(|| {
                inner.config.controllers.values().find_map(|v| {
                    if v.get("side").and_then(|s| s.as_str()) == Some(side) {
                        v.get("colors").and_then(|c| c.get(part)).and_then(|v| v.as_str()).map(String::from)
                    } else {
                        None
                    }
                })
            })
        };
        if key == "pro" {
            json!({"body": c("right", "body").or_else(|| c("left", "body")),
                   "buttons": c("right", "buttons").or_else(|| c("left", "buttons")),
                   "grip_left": c("left", "body"), "grip_right": c("right", "body")})
        } else {
            json!({"body": c(key, "body"), "buttons": c(key, "buttons")})
        }
    }

    fn ensure_emulated(&self) {
        let mut inner = self.inner.lock();
        for key in Self::emulated_keys(&inner) {
            inner.retire.remove(&key);
            let colors = Self::colors(&inner, &key);
            if let Some(emu) = inner.emulated.get(&key) {
                emu.set_colors(&colors);
                continue;
            }
            let (kind, mac, serial) = if key == "pro" {
                let any = inner.joycons.get("right").or_else(|| inner.joycons.get("left"));
                let serial = any
                    .and_then(|j| j.info().get("serial").and_then(|v| v.as_str()).map(String::from))
                    .unwrap_or_default();
                ("pro", self.pro_mac(), serial)
            } else {
                let jc = &inner.joycons[&key];
                let serial = jc.info().get("serial").and_then(|v| v.as_str()).map(String::from).unwrap_or_default();
                (if key == "left" { "jcl" } else { "jcr" }, jc.mac.clone(), serial)
            };
            let w = self.me.clone();
            let k = key.clone();
            match SwitchController::new(
                kind,
                &mac,
                &serial,
                &colors,
                Box::new(move |ev| {
                    if let Some(m) = w.upgrade() {
                        m.send(Msg::Host(k.clone(), ev));
                    }
                }),
            ) {
                Ok(emu) => {
                    let name = emu.name;
                    inner.emulated.insert(key.clone(), emu);
                    drop(inner);
                    self.event(&format!("emulated {name} created"), "info");
                    inner = self.inner.lock();
                }
                Err(e) => {
                    drop(inner);
                    self.event(&format!("could not create emulated controller: {e}"), "warn");
                    inner = self.inner.lock();
                }
            }
        }
    }

    fn schedule_retire(&self) {
        {
            let mut inner = self.inner.lock();
            let want = Self::emulated_keys(&inner);
            let keys: Vec<String> = inner.emulated.keys().cloned().collect();
            for key in keys {
                if !want.contains(&key) && !inner.retire.contains_key(&key) {
                    inner.retire.insert(key, now() + GRACE_S);
                }
            }
        }
        let w = self.me.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs_f64(GRACE_S + 0.1));
            if let Some(m) = w.upgrade() {
                m.send(Msg::RetireDue);
            }
        });
    }

    fn retire_due(&self) {
        let mut removed = Vec::new();
        {
            let mut inner = self.inner.lock();
            let t = now();
            let want = Self::emulated_keys(&inner);
            let due: Vec<String> =
                inner.retire.iter().filter(|(k, w)| t >= **w && !want.contains(k)).map(|(k, _)| k.clone()).collect();
            for key in due {
                inner.retire.remove(&key);
                if let Some(emu) = inner.emulated.remove(&key) {
                    removed.push(emu);
                }
            }
        }
        for emu in removed {
            emu.close();
            self.event(&format!("emulated {} removed", emu.name), "info");
        }
    }

    pub fn set_mode(&self, mode: &str) {
        let old: Vec<Arc<SwitchController>> = {
            let mut inner = self.inner.lock();
            if !(mode == "pro" || mode == "joycons") || mode == inner.config.mode {
                return;
            }
            inner.config.mode = mode.into();
            save_config(&inner.config);
            inner.retire.clear();
            inner.emulated.drain().map(|(_, e)| e).collect()
        };
        for e in old {
            e.close();
        }
        self.event(
            if mode == "pro" { "mode: one Pro Controller for both Joy-Cons" } else { "mode: separate Joy-Con devices" },
            "info",
        );
        self.ensure_emulated();
    }

    // --------------------------------------------------------- input routing

    /// Runs on the Joy-Con's socket thread for every report.
    fn on_state(&self, jc: &Arc<JoyCon2>, s: &State) {
        let side = jc.side();
        let mut inner = self.inner.lock();

        // SL+SR held for a second toggles mouse mode on this half.
        let toggled = inner.trackers.entry(side.clone()).or_default().combo(&side, s);
        if toggled {
            let on = !inner.config.mouse.enabled(&side);
            self.set_mouse_locked(&mut inner, &side, on);
            drop(inner);
            self.event(&format!("{side} Joy-Con: mouse mode {}", if on { "on" } else { "off" }), "info");
            self.send(Msg::Feedback(jc.clone(), if on { 3 } else { 2 }));
            inner = self.inner.lock();
        }

        if inner.config.mouse.enabled(&side) {
            if inner.vmouse.is_none() && inner.vmouse_error.is_none() {
                match VirtualMouse::new() {
                    Ok(m) => inner.vmouse = Some(m),
                    Err(e) => {
                        let msg = format!("could not create the virtual mouse: {e} (is /dev/uinput writable?)");
                        inner.vmouse_error = Some(msg.clone());
                        drop(inner);
                        self.event(&msg, "warn");
                        inner = self.inner.lock();
                    }
                }
            }
            let cfg = inner.config.mouse.clone();
            let Inner { trackers, vmouse, .. } = &mut *inner;
            if let (Some(t), Some(m)) = (trackers.get_mut(&side), vmouse.as_mut()) {
                t.feed(&side, s, &cfg, m);
            }
        }
        self.route_gamepad(&mut inner, &side, Some(s));
    }

    /// The state of one half as the gamepad should see it: None while it is
    /// disconnected, not ready, or used as a mouse.
    fn pad_state(inner: &Inner, side: &str, current: Option<(&str, &State)>) -> Option<State> {
        if inner.config.mouse.enabled(side) {
            return None;
        }
        if let Some((cs, st)) = current
            && cs == side
        {
            return Some(st.clone());
        }
        let jc = inner.joycons.get(side)?;
        if !jc.is_ready() {
            return None;
        }
        Some(jc.live.lock().state.clone())
    }

    fn route_gamepad(&self, inner: &mut Inner, side: &str, s: Option<&State>) {
        let current = s.map(|st| (side, st));
        if inner.config.mode == "pro" {
            let Some(emu) = inner.emulated.get("pro").cloned() else { return };
            let ls = Self::pad_state(inner, "left", current);
            let rs = Self::pad_state(inner, "right", current);
            let pref = inner.config.imu_source.clone();
            emu.push_with(|i| {
                i.right = rs.as_ref().map_or(0, |x| x.b4) | ls.as_ref().map_or(0, |x| x.b4);
                i.shared = rs.as_ref().map_or(0, |x| x.b5) | ls.as_ref().map_or(0, |x| x.b5);
                i.left = ls.as_ref().map_or(0, |x| x.b6) | rs.as_ref().map_or(0, |x| x.b6);
                i.lstick = ls.as_ref().map_or((0.0, 0.0), |x| x.stick);
                i.rstick = rs.as_ref().map_or((0.0, 0.0), |x| x.stick);
                let src = if pref == "right" { rs.as_ref().or(ls.as_ref()) } else { ls.as_ref().or(rs.as_ref()) };
                if let Some(src) = src.filter(|x| x.imu_valid) {
                    (i.accel, i.gyro) = to_switch1("pro", src.accel, src.gyro);
                }
                let low = [ls.as_ref(), rs.as_ref()]
                    .into_iter()
                    .flatten()
                    .filter(|x| x.battery_mv > 0)
                    .min_by_key(|x| x.battery_mv);
                if let Some(low) = low {
                    i.battery = switch1::battery_nibble(low.battery_mv, low.charge != 0 && low.charge != 0x20);
                }
            });
        } else {
            let Some(emu) = inner.emulated.get(side).cloned() else { return };
            let st = Self::pad_state(inner, side, current);
            let side = side.to_string();
            emu.push_with(|i| {
                let Some(st) = st else {
                    *i = Inputs::default();
                    return;
                };
                (i.right, i.shared, i.left) = (st.b4, st.b5, st.b6);
                if side == "left" {
                    (i.lstick, i.rstick) = (st.stick, (0.0, 0.0));
                } else {
                    (i.lstick, i.rstick) = ((0.0, 0.0), st.stick);
                }
                if st.imu_valid {
                    (i.accel, i.gyro) = to_switch1(if side == "left" { "jcl" } else { "jcr" }, st.accel, st.gyro);
                }
                if st.battery_mv > 0 {
                    i.battery = switch1::battery_nibble(st.battery_mv, st.charge != 0 && st.charge != 0x20);
                }
            });
        }
    }

    /// Neutral inputs for a half that disconnected.
    fn release(&self, inner: &mut Inner, side: &str) {
        if inner.config.mode == "pro" {
            self.route_gamepad(inner, side, None);
        }
    }

    fn set_mouse_locked(&self, inner: &mut Inner, side: &str, on: bool) {
        inner.config.mouse.set(side, on);
        save_config(&inner.config);
        let Inner { trackers, vmouse, .. } = &mut *inner;
        if let Some(t) = trackers.get_mut(side) {
            match vmouse.as_mut() {
                Some(m) if !on => t.release(m),
                _ => t.reset(),
            }
        }
        if on {
            inner.vmouse_error = None; // try creating it again
        }
        // Clear the half's buttons from the gamepad (or bring them back).
        self.route_gamepad(inner, side, None);
    }

    fn host_rumble(&self, key: &str, block: &str, state: Rumble) {
        let side = if key == "pro" {
            block
        } else if (key == "left") != (block == "left") {
            return; // the other half's block of a single Joy-Con
        } else {
            key
        };
        let inner = self.inner.lock();
        if inner.config.mouse.enabled(side) {
            return;
        }
        if let Some(r) = inner.rumblers.get(side) {
            r.set(state, None);
        }
    }

    fn host_leds(&self, key: &str, on: u8, flash: u8) {
        let sides: Vec<&str> = if key == "pro" { vec!["left", "right"] } else { vec![key] };
        let jcs: Vec<Arc<JoyCon2>> = {
            let inner = self.inner.lock();
            sides.iter().filter_map(|s| inner.joycons.get(*s).cloned()).filter(|j| j.is_ready()).collect()
        };
        for jc in jcs {
            std::thread::spawn(move || {
                let _ = jc.set_leds(on | flash, flash != 0);
            });
        }
    }

    // ------------------------------------------------------- device nodes

    fn refresh_nodes(&self) -> HashMap<String, (String, Value)> {
        let emus: Vec<(String, String)> =
            self.inner.lock().emulated.iter().map(|(k, e)| (k.clone(), e.phys.clone())).collect();
        let mut nodes = HashMap::new();
        let Ok(dir) = std::fs::read_dir("/sys/bus/hid/devices") else { return nodes };
        let devs: Vec<_> = dir.flatten().map(|e| e.path()).collect();
        for (key, phys) in emus {
            let mut info = json!({"hidraw": [], "event": [], "sysfs": null});
            for d in &devs {
                let name = d.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                if !name.starts_with("0005:057E:") {
                    continue;
                }
                let Ok(ue) = std::fs::read_to_string(d.join("uevent")) else { continue };
                if !ue.contains(&format!("HID_PHYS={phys}")) {
                    continue;
                }
                info["sysfs"] = json!(d.to_string_lossy());
                let list = |sub: &Path| -> Vec<String> {
                    std::fs::read_dir(sub)
                        .map(|r| r.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect())
                        .unwrap_or_default()
                };
                let hidraw: Vec<String> = list(&d.join("hidraw")).into_iter().map(|n| format!("/dev/{n}")).collect();
                info["hidraw"] = json!(hidraw);
                let mut events = Vec::new();
                for inp in list(&d.join("input")) {
                    let ip = d.join("input").join(&inp);
                    let nm =
                        std::fs::read_to_string(ip.join("name")).map(|s| s.trim().to_string()).unwrap_or("?".into());
                    for ev in list(&ip).into_iter().filter(|n| n.starts_with("event")) {
                        events.push(json!({"node": format!("/dev/input/{ev}"), "name": nm}));
                    }
                }
                info["event"] = json!(events);
                let mut leds = list(&d.join("leds"));
                leds.sort();
                info["leds"] = json!(leds);
                if let Some(ps) = list(&d.join("power_supply")).into_iter().next() {
                    info["power_supply"] = json!(ps);
                }
            }
            nodes.insert(key, (phys, info));
        }
        nodes
    }

    /// Which processes have the emulated controllers' nodes open.
    pub fn refresh_consumers(&self) {
        let nodes = self.refresh_nodes();
        let mut wanted: HashMap<String, String> = HashMap::new();
        for (key, (_, info)) in &nodes {
            for n in info["hidraw"].as_array().into_iter().flatten() {
                wanted.insert(n.as_str().unwrap_or_default().to_string(), key.clone());
            }
            for e in info["event"].as_array().into_iter().flatten() {
                wanted.insert(e["node"].as_str().unwrap_or_default().to_string(), key.clone());
            }
        }
        let mut found: HashMap<String, BTreeMap<u32, Value>> =
            nodes.keys().map(|k| (k.clone(), BTreeMap::new())).collect();
        if !wanted.is_empty() {
            let me = std::process::id();
            if let Ok(procs) = std::fs::read_dir("/proc") {
                for p in procs.flatten() {
                    let Ok(pid) = p.file_name().to_string_lossy().parse::<u32>() else { continue };
                    if pid == me {
                        continue;
                    }
                    let Ok(fds) = std::fs::read_dir(p.path().join("fd")) else { continue };
                    for fd in fds.flatten() {
                        let Ok(target) = std::fs::read_link(fd.path()) else { continue };
                        let target = target.to_string_lossy().to_string();
                        if let Some(key) = wanted.get(&target) {
                            let comm = std::fs::read_to_string(p.path().join("comm"))
                                .map(|s| s.trim().to_string())
                                .unwrap_or("?".into());
                            let entry = found
                                .get_mut(key)
                                .unwrap()
                                .entry(pid)
                                .or_insert_with(|| json!({"pid": pid, "name": comm, "nodes": []}));
                            entry["nodes"].as_array_mut().unwrap().push(json!(target));
                        }
                    }
                }
            }
        }
        let mut inner = self.inner.lock();
        inner.nodes = nodes.into_iter().map(|(k, (_, v))| (k, v)).collect();
        inner.consumers = found.into_iter().map(|(k, v)| (k, Value::Array(v.into_values().collect()))).collect();
    }

    // -------------------------------------------------------------- status

    pub fn snapshot(&self) -> Value {
        let inner = self.inner.lock();
        let mut joycons = Map::new();
        for (side, jc) in &inner.joycons {
            let rumble = inner.rumblers.get(side).map(|r| r.level()).unwrap_or(0.0);
            let live = jc.live.lock();
            let s = &live.state;
            let mut info = live.info.clone();
            info.remove("ltk");
            let deg = |v: f64| v.to_degrees();
            let tracker = inner.trackers.get(side);
            joycons.insert(
                side.clone(),
                json!({
                    "mac": jc.mac, "ready": jc.is_ready(), "info": info,
                    "buttons": s.pressed(), "stick": [s.stick.0, s.stick.1], "stick_raw": [s.stick_raw.0, s.stick_raw.1],
                    "accel": s.accel, "gyro": [deg(s.gyro[0]), deg(s.gyro[1]), deg(s.gyro[2])],
                    "quat": live.orientation.q, "mouse": s.mouse,
                    "mouse_mode": inner.config.mouse.enabled(side),
                    "mouse_delta": tracker.map(|t| [t.last_delta.0, t.last_delta.1]).unwrap_or([0, 0]),
                    "battery_mv": s.battery_mv, "battery_pct": switch1::battery_percent(s.battery_mv),
                    "charge": s.charge, "rate_hz": live.rate_hz, "interval_ms": live.interval_ms,
                    "jitter_ms": live.jitter_ms, "gaps": live.gaps, "reports": live.reports,
                    "worst_gap_ms": live.worst_gap_ms,
                    "gyro_bias": live.gyro_cal.bias.map(deg),
                    "gyro_cal_age": if live.gyro_cal.calibrated_at > 0.0 { json!(now() - live.gyro_cal.calibrated_at) } else { Value::Null },
                    "rumble": rumble,
                    "raw": live.last_raw.iter().map(|b| format!("{b:02x}")).collect::<String>(),
                }),
            );
        }
        let mut emulated = Map::new();
        for (key, emu) in &inner.emulated {
            let st = emu.st.lock();
            emulated.insert(
                key.clone(),
                json!({
                    "name": emu.name, "kind": emu.kind, "mac": emu.mac, "mode": format!("{:02x}", st.mode),
                    "imu": st.imu_enabled, "vibration": st.vibration_enabled,
                    "player_leds": st.player_leds, "reports": st.reports, "subcommands": st.subcommands,
                    "rumble": {"left": st.rumble_state[0].1 + st.rumble_state[0].3, "right": st.rumble_state[1].1 + st.rumble_state[1].3},
                    "nodes": inner.nodes.get(key).cloned().unwrap_or(json!({})),
                    "consumers": inner.consumers.get(key).cloned().unwrap_or(json!([])),
                    "retiring": inner.retire.contains_key(key),
                }),
            );
        }
        json!({
            "host": self.host, "mode": inner.config.mode, "scanning": inner.scanning,
            "foreign_discovery": inner.foreign_discovery,
            "gyro_autocal": inner.config.gyro_autocal, "imu_source": inner.config.imu_source,
            "mouse": inner.config.mouse, "mouse_error": inner.vmouse_error,
            "joycons": joycons, "emulated": emulated, "conn": inner.conn,
            "seen": inner.seen.values().cloned().collect::<Vec<_>>(),
            "known": inner.config.controllers,
        })
    }

    // ------------------------------------------------------------- actions

    pub fn action(&self, name: &str, args: &Value) -> Value {
        let side = args.get("side").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let jc = self.inner.lock().joycons.get(&side).cloned();
        let f = |k: &str, d: f64| args.get(k).and_then(|v| v.as_f64()).unwrap_or(d);
        let b = |k: &str| args.get(k).and_then(|v| v.as_bool());
        match name {
            "rumble" => {
                let amp = f("amp", 0.5);
                let inner = self.inner.lock();
                let Some(r) = inner.rumblers.get(&side) else {
                    return json!({"ok": false, "error": "no such controller"});
                };
                r.set((320.0, amp, 160.0, amp), Some(f("seconds", 0.4)));
            }
            "sample" if jc.is_some() => {
                let id = f("id", 3.0) as u8;
                let jc = jc.unwrap();
                std::thread::spawn(move || {
                    let _ = jc.play_sample(id);
                });
            }
            "leds" if jc.is_some() => {
                let (mask, flash) = (f("mask", 1.0) as u8, b("flash").unwrap_or(false));
                let jc = jc.unwrap();
                std::thread::spawn(move || {
                    let _ = jc.set_leds(mask, flash);
                });
            }
            "disconnect" if !side.is_empty() => self.disconnect(&side),
            "mode" => self.set_mode(args.get("mode").and_then(|v| v.as_str()).unwrap_or("")),
            "gyro_autocal" => {
                let on = b("enabled").unwrap_or(true);
                let mut inner = self.inner.lock();
                inner.config.gyro_autocal = on;
                save_config(&inner.config);
                for j in inner.joycons.values() {
                    j.live.lock().gyro_cal.enabled = on;
                }
            }
            "imu_source" => {
                let src = args.get("source").and_then(|v| v.as_str()).unwrap_or("");
                if src == "left" || src == "right" {
                    let mut inner = self.inner.lock();
                    inner.config.imu_source = src.into();
                    save_config(&inner.config);
                }
            }
            "reset_orientation" => {
                for j in self.inner.lock().joycons.values() {
                    j.live.lock().orientation.reset();
                }
            }
            "takeover" => {
                if let Some(mac) = args.get("mac").and_then(|v| v.as_str()) {
                    self.inner.lock().takeover.insert(mac.to_uppercase());
                    self.scan_wake.notify_one();
                }
            }
            "connect_now" => self.connect_now(),
            "forget" => {
                if let Some(mac) = args.get("mac").and_then(|v| v.as_str()) {
                    let mut inner = self.inner.lock();
                    inner.config.controllers.remove(&mac.to_uppercase());
                    save_config(&inner.config);
                }
            }
            "mouse_mode" if side == "left" || side == "right" => {
                let on = b("enabled").unwrap_or(false);
                {
                    let mut inner = self.inner.lock();
                    self.set_mouse_locked(&mut inner, &side, on);
                }
                self.event(&format!("{side} Joy-Con: mouse mode {}", if on { "on" } else { "off" }), "info");
            }
            "mouse_settings" => {
                let mut inner = self.inner.lock();
                let m = &mut inner.config.mouse;
                m.sensitivity = f("sensitivity", m.sensitivity).clamp(0.05, 20.0);
                m.scroll_speed = f("scroll_speed", m.scroll_speed).clamp(0.1, 10.0);
                if let Some(v) = b("invert_x") {
                    m.invert_x = v;
                }
                if let Some(v) = b("invert_y") {
                    m.invert_y = v;
                }
                if let Some(v) = b("swap_axes") {
                    m.swap_axes = v;
                }
                save_config(&inner.config);
            }
            _ => return json!({"ok": false, "error": format!("unknown action or controller: {name}")}),
        }
        json!({"ok": true})
    }

    pub fn shutdown(&self) {
        let (rumblers, jcs, emus) = {
            let mut inner = self.inner.lock();
            let Inner { trackers, vmouse, .. } = &mut *inner;
            if let Some(m) = vmouse.as_mut() {
                for t in trackers.values_mut() {
                    t.release(m);
                }
            }
            (
                inner.rumblers.drain().map(|(_, r)| r).collect::<Vec<_>>(),
                inner.joycons.values().cloned().collect::<Vec<_>>(),
                inner.emulated.drain().map(|(_, e)| e).collect::<Vec<_>>(),
            )
        };
        for r in rumblers {
            r.stop();
        }
        for j in jcs {
            j.disconnect();
        }
        for e in emus {
            e.close();
        }
    }
}

/// Switch 2 controller axes -> original-controller axes and LSB units.
///
/// In SDL's common frame a Switch 2 controller reads (x, z, -y) and an
/// original Pro Controller or left Joy-Con reads (-y, z, -x); an original
/// right Joy-Con additionally negates the first two components.
fn to_switch1(kind: &str, a: [f64; 3], g: [f64; 3]) -> ([i16; 3], [i16; 3]) {
    let (a, g) = if kind == "jcr" {
        ([a[1], a[0], -a[2]], [g[1], g[0], -g[2]])
    } else {
        ([a[1], -a[0], a[2]], [g[1], -g[0], g[2]])
    };
    let clamp = |v: f64| v.round().clamp(-32768.0, 32767.0) as i16;
    (a.map(|v| clamp(v * switch1::ACCEL_LSB_PER_G)), g.map(|v| clamp(v * switch1::RAD_TO_LSB)))
}

// --------------------------------------------------------------- bluetooth

async fn adapter_address(session: &bluer::Session) -> bluer::Result<String> {
    let adapter = session.default_adapter().await?;
    Ok(adapter.address().await?.to_string().to_uppercase())
}

/// Scan while a Joy-Con side is missing and nothing is connecting.
async fn scan_loop(mgr: Arc<Manager>, session: bluer::Session) {
    loop {
        let (wanted, connecting) = {
            let inner = mgr.inner.lock();
            (!Manager::wanted_sides(&inner).is_empty(), !inner.connecting.is_empty())
        };
        if !wanted {
            mgr.inner.lock().scanning = "off";
            mgr.scan_wake.notified().await;
            continue;
        }
        if connecting {
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        }
        if let Err(e) = scan_once(&mgr, &session).await {
            mgr.event(&format!("active scan error: {e}"), "warn");
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        mgr.inner.lock().scanning = "off";
    }
}

async fn check_device(mgr: &Arc<Manager>, adapter: &bluer::Adapter, addr: bluer::Address) {
    let Ok(dev) = adapter.device(addr) else { return };
    let Ok(Some(rssi)) = dev.rssi().await else { return }; // not currently advertising
    let Ok(Some(md)) = dev.manufacturer_data().await else { return };
    let Some(data) = md.get(&NINTENDO) else { return };
    let addr_type = match dev.address_type().await {
        Ok(bluer::AddressType::LeRandom) => "random",
        _ => "public",
    };
    mgr.on_advertisement(&addr.to_string(), data, rssi, addr_type);
}

async fn scan_once(mgr: &Arc<Manager>, session: &bluer::Session) -> bluer::Result<()> {
    let adapter = session.default_adapter().await?;
    if !adapter.is_powered().await? {
        return Err(bluer::Error { kind: bluer::ErrorKind::NotReady, message: "the Bluetooth adapter is off".into() });
    }
    adapter
        .set_discovery_filter(bluer::DiscoveryFilter {
            transport: bluer::DiscoveryTransport::Le,
            duplicate_data: true,
            ..Default::default()
        })
        .await?;
    let stream = adapter.discover_devices_with_changes().await?;
    futures::pin_mut!(stream);
    mgr.inner.lock().scanning = "active";
    loop {
        tokio::select! {
            ev = stream.next() => match ev {
                Some(bluer::AdapterEvent::DeviceAdded(addr)) => check_device(mgr, &adapter, addr).await,
                Some(_) => {}
                None => break,
            },
            _ = mgr.scan_wake.notified() => {}
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
        mgr.expire_seen();
        let inner = mgr.inner.lock();
        if Manager::wanted_sides(&inner).is_empty() || !inner.connecting.is_empty() {
            break;
        }
    }
    Ok(())
}

/// Warn while another program runs device discovery: it holds the radio, so
/// new connections can't be created and connected controllers lose reports.
async fn discovery_watch(mgr: Arc<Manager>, session: bluer::Session) {
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        mgr.check_adapter();
        let Ok(adapter) = session.default_adapter().await else {
            mgr.inner.lock().conn.adapter_powered = false;
            continue;
        };
        let powered = adapter.is_powered().await.unwrap_or(false);
        mgr.inner.lock().conn.adapter_powered = powered;
        let Ok(disc) = adapter.is_discovering().await else { continue };
        let on = disc && mgr.inner.lock().scanning != "active";
        let changed = {
            let mut inner = mgr.inner.lock();
            let c = inner.foreign_discovery != on;
            inner.foreign_discovery = on;
            c
        };
        if changed {
            if on {
                mgr.event(
                    "another program is searching for Bluetooth devices (e.g. the desktop's 'add device' dialog); \
                     Joy-Cons can't connect and lose input until it stops",
                    "warn",
                );
            } else {
                mgr.event("Bluetooth device search by another program stopped", "info");
            }
        }
    }
}

pub fn run(verbose: bool) -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(if verbose { "debug" } else { "info" }))
        .format_timestamp_millis()
        .init();
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    rt.block_on(async {
        let session = bluer::Session::new().await?;
        let host = adapter_address(&session).await?;
        let mgr = Manager::new(host);
        *mgr.bt.lock() = Some((tokio::runtime::Handle::current(), session.clone()));
        let server = crate::server::Server::start(mgr.clone())?;
        let scan = tokio::spawn(scan_loop(mgr.clone(), session.clone()));
        let watch = tokio::spawn(discovery_watch(mgr.clone(), session.clone()));
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
        log::info!("stopping");
        scan.abort();
        watch.abort();
        mgr.shutdown();
        server.remove_socket();
        anyhow::Ok(())
    })?;
    // Worker threads may still be blocked in a connection attempt; everything
    // that matters (uhid devices, Bluetooth links) is closed, so don't wait.
    std::process::exit(0);
}
