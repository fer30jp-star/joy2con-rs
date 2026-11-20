// SPDX-License-Identifier: GPL-3.0-or-later
//! Switch 2 Joy-Con protocol over Bluetooth LE.
//!
//! `JoyCon2` owns one controller: it connects over a raw ATT socket, runs the
//! setup the console performs (pairing, player LEDs, feature flags), reads the
//! factory calibration from the controller's flash, and turns every input
//! report into a calibrated `State`. Reports are handled on the socket's reader
//! thread and handed to `on_state` immediately.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Weak};
use std::time::Duration;

use aes::cipher::{BlockCipherEncrypt, KeyInit};
use parking_lot::{Mutex, RwLock};
use serde_json::{Map, Value, json};

use crate::att::{AttClient, BDADDR_LE_PUBLIC, BDADDR_LE_RANDOM, Characteristic};
use crate::util::now;

pub const PID_LEFT: u16 = 0x2067;
pub const PID_RIGHT: u16 = 0x2066;
pub const PID_PRO: u16 = 0x2069;

pub fn side_by_pid(pid: u16) -> Option<&'static str> {
    match pid {
        PID_LEFT => Some("left"),
        PID_RIGHT => Some("right"),
        PID_PRO => Some("pro"),
        _ => None,
    }
}

const UUID_INPUT: &str = "ab7de9be-89fe-49ad-828f-118f09df7fd2"; // input report 0x05
const UUID_COMMAND: &str = "649d4ac9-8eb7-4e6c-af44-1ea54fe5f005"; // command requests
const UUID_RESPONSE: &str = "c765a961-d9d8-4d36-a20a-5315b111836a"; // command responses
const UUID_UNKNOWN_W: &str = "00c5af5d-1964-4e30-8f51-1956f96bd282"; // written 01 00 by the console first
const UUID_RATE_DESC: &str = "679d5510-5a24-4dee-9557-95df80486ecb"; // written 85 00 by the console

fn uuid_rumble(side: &str) -> &'static str {
    match side {
        "left" => "289326cb-a471-485d-a8f4-240c14f18241",
        "right" => "fa19b0fb-cd1f-46a7-84a1-bbb09e00c149",
        _ => "cc483f51-9258-427d-a939-630c31f72b05",
    }
}

/// Feature flags: buttons, sticks, IMU, mouse (0x10), rumble. The console uses 0x37 for Joy-Cons.
const FEATURES: u8 = 0x37;

/// The controller's half of the pairing key exchange is fixed (B1). The
/// long-term key is A1 ^ B1, so choosing A1 = wanted ^ B1 makes the key whatever we want.
const PAIRING_B1: [u8; 16] =
    [0x5c, 0xf6, 0xee, 0x79, 0x2c, 0xdf, 0x05, 0xe1, 0xba, 0x2b, 0x63, 0x25, 0xc4, 0x1a, 0x5f, 0x10];

/// Report 0x05 button bits: name, byte offset, mask. Bytes 4-6 use the same
/// layout as the original Switch's full report.
pub const BUTTONS: &[(&str, usize, u8)] = &[
    ("Y", 4, 0x01),
    ("X", 4, 0x02),
    ("B", 4, 0x04),
    ("A", 4, 0x08),
    ("SR_R", 4, 0x10),
    ("SL_R", 4, 0x20),
    ("R", 4, 0x40),
    ("ZR", 4, 0x80),
    ("MINUS", 5, 0x01),
    ("PLUS", 5, 0x02),
    ("RSTICK", 5, 0x04),
    ("LSTICK", 5, 0x08),
    ("HOME", 5, 0x10),
    ("CAPTURE", 5, 0x20),
    ("C", 5, 0x40),
    ("DOWN", 6, 0x01),
    ("UP", 6, 0x02),
    ("RIGHT", 6, 0x04),
    ("LEFT", 6, 0x08),
    ("SR_L", 6, 0x10),
    ("SL_L", 6, 0x20),
    ("L", 6, 0x40),
    ("ZL", 6, 0x80),
];

const ACCEL_LSB_PER_G: f64 = 4096.0;

fn u12_pair(b: &[u8]) -> (u16, u16) {
    (b[0] as u16 | ((b[1] as u16 & 0x0F) << 8), (b[1] as u16 >> 4) | ((b[2] as u16) << 4))
}

