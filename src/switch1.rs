// SPDX-License-Identifier: GPL-3.0-or-later
//! Emulation of an original Switch controller over uhid.
//!
//! The emulated device speaks the original Switch Bluetooth HID protocol: full
//! input reports (0x30), subcommand replies (0x21), simple reports (0x3F), and
//! it answers the subcommands a host sends. Linux's hid-nintendo driver, SDL,
//! Steam and Wine all support these controllers, so the Switch 2 Joy-Cons
//! become usable everywhere with gyro, rumble and LEDs.
//!
//! Sticks are calibrated by us and published with a fixed calibration
//! (centre 2048, reach 1600); motion is bias-corrected, rotated into the
//! original controller's axes and published with the default IMU calibration.

use std::io;
use std::sync::OnceLock;

use parking_lot::Mutex;
use serde_json::Value;

use crate::uhid::{BUS_BLUETOOTH, DeviceSpec, UhidDevice};

pub struct Kind {
    pub pid: u32,
    pub name: &'static str,
    pub ctype: u8,
    pub con_info: u8,
}

pub fn kind(k: &str) -> Kind {
    match k {
        "jcl" => Kind { pid: 0x2006, name: "Joy-Con (L)", ctype: 0x01, con_info: 0x0E },
        "jcr" => Kind { pid: 0x2007, name: "Joy-Con (R)", ctype: 0x02, con_info: 0x0E },
        _ => Kind { pid: 0x2009, name: "Pro Controller", ctype: 0x03, con_info: 0x00 },
    }
}

pub const STICK_CENTER: i32 = 2048;
pub const STICK_REACH: i32 = 1600;
const GYRO_DPS_PER_LSB: f64 = 936.0 / 13371.0; // default gyro calibration of the original controllers
pub const RAD_TO_LSB: f64 = (180.0 / std::f64::consts::PI) / GYRO_DPS_PER_LSB;
pub const ACCEL_LSB_PER_G: f64 = 4096.0;

fn hex(s: &str) -> Vec<u8> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

/// Bluetooth HID report descriptor with the report IDs and sizes of the
/// original controllers.
fn descriptor() -> Vec<u8> {
    hex(concat!(
        "05 01 09 05 a1 01",
        "06 01 ff 15 00 26 ff 00 75 08",
        "85 21 09 21 95 30 81 02",
        "85 30 09 30 95 30 81 02",
        "85 31 09 31 96 69 01 81 02",
        "85 32 09 32 96 69 01 81 02",
        "85 33 09 33 96 69 01 81 02",
        "85 3f 05 09 19 01 29 10 15 00 25 01 75 01 95 10 81 02",
        "05 01 09 39 15 00 25 07 75 04 95 01 81 42",
        "75 04 95 01 81 01",
        "09 30 09 31 09 33 09 34 16 00 00 27 ff ff 00 00 75 10 95 04 81 02",
        "06 01 ff 15 00 26 ff 00 75 08",
        "85 01 09 01 95 30 91 02",
        "85 10 09 10 95 30 91 02",
        "85 11 09 11 95 30 91 02",
        "85 12 09 12 95 30 91 02",
        "c0",
    ))
}

/// Linear amplitude (0..1) for each of the 101 HD-rumble amplitude codes.
fn amplitude() -> &'static [f64; 101] {
    static T: OnceLock<[f64; 101]> = OnceLock::new();
    T.get_or_init(|| {
        let anchors =
            [(1, 514.0f64), (2, 775.0), (15, 7372.0), (16, 7698.0), (31, 14744.0), (32, 15067.0), (100, 65535.0)];
        let mut t = [0.0; 101];
        for w in anchors.windows(2) {
            let ((i0, a0), (i1, a1)) = (w[0], w[1]);
            for i in i0..=i1 {
                let f = (i - i0) as f64 / (i1 - i0) as f64;
                t[i] = (a0.ln() + f * (a1.ln() - a0.ln())).exp() / 65535.0;
            }
        }
        t
    })
}

/// (hi Hz, hi amp, lo Hz, lo amp)
pub type Rumble = (f64, f64, f64, f64);

