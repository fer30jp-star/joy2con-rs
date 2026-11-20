// SPDX-License-Identifier: GPL-3.0-or-later
//! User-space HID devices through /dev/uhid.
//!
//! A device created here looks to the kernel like real HID hardware: kernel
//! HID drivers bind to it by bus/vendor/product, and it gets a hidraw node.
//! Input reports are written with UHID_INPUT2; reports the host sends to the
//! device arrive as events on the same file, read by a background thread.
//! Struct layouts follow <linux/uhid.h>; struct uhid_event is packed.

use std::io;
use std::os::fd::RawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use parking_lot::Mutex;

const UHID_DESTROY: u32 = 1;
const UHID_OUTPUT: u32 = 6;
const UHID_GET_REPORT: u32 = 9;
const UHID_GET_REPORT_REPLY: u32 = 10;
const UHID_CREATE2: u32 = 11;
const UHID_INPUT2: u32 = 12;
const UHID_SET_REPORT: u32 = 13;
const UHID_SET_REPORT_REPLY: u32 = 14;
const UHID_OUTPUT_REPORT: u8 = 1;

pub const BUS_BLUETOOTH: u16 = 0x05;
const DATA_MAX: usize = 4096;
const EVENT_SIZE: usize = 4 + 4372; // u32 type + largest union member (uhid_create2_req)

pub type OutputFn = Box<dyn Fn(&[u8]) + Send + Sync>;

struct Inner {
    fd: RawFd,
    write_lock: Mutex<()>,
    closed: AtomicBool,
    wake_w: RawFd,
}