#[derive(Clone, Copy, Debug)]
pub struct AxisCal {
    pub neutral: i32,
    pub max: i32,
    pub min: i32,
}

impl Default for AxisCal {
    fn default() -> Self {
        AxisCal { neutral: 2048, max: 1500, min: 1500 }
    }
}

impl AxisCal {
    fn normalize(&self, raw: u16) -> f64 {
        let d = raw as i32 - self.neutral;
        d as f64 / (if d >= 0 { self.max } else { self.min }) as f64
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct StickCal {
    pub x: AxisCal,
    pub y: AxisCal,
}

impl StickCal {
    fn parse(b: &[u8]) -> Option<StickCal> {
        if b.len() < 9 {
            return None;
        }
        let (nx, ny) = u12_pair(&b[0..3]);
        let (xmax, ymax) = u12_pair(&b[3..6]);
        let (xmin, ymin) = u12_pair(&b[6..9]);
        if [nx, ny, xmax, ymax, xmin, ymin].contains(&0) || [nx, ny, xmax].contains(&0xFFF) {
            return None;
        }
        Some(StickCal {
            x: AxisCal { neutral: nx as i32, max: xmax as i32, min: xmin as i32 },
            y: AxisCal { neutral: ny as i32, max: ymax as i32, min: ymin as i32 },
        })
    }
}

/// One decoded input report. Sticks are -1..1 (calibrated, Y up), gyro in
/// rad/s and accel in g, both bias-corrected and in the controller's own frame.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct State {
    pub t: f64,
    pub counter: u32,
    pub b4: u8,
    pub b5: u8,
    pub b6: u8,
    pub b7: u8,
    pub stick: (f64, f64),
    pub stick_raw: (u16, u16),
    pub accel: [f64; 3],
    pub gyro: [f64; 3],
    pub imu_ts: u32,
    pub imu_valid: bool,
    /// Optical sensor: X and Y are free-running 16-bit counters; the other two are not understood yet.
    pub mouse: [u16; 4],
    pub battery_mv: u16,
    pub charge: u8,
}

impl Default for State {
    fn default() -> Self {
        State {
            t: 0.0,
            counter: 0,
            b4: 0,
            b5: 0,
            b6: 0,
            b7: 0,
            stick: (0.0, 0.0),
            stick_raw: (2048, 2048),
            accel: [0.0, 0.0, 1.0],
            gyro: [0.0; 3],
            imu_ts: 0,
            imu_valid: false,
            mouse: [0; 4],
            battery_mv: 0,
            charge: 0,
        }
    }
}

impl State {
    pub fn pressed(&self) -> Vec<&'static str> {
        BUTTONS
            .iter()
            .filter(|(_, o, m)| {
                let b = match o {
                    4 => self.b4,
                    5 => self.b5,
                    _ => self.b6,
                };
                b & m != 0
            })
            .map(|(n, _, _)| *n)
            .collect()
    }

    pub fn is(&self, name: &str) -> bool {
        BUTTONS.iter().any(|(n, o, m)| {
            *n == name
                && (match o {
                    4 => self.b4,
                    5 => self.b5,
                    _ => self.b6,
                }) & m
                    != 0
        })
    }
}

/// Refines the gyro bias while the controller rests: when gyro and
/// accelerometer have both been steady for WINDOW seconds, the mean gyro
/// reading over that window is the remaining bias.
pub struct GyroCalibrator {
    pub bias: [f64; 3],
    pub enabled: bool,
    pub calibrated_at: f64,
    n: u32,
    start: f64,
    sum: [f64; 3],
    sq: [f64; 3],
    asum: [f64; 3],
    asq: [f64; 3],
}

impl GyroCalibrator {
    const WINDOW: f64 = 1.2;
    const MAX_RATE: f64 = 2.5 * std::f64::consts::PI / 180.0;
    const MAX_SPREAD: f64 = 0.6 * std::f64::consts::PI / 180.0;
    const MAX_ACCEL_SPREAD: f64 = 0.012;