/// Original-Switch HD rumble block (4 bytes) -> (hi Hz, hi amp, lo Hz, lo amp).
pub fn decode_rumble(b: &[u8]) -> Rumble {
    let hf = b[0] as i32 | ((b[1] as i32 & 0x01) << 8);
    let hi_idx = ((b[1] >> 1) as usize).min(100);
    let lf = (b[2] & 0x7F) as i32;
    let lo_idx = ((((b[3] as i32 - 0x40) << 1) | (b[2] as i32 >> 7)).max(0) as usize).min(100);
    let hi_hz = 10.0 * 2f64.powf((hf as f64 / 4.0 + 96.0) / 32.0);
    let lo_hz = 10.0 * 2f64.powf((lf as f64 + 64.0) / 32.0);
    let a = amplitude();
    (hi_hz, a[hi_idx], lo_hz, a[lo_idx])
}

pub fn pack12(a: i32, b: i32) -> [u8; 3] {
    let (a, b) = ((a & 0xFFF) as u16, (b & 0xFFF) as u16);
    [(a & 0xFF) as u8, ((a >> 8) | ((b & 0x0F) << 4)) as u8, (b >> 4) as u8]
}

fn stick_bytes(x: f64, y: f64) -> [u8; 3] {
    let xi = (STICK_CENTER as f64 + x.clamp(-1.27, 1.27) * STICK_REACH as f64).round() as i32;
    let yi = (STICK_CENTER as f64 + y.clamp(-1.27, 1.27) * STICK_REACH as f64).round() as i32;
    pack12(xi.clamp(0, 4095), yi.clamp(0, 4095))
}

const DISCHARGE: [(i32, i32); 12] = [
    (3300, 0),
    (3450, 5),
    (3600, 10),
    (3680, 20),
    (3730, 30),
    (3770, 40),
    (3810, 50),
    (3860, 60),
    (3920, 70),
    (3990, 80),
    (4070, 90),
    (4150, 100),
];

pub fn battery_percent(mv: u16) -> i32 {
    let mv = mv as i32;
    if mv <= DISCHARGE[0].0 {
        return 0;
    }
    for w in DISCHARGE.windows(2) {
        let ((v0, p0), (v1, p1)) = (w[0], w[1]);
        if mv <= v1 {
            return (p0 as f64 + (p1 - p0) as f64 * (mv - v0) as f64 / (v1 - v0) as f64).round() as i32;
        }
    }
    100
}

/// Battery byte high nibble: level 0/2/4/6/8 plus charging bit.
pub fn battery_nibble(mv: u16, charging: bool) -> u8 {
    let pct = battery_percent(mv);
    let level = if pct > 70 {
        8
    } else if pct > 45 {
        6
    } else if pct > 20 {
        4
    } else if pct > 5 {
        2
    } else {
        0
    };
    level | charging as u8
}

/// What the emulated controller currently reports.
#[derive(Clone, Debug)]
pub struct Inputs {
    pub right: u8,
    pub shared: u8,
    pub left: u8,
    pub lstick: (f64, f64),
    pub rstick: (f64, f64),
    pub accel: [i16; 3],
    pub gyro: [i16; 3],
    pub battery: u8,
}

impl Default for Inputs {
    fn default() -> Self {
        Inputs {
            right: 0,
            shared: 0,
            left: 0,
            lstick: (0.0, 0.0),
            rstick: (0.0, 0.0),
            accel: [0; 3],
            gyro: [0; 3],
            battery: 0x8,
        }
    }
}

