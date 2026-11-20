// SPDX-License-Identifier: GPL-3.0-or-later
//! Minimal Bluetooth LE ATT client over a raw L2CAP socket.
//!
//! The Joy-Con 2 does not finish GATT discovery through bluetoothd, so this
//! talks the ATT protocol directly on the fixed ATT channel (CID 4). A reader
//! thread splits incoming PDUs into notifications (passed to a callback) and
//! responses (handed to the request that is waiting for one).

use std::collections::HashMap;
use std::io;
use std::os::fd::RawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

const AF_BLUETOOTH: i32 = 31;
const BTPROTO_L2CAP: i32 = 0;
const SOL_BLUETOOTH: i32 = 274;
const BT_SECURITY: i32 = 4;
const BT_SECURITY_LOW: u8 = 1;
const ATT_CID: u16 = 4;
pub const BDADDR_LE_PUBLIC: u8 = 1;
pub const BDADDR_LE_RANDOM: u8 = 2;

const OP_ERROR: u8 = 0x01;
const OP_MTU_REQ: u8 = 0x02;
const OP_FIND_INFO_REQ: u8 = 0x04;
const OP_READ_BY_TYPE_REQ: u8 = 0x08;
const OP_READ_REQ: u8 = 0x0A;
const OP_READ_BY_GROUP_REQ: u8 = 0x10;
const OP_WRITE_REQ: u8 = 0x12;
const OP_WRITE_CMD: u8 = 0x52;
const OP_NOTIFY: u8 = 0x1B;
const OP_INDICATE: u8 = 0x1D;
const OP_CONFIRM: u8 = 0x1E;
const ERR_ATTR_NOT_FOUND: u8 = 0x0A;

#[derive(Debug)]
pub enum AttError {
    Att { op: u8, handle: u16, code: u8 },
    Timeout(u8),
    Io(io::Error),
    Closed,
}

impl std::fmt::Display for AttError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AttError::Att { op, handle, code } => {
                write!(f, "ATT error: request {op:#04x} handle {handle:#06x} code {code:#04x}")
            }
            AttError::Timeout(op) => write!(f, "no response to ATT request {op:#04x}"),
            AttError::Io(e) => write!(f, "{e}"),
            AttError::Closed => write!(f, "link closed"),
        }
    }
}

impl std::error::Error for AttError {}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Characteristic {
    pub uuid: String,
    pub props: u8,
    pub value_handle: u16,
    pub cccd: Option<u16>,
    pub descriptors: HashMap<String, u16>,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Service {
    pub uuid: String,
    pub start: u16,
    pub end: u16,
    pub chars: Vec<Characteristic>,
}

#[repr(C)]
struct SockaddrL2 {
    family: u16,
    psm: u16,
    bdaddr: [u8; 6],
    cid: u16,
    bdaddr_type: u8,
}

fn bdaddr(mac: &str) -> [u8; 6] {
    let mut out = [0u8; 6];
    for (i, part) in mac.split(':').rev().take(6).enumerate() {
        out[i] = u8::from_str_radix(part, 16).unwrap_or(0);
    }
    out
}

fn sockaddr(mac: &str, addr_type: u8) -> SockaddrL2 {
    SockaddrL2 {
        family: AF_BLUETOOTH as u16,
        psm: 0,
        bdaddr: bdaddr(mac),
        cid: ATT_CID.to_le(),
        bdaddr_type: addr_type,
    }
}

pub fn uuid_str(raw: &[u8]) -> String {
    if raw.len() == 2 {
        return format!("{:04x}", u16::from_le_bytes([raw[0], raw[1]]));
    }
    let b: String = raw.iter().rev().map(|x| format!("{x:02x}")).collect();
    if b.len() < 32 {
        return b;
    }
    format!("{}-{}-{}-{}-{}", &b[0..8], &b[8..12], &b[12..16], &b[16..20], &b[20..32])
}

type NotifyFn = Box<dyn Fn(u16, &[u8]) + Send + Sync>;
type DisconnectFn = Box<dyn Fn() + Send + Sync>;

struct Shared {
    fd: RawFd,
    resp: Mutex<Option<Vec<u8>>>,
    cv: Condvar,
    req_lock: Mutex<()>,
    closing: AtomicBool,
    closed: AtomicBool,
    notify_errors: AtomicU32,
}

impl Drop for Shared {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

impl Shared {
    fn send(&self, pdu: &[u8]) -> Result<(), AttError> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(AttError::Closed);
        }
        let n = unsafe { libc::send(self.fd, pdu.as_ptr().cast(), pdu.len(), libc::MSG_NOSIGNAL) };
        if n < 0 {
            return Err(AttError::Io(io::Error::last_os_error()));
        }
        Ok(())
    }
}

pub struct AttClient {
    shared: Arc<Shared>,
    pub mtu: u16,
}