    fn new() -> Self {
        GyroCalibrator {
            bias: [0.0; 3],
            enabled: true,
            calibrated_at: 0.0,
            n: 0,
            start: 0.0,
            sum: [0.0; 3],
            sq: [0.0; 3],
            asum: [0.0; 3],
            asq: [0.0; 3],
        }
    }

    fn reset(&mut self) {
        self.n = 0;
        self.start = 0.0;
        self.sum = [0.0; 3];
        self.sq = [0.0; 3];
        self.asum = [0.0; 3];
        self.asq = [0.0; 3];
    }

    /// g: gyro (rad/s) corrected by the factory bias only.
    fn feed(&mut self, t: f64, g: [f64; 3], a: [f64; 3]) {
        if !self.enabled {
            return;
        }
        if (0..3).any(|i| (g[i] - self.bias[i]).abs() > 4.0 * Self::MAX_RATE) {
            self.reset();
            return;
        }
        if self.n == 0 {
            self.start = t;
        }
        self.n += 1;
        for i in 0..3 {
            self.sum[i] += g[i];
            self.sq[i] += g[i] * g[i];
            self.asum[i] += a[i];
            self.asq[i] += a[i] * a[i];
        }
        if t - self.start < Self::WINDOW || self.n < 40 {
            return;
        }
        let n = self.n as f64;
        let mean: Vec<f64> = self.sum.iter().map(|s| s / n).collect();
        let spread_ok = (0..3).all(|i| (self.sq[i] / n - mean[i] * mean[i]).max(0.0).sqrt() < Self::MAX_SPREAD);
        let aspread_ok = (0..3).all(|i| {
            let m = self.asum[i] / n;
            (self.asq[i] / n - m * m).max(0.0).sqrt() < Self::MAX_ACCEL_SPREAD
        });
        if mean.iter().all(|m| m.abs() < Self::MAX_RATE) && spread_ok && aspread_ok {
            self.bias = [mean[0], mean[1], mean[2]];
            self.calibrated_at = t;
        }
        self.reset();
    }
}

/// Mahony filter: gyro integration corrected towards gravity. For display only.
pub struct Orientation {
    pub q: [f64; 4],
    t: Option<f64>,
}

impl Orientation {
    fn new() -> Self {
        Orientation { q: [1.0, 0.0, 0.0, 0.0], t: None }
    }