/// Things the host asks of the emulated controller.
pub enum HostEvent {
    Rumble { block: &'static str, state: Rumble },
    Leds { on: u8, flash: u8 },
    PowerOff,
}

pub struct EmuState {
    pub inputs: Inputs,
    pub mode: u8,
    pub imu_enabled: bool,
    pub vibration_enabled: bool,
    pub player_leds: u8,
    timer: u8,
    pub rumble_state: [Rumble; 2],
    pub subcommands: u64,
    pub reports: u64,
    spi: Vec<u8>,
}

/// One emulated original-Switch controller.
pub struct SwitchController {
    pub kind: String,
    pub name: &'static str,
    ctype: u8,
    con_info: u8,
    pub mac: String,
    pub phys: String,
    pub st: Mutex<EmuState>,
    dev: OnceLock<UhidDevice>,
    on_host: Box<dyn Fn(HostEvent) + Send + Sync>,
}

fn rgb(h: Option<&str>) -> Option<[u8; 3]> {
    let h = h?.trim_start_matches('#');
    if h.len() != 6 {
        return None;
    }
    let v = hex(h);
    Some([v[0], v[1], v[2]])
}

impl SwitchController {
    pub fn new(
        kind_name: &str,
        mac: &str,
        serial: &str,
        colors: &Value,
        on_host: Box<dyn Fn(HostEvent) + Send + Sync>,
    ) -> io::Result<std::sync::Arc<SwitchController>> {
        let k = kind(kind_name);
        let mac = mac.to_uppercase();
        let phys = format!("joy2con/{kind_name}/{}", mac.to_lowercase());
        let emu = std::sync::Arc::new(SwitchController {
            kind: kind_name.to_string(),
            name: k.name,
            ctype: k.ctype,
            con_info: k.con_info,
            mac: mac.clone(),
            phys: phys.clone(),
            st: Mutex::new(EmuState {
                inputs: Inputs::default(),
                mode: 0x3F,
                imu_enabled: false,
                vibration_enabled: false,
                player_leds: 0,
                timer: 0,
                rumble_state: [(0.0, 0.0, 0.0, 0.0); 2],
                subcommands: 0,
                reports: 0,
                spi: Vec::new(),
            }),
            dev: OnceLock::new(),
            on_host,
        });
        emu.st.lock().spi = emu.build_spi(serial, colors);
        let weak = std::sync::Arc::downgrade(&emu);
        let dev = UhidDevice::create(
            DeviceSpec {
                name: k.name,
                phys: &phys,
                uniq: &mac.to_lowercase(),
                bus: BUS_BLUETOOTH,
                vendor: 0x057E,
                product: k.pid,
                version: 0x0001,
                descriptor: &descriptor(),
            },
            Box::new(move |data| {
                if let Some(e) = weak.upgrade() {
                    e.on_output(data);
                }
            }),
        )?;
        let _ = emu.dev.set(dev);
        log::info!("created emulated {} ({phys})", k.name);
        Ok(emu)
    }

    // ------------------------------------------------------------ SPI flash

    fn build_spi(&self, serial: &str, colors: &Value) -> Vec<u8> {
        let mut spi = vec![0xFFu8; 0x80000];
        let ser = serial.as_bytes();
        let n = ser.len().min(16);
        spi[0x6000..0x6010].fill(0);
        spi[0x6000..0x6000 + n].copy_from_slice(&ser[..n]);
        spi[0x6012] = self.ctype;
        spi[0x6013] = 0xA0;
        spi[0x601B] = if self.kind == "pro" { 0x02 } else { 0x01 };
        // Default IMU calibration: zero offsets, 1 g = 4096, gyro sens 13371.
        let imu: [i16; 12] = [0, 0, 0, 0x4000, 0x4000, 0x4000, 0, 0, 0, 0x343B, 0x343B, 0x343B];
        for (i, v) in imu.iter().enumerate() {
            spi[0x6020 + i * 2..0x6022 + i * 2].copy_from_slice(&v.to_le_bytes());
        }
        let (c, r) = (STICK_CENTER, STICK_REACH);
        // Left stick: [above centre][centre][below centre]; right: [centre][below][above].
        let mut o = 0x603D;
        for p in [pack12(r, r), pack12(c, c), pack12(r, r), pack12(c, c), pack12(r, r), pack12(r, r)] {
            spi[o..o + 3].copy_from_slice(&p);
            o += 3;
        }
        let col = |k: &str| rgb(colors.get(k).and_then(|v| v.as_str()));
        let body = col("body").unwrap_or([0x32, 0x32, 0x32]);
        spi[0x6050..0x6053].copy_from_slice(&body);
        spi[0x6053..0x6056].copy_from_slice(&col("buttons").unwrap_or([0x32, 0x32, 0x32]));
        spi[0x6056..0x6059].copy_from_slice(&col("grip_left").unwrap_or(body));
        spi[0x6059..0x605C].copy_from_slice(&col("grip_right").unwrap_or(body));
        spi[0x6080..0x6086].copy_from_slice(&hex("50fd0000c60f"));
        let params = hex("0f30619630f3d41454411554c7799c333663");
        spi[0x6086..0x6098].copy_from_slice(&params);
        spi[0x6098..0x60AA].copy_from_slice(&params);
        spi
    }