impl AttClient {
    /// Connect to `mac` with the given LE address type. `on_notify` receives
    /// every notification/indication (handle, value) on the reader thread;
    /// `on_disconnect` runs once if the link drops without `close()`.
    pub fn connect(
        mac: &str,
        addr_type: u8,
        timeout: Duration,
        on_notify: NotifyFn,
        on_disconnect: DisconnectFn,
    ) -> Result<AttClient, String> {
        let fd = unsafe { libc::socket(AF_BLUETOOTH, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, BTPROTO_L2CAP) };
        if fd < 0 {
            return Err(format!("socket: {}", io::Error::last_os_error()));
        }
        let close_err = |msg: String| {
            unsafe { libc::close(fd) };
            Err(msg)
        };
        let sec = [BT_SECURITY_LOW, 0u8];
        unsafe {
            libc::setsockopt(fd, SOL_BLUETOOTH, BT_SECURITY, sec.as_ptr().cast(), 2);
        }
        // Binding to the any-address lets the kernel route through the default adapter.
        let local = sockaddr("00:00:00:00:00:00", BDADDR_LE_PUBLIC);
        let size = std::mem::size_of::<SockaddrL2>() as libc::socklen_t;
        if unsafe { libc::bind(fd, (&local as *const SockaddrL2).cast(), size) } != 0 {
            return close_err(format!("bind errno {}", io::Error::last_os_error().raw_os_error().unwrap_or(0)));
        }
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        let remote = sockaddr(mac, addr_type);
        if unsafe { libc::connect(fd, (&remote as *const SockaddrL2).cast(), size) } != 0 {
            let e = io::Error::last_os_error().raw_os_error().unwrap_or(0);
            if e != libc::EINPROGRESS && e != libc::EALREADY {
                return close_err(format!("connect errno {e}"));
            }
        }
        let mut pfd = libc::pollfd { fd, events: libc::POLLOUT, revents: 0 };
        let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        if unsafe { libc::poll(&mut pfd, 1, ms) } <= 0 {
            return close_err("timeout".into());
        }
        let mut err: i32 = 0;
        let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
        unsafe {
            libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_ERROR, (&mut err as *mut i32).cast(), &mut len);
        }
        if err != 0 {
            return close_err(format!("SO_ERROR {err}"));
        }
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
        }
        let shared = Arc::new(Shared {
            fd,
            resp: Mutex::new(None),
            cv: Condvar::new(),
            req_lock: Mutex::new(()),
            closing: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            notify_errors: AtomicU32::new(0),
        });
        let reader = shared.clone();
        std::thread::Builder::new()
            .name(format!("att-{mac}"))
            .spawn(move || reader_loop(reader, on_notify, on_disconnect))
            .map_err(|e| e.to_string())?;
        let mut client = AttClient { shared, mtu: 23 };
        if let Ok(rsp) = client.request(&[OP_MTU_REQ, 0x00, 0x02], Duration::from_secs(3))
            && rsp.len() >= 3
        {
            client.mtu = u16::from_le_bytes([rsp[1], rsp[2]]).min(512);
        }
        Ok(client)
    }

    pub fn close(&self) {
        self.shared.closing.store(true, Ordering::SeqCst);
        if !self.shared.closed.swap(true, Ordering::SeqCst) {
            unsafe { libc::shutdown(self.shared.fd, libc::SHUT_RDWR) };
        }
        self.shared.cv.notify_all();
    }

    pub fn request(&self, pdu: &[u8], timeout: Duration) -> Result<Vec<u8>, AttError> {
        let _guard = self.shared.req_lock.lock();
        let mut slot = self.shared.resp.lock();
        *slot = None;
        self.shared.send(pdu)?;
        let deadline = Instant::now() + timeout;
        while slot.is_none() {
            if self.shared.closed.load(Ordering::Relaxed) {
                return Err(AttError::Closed);
            }
            if self.shared.cv.wait_until(&mut slot, deadline).timed_out() {
                break;
            }
        }
        let rsp = slot.take().ok_or(AttError::Timeout(pdu[0]))?;
        if rsp[0] == OP_ERROR && rsp.len() >= 5 {
            return Err(AttError::Att { op: rsp[1], handle: u16::from_le_bytes([rsp[2], rsp[3]]), code: rsp[4] });
        }
        Ok(rsp)
    }

    #[allow(dead_code)]
    pub fn read(&self, handle: u16) -> Result<Vec<u8>, AttError> {
        let h = handle.to_le_bytes();
        Ok(self.request(&[OP_READ_REQ, h[0], h[1]], Duration::from_secs(3))?[1..].to_vec())
    }

    /// Write with response (ATT Write Request).
    pub fn write(&self, handle: u16, value: &[u8]) -> Result<(), AttError> {
        let mut pdu = vec![OP_WRITE_REQ];
        pdu.extend_from_slice(&handle.to_le_bytes());
        pdu.extend_from_slice(value);
        self.request(&pdu, Duration::from_secs(3)).map(|_| ())
    }

    /// Write without response (ATT Write Command).
    pub fn write_cmd(&self, handle: u16, value: &[u8]) -> Result<(), AttError> {
        let mut pdu = Vec::with_capacity(3 + value.len());
        pdu.push(OP_WRITE_CMD);
        pdu.extend_from_slice(&handle.to_le_bytes());
        pdu.extend_from_slice(value);
        self.shared.send(&pdu)
    }

    pub fn enable_notify(&self, cccd: u16) -> Result<(), AttError> {
        self.write(cccd, &[0x01, 0x00])
    }

    /// Raw entries of a Read By Group/Type or Find Information walk.
    fn iterate(&self, op: u8, mut start: u16, end: u16, type16: Option<u16>) -> Result<Vec<Vec<u8>>, AttError> {
        let mut out = Vec::new();
        while start <= end {
            let mut pdu = vec![op];
            pdu.extend_from_slice(&start.to_le_bytes());
            pdu.extend_from_slice(&end.to_le_bytes());
            if let Some(t) = type16 {
                pdu.extend_from_slice(&t.to_le_bytes());
            }
            let rsp = match self.request(&pdu, Duration::from_secs(3)) {
                Ok(r) => r,
                Err(AttError::Att { code: ERR_ATTR_NOT_FOUND, .. }) => return Ok(out),
                Err(e) => return Err(e),
            };
            if rsp.len() < 2 {
                return Ok(out);
            }
            let size = if op == OP_FIND_INFO_REQ { if rsp[1] == 1 { 4 } else { 18 } } else { rsp[1] as usize };
            if size == 0 {
                return Ok(out);
            }
            let data = &rsp[2..];
            let mut last = start;
            let off = if op == OP_READ_BY_GROUP_REQ { 2 } else { 0 };
            let mut i = 0;
            while i + size <= data.len() {
                let entry = &data[i..i + size];
                last = u16::from_le_bytes([entry[off], entry[off + 1]]);
                out.push(entry.to_vec());
                i += size;
            }
            if last >= end || last == u16::MAX {
                return Ok(out);
            }
            start = last + 1;
        }
        Ok(out)
    }

    pub fn discover(&self) -> Result<Vec<Service>, AttError> {
        let mut services = Vec::new();
        for e in self.iterate(OP_READ_BY_GROUP_REQ, 1, 0xFFFF, Some(0x2800))? {
            if e.len() < 6 {
                continue;
            }
            services.push(Service {
                uuid: uuid_str(&e[4..]),
                start: u16::from_le_bytes([e[0], e[1]]),
                end: u16::from_le_bytes([e[2], e[3]]),
                chars: Vec::new(),
            });
        }
        for svc in &mut services {
            for e in self.iterate(OP_READ_BY_TYPE_REQ, svc.start, svc.end, Some(0x2803))? {
                if e.len() < 7 {
                    continue;
                }
                svc.chars.push(Characteristic {
                    uuid: uuid_str(&e[5..]),
                    props: e[2],
                    value_handle: u16::from_le_bytes([e[3], e[4]]),
                    cccd: None,
                    descriptors: HashMap::new(),
                });
            }
            let n = svc.chars.len();
            for i in 0..n {
                let vh = svc.chars[i].value_handle;
                let stop = if i + 1 < n { svc.chars[i + 1].value_handle.saturating_sub(2) } else { svc.end };
                if stop <= vh {
                    continue;
                }
                for e in self.iterate(OP_FIND_INFO_REQ, vh + 1, stop, None)? {
                    let handle = u16::from_le_bytes([e[0], e[1]]);
                    let uuid = uuid_str(&e[2..]);
                    if uuid == "2902" {
                        svc.chars[i].cccd = Some(handle);
                    }
                    svc.chars[i].descriptors.insert(uuid, handle);
                }
            }
        }
        Ok(services)
    }
}