    fn update(&mut self, t: f64, g: [f64; 3], a: [f64; 3]) {
        let Some(prev) = self.t else {
            self.t = Some(t);
            return;
        };
        let dt = (t - prev).clamp(0.0, 0.05);
        self.t = Some(t);
        let [q0, q1, q2, q3] = self.q;
        let [mut gx, mut gy, mut gz] = g;
        let an = (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt();
        if an > 0.6 && an < 1.4 {
            let (ax, ay, az) = (a[0] / an, a[1] / an, a[2] / an);
            let vx = 2.0 * (q1 * q3 - q0 * q2);
            let vy = 2.0 * (q0 * q1 + q2 * q3);
            let vz = q0 * q0 - q1 * q1 - q2 * q2 + q3 * q3;
            let kp = 1.5;
            gx += kp * (ay * vz - az * vy);
            gy += kp * (az * vx - ax * vz);
            gz += kp * (ax * vy - ay * vx);
        }
        let h = 0.5 * dt;
        let n0 = q0 + (-q1 * gx - q2 * gy - q3 * gz) * h;
        let n1 = q1 + (q0 * gx + q2 * gz - q3 * gy) * h;
        let n2 = q2 + (q0 * gy - q1 * gz + q3 * gx) * h;
        let n3 = q3 + (q0 * gz + q1 * gy - q2 * gx) * h;
        let n = (n0 * n0 + n1 * n1 + n2 * n2 + n3 * n3).sqrt();
        self.q = [n0 / n, n1 / n, n2 / n, n3 / n];
    }

    pub fn reset(&mut self) {
        self.q = [1.0, 0.0, 0.0, 0.0];
    }
}

/// Everything that changes with each report, behind one lock.
pub struct Live {
    pub state: State,
    pub cal: StickCal,
    pub gyro_bias_factory: [f64; 3],
    pub gyro_scale: f64,
    pub gyro_cal: GyroCalibrator,
    pub orientation: Orientation,
    pub info: Map<String, Value>,
    pub reports: u64,
    pub rate_hz: f64,
    pub interval_ms: f64,
    pub jitter_ms: f64,
    pub gaps: u64,
    pub worst_gap_ms: f64,
    pub last_raw: Vec<u8>,
    gap_window: VecDeque<f64>,
    last_t: f64,
    imu_base: Option<(f64, u32)>,
    imu_checked: bool,
}

pub type StateFn = Box<dyn Fn(&Arc<JoyCon2>, &State) + Send + Sync>;
pub type LostFn = Box<dyn Fn(&Arc<JoyCon2>) + Send + Sync>;

/// One connected Switch 2 controller.
pub struct JoyCon2 {
    pub mac: String,
    pub address_type: String,
    side: RwLock<String>,
    host_mac: String,
    pairing_key: [u8; 16],
    client: RwLock<Option<Arc<AttClient>>>,
    chars: RwLock<HashMap<String, Characteristic>>,
    pending: Mutex<HashMap<(u8, u8), SyncSender<Vec<u8>>>>,
    pub live: Mutex<Live>,
    pub connected: AtomicBool,
    pub ready: AtomicBool,
    rumble_seq: AtomicU8,
    on_state: StateFn,
    on_disconnect: LostFn,
    me: Weak<JoyCon2>,
}

#[derive(Debug)]
pub struct CommandError(pub String);

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CommandError {}

type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

impl JoyCon2 {
    pub fn new(
        mac: &str,
        side: &str,
        host_mac: &str,
        pairing_key: [u8; 16],
        address_type: &str,
        on_state: StateFn,
        on_disconnect: LostFn,
    ) -> Arc<JoyCon2> {
        Arc::new_cyclic(|me| JoyCon2 {
            mac: mac.to_uppercase(),
            address_type: address_type.to_string(),
            side: RwLock::new(side.to_string()),
            host_mac: host_mac.to_uppercase(),
            pairing_key,
            client: RwLock::new(None),
            chars: RwLock::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            live: Mutex::new(Live {
                state: State::default(),
                cal: StickCal::default(),
                gyro_bias_factory: [0.0; 3],
                gyro_scale: 34.8 / 32767.0,
                gyro_cal: GyroCalibrator::new(),
                orientation: Orientation::new(),
                info: Map::new(),
                reports: 0,
                rate_hz: 0.0,
                interval_ms: 0.0,
                jitter_ms: 0.0,
                gaps: 0,
                worst_gap_ms: 0.0,
                last_raw: Vec::new(),
                gap_window: VecDeque::new(),
                last_t: 0.0,
                imu_base: None,
                imu_checked: false,
            }),
            connected: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            rumble_seq: AtomicU8::new(0),
            on_state,
            on_disconnect,
            me: me.clone(),
        })
    }

    pub fn side(&self) -> String {
        self.side.read().clone()
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }

    pub fn info(&self) -> Map<String, Value> {
        self.live.lock().info.clone()
    }

    fn char_handle(&self, uuid: &str) -> Option<u16> {
        self.chars.read().get(uuid).map(|c| c.value_handle)
    }

    fn client(&self) -> Res<Arc<AttClient>> {
        self.client.read().clone().ok_or_else(|| CommandError("not connected".into()).into())
    }

    // ------------------------------------------------------------------ setup