    pub fn set_colors(&self, colors: &Value) {
        let mut st = self.st.lock();
        for (key, off) in [("body", 0x6050), ("buttons", 0x6053), ("grip_left", 0x6056), ("grip_right", 0x6059)] {
            if let Some(c) = rgb(colors.get(key).and_then(|v| v.as_str())) {
                st.spi[off..off + 3].copy_from_slice(&c);
            }
        }
    }

    // ---------------------------------------------------------------- input

    fn header(&self, st: &mut EmuState) -> Vec<u8> {
        st.timer = st.timer.wrapping_add(1);
        let i = &st.inputs;
        let bat = (i.battery << 4) | self.con_info;
        let mut h = vec![st.timer, bat, i.right, i.shared, i.left];
        h.extend_from_slice(&stick_bytes(i.lstick.0, i.lstick.1));
        h.extend_from_slice(&stick_bytes(i.rstick.0, i.rstick.1));
        h.push(0x09);
        h
    }

    /// Update the inputs and send them as one report in the active mode.
    pub fn push_with(&self, f: impl FnOnce(&mut Inputs)) {
        let report = {
            let mut st = self.st.lock();
            f(&mut st.inputs);
            let report = if st.mode == 0x3F {
                simple_report(&st.inputs)
            } else {
                let sample: Vec<u8> = if st.imu_enabled {
                    let i = &st.inputs;
                    i.accel.iter().chain(i.gyro.iter()).flat_map(|v| v.to_le_bytes()).collect()
                } else {
                    vec![0; 12]
                };
                let mut r = vec![0x30];
                r.extend(self.header(&mut st));
                // Three samples per report; each carries the newest reading.
                for _ in 0..3 {
                    r.extend_from_slice(&sample);
                }
                if matches!(st.mode, 0x31..=0x33) {
                    r[0] = st.mode;
                    r.resize(362, 0);
                }
                r
            };
            st.reports += 1;
            report
        };
        if let Some(d) = self.dev.get() {
            d.send_input(&report);
        }
    }

    // --------------------------------------------------------------- output

    fn on_output(&self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let rid = data[0];
        if (rid == 0x01 || rid == 0x10) && data.len() >= 10 {
            self.rumble(&data[2..10]);
        }
        if rid == 0x01 && data.len() >= 11 {
            self.subcommand(data[10], &data[11..]);
        }
    }

    fn rumble(&self, block: &[u8]) {
        for (idx, side) in [(0usize, "left"), (1, "right")] {
            let state = decode_rumble(&block[idx * 4..idx * 4 + 4]);
            let changed = {
                let mut st = self.st.lock();
                if st.rumble_state[idx] != state {
                    st.rumble_state[idx] = state;
                    true
                } else {
                    false
                }
            };
            if changed {
                (self.on_host)(HostEvent::Rumble { block: side, state });
            }
        }
    }

    fn reply(&self, ack: u8, sub: u8, data: &[u8]) {
        let mut report = {
            let mut st = self.st.lock();
            let mut r = vec![0x21];
            r.extend(self.header(&mut st));
            r
        };
        report.push(ack);
        report.push(sub);
        report.extend_from_slice(data);
        report.resize(49, 0);
        if let Some(d) = self.dev.get() {
            d.send_input(&report);
        }
    }