impl Drop for AttClient {
    fn drop(&mut self) {
        self.close();
    }
}

fn reader_loop(shared: Arc<Shared>, on_notify: NotifyFn, on_disconnect: DisconnectFn) {
    let mut buf = [0u8; 1024];
    loop {
        let n = unsafe { libc::recv(shared.fd, buf.as_mut_ptr().cast(), buf.len(), 0) };
        if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if n <= 0 {
            break;
        }
        let pdu = &buf[..n as usize];
        let op = pdu[0];
        if op == OP_NOTIFY || op == OP_INDICATE {
            if op == OP_INDICATE {
                let _ = shared.send(&[OP_CONFIRM]);
            }
            if pdu.len() >= 3 {
                let handle = u16::from_le_bytes([pdu[1], pdu[2]]);
                // A failure while handling one notification must not end the link.
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| on_notify(handle, &pdu[3..])));
                if r.is_err() {
                    let k = shared.notify_errors.fetch_add(1, Ordering::Relaxed) + 1;
                    if k <= 5 || k.is_multiple_of(1000) {
                        log::error!("error handling notification ({k} so far)");
                    }
                }
            }
        } else {
            *shared.resp.lock() = Some(pdu.to_vec());
            shared.cv.notify_all();
        }
    }
    shared.closed.store(true, Ordering::SeqCst);
    shared.cv.notify_all();
    if !shared.closing.load(Ordering::SeqCst) {
        on_disconnect();
    }
}