    /// Connect and initialise. Blocking; run on a worker thread.
    pub fn start(&self) -> Res<()> {
        let t0 = now();
        let kind = if self.address_type == "random" { BDADDR_LE_RANDOM } else { BDADDR_LE_PUBLIC };
        let w1 = self.me.clone();
        let w2 = self.me.clone();
        let client = AttClient::connect(
            &self.mac,
            kind,
            Duration::from_secs(20),
            Box::new(move |h, d| {
                if let Some(me) = w1.upgrade() {
                    me.on_notify(h, d);
                }
            }),
            Box::new(move || {
                if let Some(me) = w2.upgrade() {
                    me.on_link_lost();
                }
            }),
        )
        .map_err(|e| CommandError(format!("{}: {e}", self.address_type)))?;
        let client = Arc::new(client);
        *self.client.write() = Some(client.clone());
        self.connected.store(true, Ordering::SeqCst);
        {
            let mut chars = self.chars.write();
            for svc in client.discover()? {
                for ch in svc.chars {
                    chars.insert(ch.uuid.clone(), ch);
                }
            }
            for uuid in [UUID_INPUT, UUID_COMMAND, UUID_RESPONSE] {
                if !chars.contains_key(uuid) {
                    return Err(
                        CommandError(format!("characteristic {uuid} missing; not a Switch 2 controller?")).into()
                    );
                }
            }
        }
        log::info!(
            "{} {}: connected, MTU {}, discovery {:.0} ms",
            self.side(),
            self.mac,
            client.mtu,
            (now() - t0) * 1000.0
        );

        if let Some(h) = self.char_handle(UUID_UNKNOWN_W)
            && let Err(e) = client.write(h, &[0x01, 0x00])
        {
            log::debug!("write 0x0005 refused: {e}");
        }
        let resp_cccd = self.chars.read()[UUID_RESPONSE].cccd;
        if let Some(c) = resp_cccd {
            client.enable_notify(c)?;
        }
        self.read_identity()?;
        if let Err(e) = self.ensure_paired() {
            log::warn!(
                "{}: pairing failed ({e}); this session still works, but the Joy-Con will need its sync button next time",
                self.side()
            );
        }
        self.command(0x0C, 0x02, &[FEATURES, 0, 0, 0])?;
        self.command(0x0C, 0x04, &[FEATURES, 0, 0, 0])?;
        let (rate_desc, input_cccd) = {
            let chars = self.chars.read();
            let c = &chars[UUID_INPUT];
            (c.descriptors.get(UUID_RATE_DESC).copied(), c.cccd)
        };
        if let Some(d) = rate_desc
            && let Err(e) = client.write(d, &[0x85, 0x00])
        {
            log::debug!("report-rate descriptor write refused: {e}");
        }
        if let Some(c) = input_cccd {
            client.enable_notify(c)?;
        }
        self.ready.store(true, Ordering::SeqCst);
        log::info!("{} {}: ready in {:.0} ms", self.side(), self.mac, (now() - t0) * 1000.0);
        Ok(())
    }

    fn read_identity(&self) -> Res<()> {
        let fw = self.command(0x10, 0x01, &[])?;
        let firmware = if fw.len() >= 3 { format!("{}.{}.{}", fw[0], fw[1], fw[2]) } else { "?".into() };
        let factory = self.read_flash(0x13000, 0x40)?;
        let serial_raw = &factory[2..0x12];
        let end = serial_raw.iter().position(|&b| b == 0).unwrap_or(serial_raw.len());
        let serial = String::from_utf8_lossy(&serial_raw[..end]).to_string();
        let pid = u16::from_le_bytes([factory[0x14], factory[0x15]]);
        if let Some(s) = side_by_pid(pid) {
            *self.side.write() = s.to_string();
        }
        let rgb = |o: usize| format!("#{:02x}{:02x}{:02x}", factory[o], factory[o + 1], factory[o + 2]);
        let colors = json!({"body": rgb(0x19), "buttons": rgb(0x1C), "highlight": rgb(0x1F), "grip": rgb(0x22)});
        // Gyro bias (float32 x/y/z, rad/s) follows a 4-byte field at 0x13040.
        let g = self.read_flash(0x13040, 0x10)?;
        let f = |o: usize| f32::from_le_bytes([g[o], g[o + 1], g[o + 2], g[o + 3]]) as f64;
        let mut bias = [f(4), f(8), f(12)];
        if !bias.iter().all(|v| v.is_finite() && v.abs() < 0.5) {
            bias = [0.0; 3];
        }
        // The controller's own stick is the "primary" calibration at 0x130A8;
        // a user calibration at 0x1FC040 (magic b2 a1) takes precedence.
        let mut cal = StickCal::parse(&self.read_flash(0x130A8, 9)?);
        let user = self.read_flash(0x1FC040, 11)?;
        if user[..2] == [0xb2, 0xa1]
            && let Some(u) = StickCal::parse(&user[2..11])
        {
            cal = Some(u);
        }
        let mut live = self.live.lock();
        live.info.insert("firmware".into(), json!(firmware));
        live.info.insert("serial".into(), json!(serial));
        live.info.insert("pid".into(), json!(format!("{pid:04x}")));
        live.info.insert("colors".into(), colors);
        live.gyro_bias_factory = bias;
        if let Some(c) = cal {
            live.cal = c;
        }
        let c = live.cal;
        live.info.insert(
            "stick_cal".into(),
            json!({"x": {"neutral": c.x.neutral, "max": c.x.max, "min": c.x.min},
                   "y": {"neutral": c.y.neutral, "max": c.y.max, "min": c.y.min}}),
        );
        Ok(())
    }