    fn subcommand(&self, sub: u8, args: &[u8]) {
        self.st.lock().subcommands += 1;
        let a0 = args.first().copied().unwrap_or(0);
        let addr_size = || {
            let mut a = [0u8; 5];
            for (i, b) in args.iter().take(5).enumerate() {
                a[i] = *b;
            }
            (u32::from_le_bytes([a[0], a[1], a[2], a[3]]) as usize, a[4] as usize)
        };
        match sub {
            0x02 => {
                // device info
                let mac: Vec<u8> = self.mac.split(':').map(|x| u8::from_str_radix(x, 16).unwrap_or(0)).collect();
                let mut d = vec![0x04, 0x21, self.ctype, 0x02];
                d.extend(mac);
                d.extend_from_slice(&[0x01, 0x01]);
                self.reply(0x82, sub, &d);
            }
            0x03 => {
                self.st.lock().mode = a0;
                log::debug!("{}: report mode {a0:02x}", self.name);
                self.reply(0x80, sub, &[]);
            }
            0x04 => self.reply(0x83, sub, &[0; 14]),
            0x10 => {
                let (addr, size) = addr_size();
                let size = size.min(0x1D);
                let chunk = {
                    let st = self.st.lock();
                    if addr + size <= st.spi.len() { st.spi[addr..addr + size].to_vec() } else { vec![0; size] }
                };
                let mut d = (addr as u32).to_le_bytes().to_vec();
                d.push(size as u8);
                d.extend(chunk);
                self.reply(0x90, sub, &d);
            }
            0x11 => {
                let (addr, size) = addr_size();
                if (0x8000..0x9000).contains(&addr) && args.len() >= 5 + size {
                    self.st.lock().spi[addr..addr + size].copy_from_slice(&args[5..5 + size]);
                }
                self.reply(0x80, sub, &[0]);
            }
            0x12 => {
                let (addr, _) = addr_size();
                if (0x8000..0x9000).contains(&addr) {
                    let base = addr & !0xFFF;
                    self.st.lock().spi[base..base + 0x1000].fill(0xFF);
                }
                self.reply(0x80, sub, &[0]);
            }
            0x21 => {
                let mut d = vec![0x01, 0x00, 0xFF, 0x00, 0x08, 0x00, 0x1B, 0x01];
                d.resize(34, 0);
                self.reply(0xA0, sub, &d);
            }
            0x30 => {
                self.st.lock().player_leds = a0;
                (self.on_host)(HostEvent::Leds { on: a0 & 0x0F, flash: a0 >> 4 });
                self.reply(0x80, sub, &[]);
            }
            0x31 => {
                let l = self.st.lock().player_leds;
                self.reply(0xB0, sub, &[l]);
            }
            0x40 => {
                self.st.lock().imu_enabled = a0 != 0;
                self.reply(0x80, sub, &[]);
            }
            0x43 => {
                let count = args.get(1).copied().unwrap_or(1) as usize;
                let mut d = vec![a0, count as u8];
                d.resize(2 + count, 0);
                self.reply(0xC0, sub, &d);
            }
            0x48 => {
                self.st.lock().vibration_enabled = a0 != 0;
                self.reply(0x80, sub, &[]);
            }
            0x50 => self.reply(0xD0, sub, &0x0618u16.to_le_bytes()),
            0x06 if a0 == 0 => {
                self.reply(0x80, sub, &[]);
                (self.on_host)(HostEvent::PowerOff);
            }
            _ => self.reply(0x80, sub, &[]),
        }
    }

    pub fn close(&self) {
        if let Some(d) = self.dev.get() {
            d.close();
        }
        log::info!("removed emulated {}", self.name);
    }
}

fn simple_report(i: &Inputs) -> Vec<u8> {
    // Simple-report button order: B A Y X L R ZL ZR - + LS RS Home Capture
    let bits = [
        (i.right, 0x04),
        (i.right, 0x08),
        (i.right, 0x01),
        (i.right, 0x02),
        (i.left, 0x40),
        (i.right, 0x40),
        (i.left, 0x80),
        (i.right, 0x80),
        (i.shared, 0x01),
        (i.shared, 0x02),
        (i.shared, 0x08),
        (i.shared, 0x04),
        (i.shared, 0x10),
        (i.shared, 0x20),
    ];
    let mut b: u16 = 0;
    for (n, (byte, mask)) in bits.iter().enumerate() {
        if byte & mask != 0 {
            b |= 1 << n;
        }
    }
    let (up, down) = (i.left & 0x02 != 0, i.left & 0x01 != 0);
    let (right, left) = (i.left & 0x04 != 0, i.left & 0x08 != 0);
    let hat = match (up, right, down, left) {
        (true, false, false, false) => 0,
        (true, true, false, false) => 1,
        (false, true, false, false) => 2,
        (false, true, true, false) => 3,
        (false, false, true, false) => 4,
        (false, false, true, true) => 5,
        (false, false, false, true) => 6,
        (true, false, false, true) => 7,
        _ => 8,
    };
    let ax = |v: f64, inv: bool| -> u16 {
        let v = if inv { -v } else { v };
        (0x8000 as f64 + v * 0x7FFF as f64).clamp(0.0, 65535.0) as u16
    };
    let mut r = vec![0x3F];
    r.extend_from_slice(&b.to_le_bytes());
    r.push(hat);
    for v in [ax(i.lstick.0, false), ax(i.lstick.1, true), ax(i.rstick.0, false), ax(i.rstick.1, true)] {
        r.extend_from_slice(&v.to_le_bytes());
    }
    r
}