impl Inner {
    fn write(&self, buf: &[u8]) -> io::Result<()> {
        let _g = self.write_lock.lock();
        if self.closed.load(Ordering::SeqCst) {
            return Ok(());
        }
        let n = unsafe { libc::write(self.fd, buf.as_ptr().cast(), buf.len()) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

pub struct UhidDevice {
    inner: Arc<Inner>,
    wake_r: RawFd,
    reader: Mutex<Option<JoinHandle<()>>>,
}

pub struct DeviceSpec<'a> {
    pub name: &'a str,
    pub phys: &'a str,
    pub uniq: &'a str,
    pub bus: u16,
    pub vendor: u32,
    pub product: u32,
    pub version: u32,
    pub descriptor: &'a [u8],
}

fn put_str(buf: &mut [u8], s: &str) {
    let b = s.as_bytes();
    let n = b.len().min(buf.len() - 1);
    buf[..n].copy_from_slice(&b[..n]);
}

impl UhidDevice {
    pub fn create(spec: DeviceSpec, on_output: OutputFn) -> io::Result<UhidDevice> {
        let path = c"/dev/uhid";
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut pipe = [0i32; 2];
        if unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            let e = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(e);
        }
        let inner = Arc::new(Inner { fd, write_lock: Mutex::new(()), closed: AtomicBool::new(false), wake_w: pipe[1] });
        // The reader must run before CREATE2: the kernel driver probes the
        // device and its first subcommands need answers.
        let r = inner.clone();
        let wake_r = pipe[0];
        let reader = std::thread::Builder::new()
            .name(format!("uhid-{}", spec.name))
            .spawn(move || read_loop(r, wake_r, on_output))?;
        let mut ev = vec![0u8; EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_CREATE2.to_le_bytes());
        let b = &mut ev[4..];
        put_str(&mut b[0..128], spec.name);
        put_str(&mut b[128..192], spec.phys);
        put_str(&mut b[192..256], spec.uniq);
        let mut o = 256;
        b[o..o + 2].copy_from_slice(&(spec.descriptor.len() as u16).to_le_bytes());
        o += 2;
        b[o..o + 2].copy_from_slice(&spec.bus.to_le_bytes());
        o += 2;
        for v in [spec.vendor, spec.product, spec.version, 0u32] {
            b[o..o + 4].copy_from_slice(&v.to_le_bytes());
            o += 4;
        }
        b[o..o + spec.descriptor.len()].copy_from_slice(spec.descriptor);
        let dev = UhidDevice { inner, wake_r, reader: Mutex::new(Some(reader)) };
        dev.inner.write(&ev)?;
        Ok(dev)
    }

    /// Deliver one input report (report ID first) to the kernel. Reports
    /// racing a close are dropped.
    pub fn send_input(&self, report: &[u8]) {
        let n = report.len().min(DATA_MAX);
        let mut buf = Vec::with_capacity(6 + n);
        buf.extend_from_slice(&UHID_INPUT2.to_le_bytes());
        buf.extend_from_slice(&(n as u16).to_le_bytes());
        buf.extend_from_slice(&report[..n]);
        if let Err(e) = self.inner.write(&buf)
            && !matches!(e.raw_os_error(), Some(libc::EINVAL) | Some(libc::ENODEV))
        {
            log::debug!("uhid input: {e}");
        }
    }

    pub fn close(&self) {
        {
            let _g = self.inner.write_lock.lock();
            if self.inner.closed.swap(true, Ordering::SeqCst) {
                return;
            }
            let b = UHID_DESTROY.to_le_bytes();
            unsafe { libc::write(self.inner.fd, b.as_ptr().cast(), 4) };
        }
        unsafe { libc::write(self.inner.wake_w, b"x".as_ptr().cast(), 1) };
        if let Some(h) = self.reader.lock().take() {
            let _ = h.join();
        }
        unsafe {
            libc::close(self.inner.fd);
            libc::close(self.wake_r);
            libc::close(self.inner.wake_w);
        }
    }
}

impl Drop for UhidDevice {
    fn drop(&mut self) {
        self.close();
    }
}

fn read_loop(inner: Arc<Inner>, wake_r: RawFd, on_output: OutputFn) {
    let mut ev = vec![0u8; EVENT_SIZE];
    loop {
        let mut fds = [
            libc::pollfd { fd: inner.fd, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: wake_r, events: libc::POLLIN, revents: 0 },
        ];
        let r = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if r < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        if fds[1].revents != 0 {
            return;
        }
        let n = unsafe { libc::read(inner.fd, ev.as_mut_ptr().cast(), EVENT_SIZE) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if matches!(e.kind(), io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock) {
                continue;
            }
            return;
        }
        if n < 4 {
            return;
        }
        let etype = u32::from_le_bytes([ev[0], ev[1], ev[2], ev[3]]);
        let body = &ev[4..];
        match etype {
            UHID_OUTPUT => {
                let size = u16::from_le_bytes([body[DATA_MAX], body[DATA_MAX + 1]]) as usize;
                let rtype = body[DATA_MAX + 2];
                if rtype == UHID_OUTPUT_REPORT {
                    on_output(&body[..size.min(DATA_MAX)]);
                }
            }
            UHID_SET_REPORT => {
                let id = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
                let rtype = body[5];
                let size = u16::from_le_bytes([body[6], body[7]]) as usize;
                let data = &body[8..8 + size.min(DATA_MAX)];
                // Some hosts send output reports as SET_REPORT; treat them the same.
                if rtype == UHID_OUTPUT_REPORT && !data.is_empty() {
                    on_output(data);
                }
                let mut reply = Vec::with_capacity(10);
                reply.extend_from_slice(&UHID_SET_REPORT_REPLY.to_le_bytes());
                reply.extend_from_slice(&id.to_le_bytes());
                reply.extend_from_slice(&0u16.to_le_bytes());
                let _ = inner.write(&reply);
            }
            UHID_GET_REPORT => {
                let id = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
                // No feature or input reports can be fetched on demand.
                let mut reply = Vec::with_capacity(12);
                reply.extend_from_slice(&UHID_GET_REPORT_REPLY.to_le_bytes());
                reply.extend_from_slice(&id.to_le_bytes());
                reply.extend_from_slice(&(libc::EIO as u16).to_le_bytes());
                reply.extend_from_slice(&0u16.to_le_bytes());
                let _ = inner.write(&reply);
            }
            _ => {}
        }
    }
}