    /// Host addresses stored in the controller's pairing table (0x1FA000).
    fn pairing_entries(&self) -> Res<Vec<String>> {
        let raw = self.read_flash(0x1FA000, 0x40)?;
        let count = if raw[0] != 0xFF { raw[0] as usize } else { 0 };
        let mut out = Vec::new();
        for i in 0..count.min(2) {
            let a = &raw[0x08 + i * 0x28..0x0E + i * 0x28];
            out.push(a.iter().map(|x| format!("{x:02X}")).collect::<Vec<_>>().join(":"));
        }
        Ok(out)
    }

    fn ensure_paired(&self) -> Res<()> {
        let ours = self.host_mac.clone();
        let ours_rev = ours.split(':').rev().collect::<Vec<_>>().join(":");
        let entries = self.pairing_entries()?;
        {
            let mut live = self.live.lock();
            live.info.insert("paired_hosts".into(), json!(entries));
            if entries.contains(&ours) || entries.contains(&ours_rev) {
                live.info.insert("paired".into(), json!(true));
                return Ok(());
            }
        }
        log::info!("{}: pairing with this PC ({ours})", self.side());
        let host: Vec<u8> = ours.split(':').rev().map(|x| u8::from_str_radix(x, 16).unwrap_or(0)).collect();
        let mut host2 = host.clone(); // the console sends addr and addr-1
        host2[0] = host2[0].wrapping_sub(1);
        let mut d = vec![0x00, 0x02];
        d.extend_from_slice(&host);
        d.extend_from_slice(&host2);
        self.command(0x15, 0x01, &d)?;
        let a1: Vec<u8> = self.pairing_key.iter().zip(PAIRING_B1.iter()).map(|(k, b)| k ^ b).collect();
        let mut d = vec![0x00];
        d.extend_from_slice(&a1);
        let resp = self.command(0x15, 0x04, &d)?;
        if resp.len() < 17 {
            return Err(CommandError("short pairing response".into()).into());
        }
        let ltk: Vec<u8> = a1.iter().zip(resp[1..17].iter()).map(|(x, y)| x ^ y).collect();
        let mut a2 = [0u8; 16];
        getrandom::fill(&mut a2).map_err(|e| CommandError(e.to_string()))?;
        let mut d = vec![0x00];
        d.extend_from_slice(&a2);
        let resp = self.command(0x15, 0x02, &d)?;
        let key: Vec<u8> = ltk.iter().rev().copied().collect();
        let cipher = aes::Aes128::new_from_slice(&key).map_err(|e| CommandError(e.to_string()))?;
        let mut block_bytes = [0u8; 16];
        for (i, b) in a2.iter().rev().enumerate() {
            block_bytes[i] = *b;
        }
        let mut block = aes::Block::from(block_bytes);
        cipher.encrypt_block(&mut block);
        if resp.len() < 17 || resp[1..17] != block[..] {
            return Err(CommandError("controller's key confirmation did not match".into()).into());
        }
        self.command(0x15, 0x03, &[0x00])?;
        let entries = self.pairing_entries().unwrap_or_default();
        let mut live = self.live.lock();
        live.info.insert("paired".into(), json!(true));
        live.info.insert("paired_hosts".into(), json!(entries));
        log::info!("{}: paired", self.side());
        Ok(())
    }

    // --------------------------------------------------------------- commands

    /// Send a command on the command characteristic and wait for its response.
    pub fn command(&self, cmd: u8, sub: u8, data: &[u8]) -> Res<Vec<u8>> {
        let client = self.client()?;
        let handle = self.char_handle(UUID_COMMAND).ok_or_else(|| CommandError("no command characteristic".into()))?;
        let (tx, rx) = sync_channel(1);
        self.pending.lock().insert((cmd, sub), tx);
        let mut pkt = vec![cmd, 0x91, 0x01, sub, 0x00, data.len() as u8, 0x00, 0x00];
        pkt.extend_from_slice(data);
        let mut result = Err(CommandError(format!("no response to command {cmd:02x}/{sub:02x}")).into());
        for _ in 0..2 {
            if let Err(e) = client.write_cmd(handle, &pkt) {
                result = Err(e.into());
                break;
            }
            if let Ok(r) = rx.recv_timeout(Duration::from_millis(1500)) {
                result = Ok(r);
                break;
            }
        }
        self.pending.lock().remove(&(cmd, sub));
        result
    }

    pub fn read_flash(&self, addr: u32, length: usize) -> Res<Vec<u8>> {
        let mut out = Vec::with_capacity(length);
        while out.len() < length {
            let n = (length - out.len()).min(0x4F);
            let mut d = vec![n as u8, 0x7E, 0, 0];
            d.extend_from_slice(&(addr + out.len() as u32).to_le_bytes());
            let resp = self.command(0x02, 0x04, &d)?;
            if resp.len() < 8 + n {
                return Err(CommandError("short flash read".into()).into());
            }
            out.extend_from_slice(&resp[8..8 + n]);
        }
        Ok(out)
    }

    pub fn set_leds(&self, mask: u8, flash: bool) -> Res<()> {
        self.command(0x09, 0x07, &[mask & 0x0F, 0, 0, 0, 0, 0, 0, 0])?;
        self.command(0x09, 0x08, &[flash as u8, 0, 0, 0])?;
        Ok(())
    }

    /// Built-in vibration samples: 3 = connect click, 2 = search tone.
    pub fn play_sample(&self, sample: u8) -> Res<()> {
        self.command(0x0A, 0x02, &[sample, 0, 0, 0]).map(|_| ())
    }

    /// Send one HD rumble packet. Amplitudes and frequencies are 10-bit.
    pub fn rumble(&self, hi_freq: u16, hi_amp: u16, lo_freq: u16, lo_amp: u16) {
        if !self.connected.load(Ordering::Relaxed) {
            return;
        }
        let Some(h) = self.char_handle(uuid_rumble(&self.side())) else { return };
        let Ok(client) = self.client() else { return };
        let f: u64 = (hi_freq as u64 & 0x3FF)
            | ((hi_amp as u64 & 0x3FF) << 10)
            | ((lo_freq as u64 & 0x3FF) << 20)
            | ((lo_amp as u64 & 0x3FF) << 30);
        let frame = &f.to_le_bytes()[..5];
        let seq = self.rumble_seq.fetch_add(1, Ordering::Relaxed) & 0x0F;
        let mut pkt = vec![0x00, 0x50 | seq];
        for _ in 0..3 {
            pkt.extend_from_slice(frame);
        }
        pkt.resize(42, 0);
        let _ = client.write_cmd(h, &pkt);
    }

    pub fn disconnect(&self) {
        self.connected.store(false, Ordering::SeqCst);
        if let Some(c) = self.client.read().as_ref() {
            c.close();
        }
    }

    // ---------------------------------------------------------------- reports

    fn on_link_lost(&self) {
        self.connected.store(false, Ordering::SeqCst);
        self.ready.store(false, Ordering::SeqCst);
        if let Some(me) = self.me.upgrade() {
            (self.on_disconnect)(&me);
        }
    }

    fn on_notify(&self, handle: u16, data: &[u8]) {
        if Some(handle) == self.char_handle(UUID_INPUT) {
            self.on_report(data);
            return;
        }
        if Some(handle) == self.char_handle(UUID_RESPONSE)
            && data.len() >= 8
            && let Some(tx) = self.pending.lock().get(&(data[0], data[3]))
        {
            let _ = tx.try_send(data[8..].to_vec());
        }
    }

    fn on_report(&self, r: &[u8]) {
        if r.len() < 0x3C {
            return;
        }
        let t = now();
        let le16 = |o: usize| u16::from_le_bytes([r[o], r[o + 1]]);
        let le32 = |o: usize| u32::from_le_bytes([r[o], r[o + 1], r[o + 2], r[o + 3]]);
        let side = self.side();
        let mut s = State { t, counter: le32(0), b4: r[4], b5: r[5], b6: r[6], b7: r[7], ..State::default() };
        let raw = if side == "right" { u12_pair(&r[13..16]) } else { u12_pair(&r[10..13]) };
        s.stick_raw = raw;
        s.mouse = [le16(0x10), le16(0x12), le16(0x14), le16(0x16)];
        s.battery_mv = le16(0x1F);
        s.charge = r[0x21];
        s.imu_ts = le32(0x2A);
        let state = {
            let mut live = self.live.lock();
            s.stick = (live.cal.x.normalize(raw.0).clamp(-1.25, 1.25), live.cal.y.normalize(raw.1).clamp(-1.25, 1.25));
            if s.imu_ts != 0 {
                let i16_at = |o: usize| i16::from_le_bytes([r[o], r[o + 1]]) as f64;
                let (ax, ay, az) = (i16_at(0x30), i16_at(0x32), i16_at(0x34));
                let (gx, gy, gz) = (i16_at(0x36), i16_at(0x38), i16_at(0x3A));
                live.check_imu_timebase(t, s.imu_ts, &side);
                let k = live.gyro_scale;
                let fb = live.gyro_bias_factory;
                let g = [gx * k - fb[0], gy * k - fb[1], gz * k - fb[2]];
                let a = [ax / ACCEL_LSB_PER_G, ay / ACCEL_LSB_PER_G, az / ACCEL_LSB_PER_G];
                live.gyro_cal.feed(t, g, a);
                let rb = live.gyro_cal.bias;
                s.gyro = [g[0] - rb[0], g[1] - rb[1], g[2] - rb[2]];
                s.accel = a;
                s.imu_valid = true;
            }
            live.update_link_stats(t);
            live.reports += 1;
            live.last_raw.clear();
            live.last_raw.extend_from_slice(&r[..r.len().min(64)]);
            if s.imu_valid {
                live.orientation.update(t, s.gyro, s.accel);
            }
            live.state = s.clone();
            s
        };
        if let Some(me) = self.me.upgrade() {
            (self.on_state)(&me, &state);
        }
    }
}

impl Live {
    /// Link statistics from arrival times. The mean is an exponential average
    /// of every interval (late ones capped), so one early burst of reports
    /// can't pin it at a tiny value.
    fn update_link_stats(&mut self, t: f64) {
        if self.last_t > 0.0 {
            let dt = (t - self.last_t) * 1000.0;
            if self.interval_ms == 0.0 {
                self.interval_ms = dt.max(1.0);
            }
            if dt > 2.5 * self.interval_ms {
                self.gaps += 1;
            }
            self.interval_ms += (dt.min(3.0 * self.interval_ms) - self.interval_ms) * 0.02;
            self.interval_ms = self.interval_ms.max(0.5);
            self.jitter_ms += ((dt - self.interval_ms).abs() - self.jitter_ms) * 0.02;
            self.rate_hz = 1000.0 / self.interval_ms;
            self.gap_window.push_back(dt);
            if self.gap_window.len() > 260 {
                self.gap_window.pop_front();
            }
            self.worst_gap_ms = self.gap_window.iter().copied().fold(0.0, f64::max);
        }
        self.last_t = t;
    }

    /// Pick the gyro full-scale from the IMU timestamp rate: microsecond
    /// timers read 34.8 rad/s full-scale, others 40 rad/s (as SDL does).
    fn check_imu_timebase(&mut self, t: f64, ts: u32, side: &str) {
        if self.imu_checked {
            return;
        }
        let Some((t0, ts0)) = self.imu_base else {
            self.imu_base = Some((t, ts));
            return;
        };
        let dt = t - t0;
        if dt < 1.0 {
            return;
        }
        let rate = ts.wrapping_sub(ts0) as f64 / dt;
        self.imu_checked = true;
        let micro = rate > 0.9e6 && rate < 1.1e6;
        self.gyro_scale = (if micro { 34.8 } else { 40.0 }) / 32767.0;
        self.info.insert("imu_timer_hz".into(), json!(rate.round()));
        log::info!("{side}: IMU timer {rate:.0} Hz -> gyro full-scale {} rad/s", if micro { "34.8" } else { "40.0" });
    }
}
