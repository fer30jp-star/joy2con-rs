// SPDX-License-Identifier: GPL-3.0-or-later
//! Mouse mode: a Joy-Con 2 used as a mouse through its optical sensor.
//!
//! Report 0x05 carries the optical sensor's X and Y as free-running 16-bit
//! counters at 0x10 and 0x12 (as decoded by jc2mouse); movement is the
//! wrapping difference between consecutive reports. A Joy-Con in mouse mode
//! drives one shared virtual mouse (uinput) and stops feeding the emulated
//! gamepad, so games don't see its buttons twice.
//!
//! Buttons: shoulder (L / R) = left click, trigger (ZL / ZR) = right click,
//! stick press = middle click, stick = scroll wheel. Holding the two rail
//! buttons (SL + SR) for a second toggles mouse mode on that Joy-Con.

use std::io;

use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, EventType, InputEvent, KeyCode, RelativeAxisCode};
use serde::{Deserialize, Serialize};

use crate::ns2::State;

/// User settings, stored in config.json under "mouse".
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct MouseConfig {
    pub left: bool,
    pub right: bool,
    pub sensitivity: f64,
    pub invert_x: bool,
    pub invert_y: bool,
    pub swap_axes: bool,
    pub scroll_speed: f64,
}

impl Default for MouseConfig {
    fn default() -> Self {
        MouseConfig {
            left: false,
            right: false,
            sensitivity: 1.0,
            invert_x: false,
            invert_y: false,
            swap_axes: false,
            scroll_speed: 1.0,
        }
    }
}

impl MouseConfig {
    pub fn enabled(&self, side: &str) -> bool {
        match side {
            "left" => self.left,
            "right" => self.right,
            _ => false,
        }
    }

    pub fn set(&mut self, side: &str, on: bool) {
        match side {
            "left" => self.left = on,
            "right" => self.right = on,
            _ => {}
        }
    }
}

pub struct VirtualMouse {
    dev: VirtualDevice,
}

impl VirtualMouse {
    pub fn new() -> io::Result<VirtualMouse> {
        let mut keys = AttributeSet::<KeyCode>::new();
        keys.insert(KeyCode::BTN_LEFT);
        keys.insert(KeyCode::BTN_RIGHT);
        keys.insert(KeyCode::BTN_MIDDLE);
        let mut rel = AttributeSet::<RelativeAxisCode>::new();
        for r in [
            RelativeAxisCode::REL_X,
            RelativeAxisCode::REL_Y,
            RelativeAxisCode::REL_WHEEL,
            RelativeAxisCode::REL_HWHEEL,
            RelativeAxisCode::REL_WHEEL_HI_RES,
            RelativeAxisCode::REL_HWHEEL_HI_RES,
        ] {
            rel.insert(r);
        }
        let dev = VirtualDevice::builder()?
            .name("joy2con Joy-Con mouse")
            .with_keys(&keys)?
            .with_relative_axes(&rel)?
            .build()?;
        Ok(VirtualMouse { dev })
    }

    fn emit(&mut self, events: &[InputEvent]) {
        if events.is_empty() {
            return;
        }
        if let Err(e) = self.dev.emit(events) {
            log::debug!("virtual mouse: {e}");
        }
    }
}

/// Mouse-mode tracking for one Joy-Con.
#[derive(Default)]
pub struct MouseTracker {
    prev: Option<(u16, u16)>,
    acc: (f64, f64),
    scroll_acc: (f64, f64),
    wheel_total: (i64, i64),
    buttons: [bool; 3],
    last_t: f64,
    combo_since: Option<f64>,
    combo_fired: bool,
    /// Movement seen in the last report, for the app's live view.
    pub last_delta: (i32, i32),
}

const MAX_STEP: i32 = 400; // larger jumps are sensor glitches or counter resets
const SCROLL_DEADZONE: f64 = 0.25;
const SCROLL_LINES_PER_S: f64 = 20.0;
const COMBO_HOLD_S: f64 = 1.0;

fn delta_u16(curr: u16, prev: u16) -> i32 {
    let d = curr.wrapping_sub(prev) as i32;
    if d > 0x7FFF { d - 0x10000 } else { d }
}

impl MouseTracker {
    /// True once when SL+SR on this Joy-Con have been held for a second.
    pub fn combo(&mut self, side: &str, s: &State) -> bool {
        let held = if side == "left" { s.is("SL_L") && s.is("SR_L") } else { s.is("SL_R") && s.is("SR_R") };
        if !held {
            self.combo_since = None;
            self.combo_fired = false;
            return false;
        }
        let since = *self.combo_since.get_or_insert(s.t);
        if !self.combo_fired && s.t - since >= COMBO_HOLD_S {
            self.combo_fired = true;
            return true;
        }
        false
    }

    /// Forget the previous sample, so the next report sets a new baseline.
    pub fn reset(&mut self) {
        self.prev = None;
        self.acc = (0.0, 0.0);
        self.scroll_acc = (0.0, 0.0);
        self.last_t = 0.0;
        self.last_delta = (0, 0);
    }

    /// Turn one report into mouse events.
    pub fn feed(&mut self, side: &str, s: &State, cfg: &MouseConfig, mouse: &mut VirtualMouse) {
        let mut ev = Vec::with_capacity(8);
        let rel = |code: RelativeAxisCode, v: i32| InputEvent::new(EventType::RELATIVE.0, code.0, v);

        // Movement.
        let (x, y) = (s.mouse[0], s.mouse[1]);
        if let Some((px, py)) = self.prev {
            let (mut dx, mut dy) = (delta_u16(x, px), delta_u16(y, py));
            if dx.abs() > MAX_STEP || dy.abs() > MAX_STEP {
                dx = 0;
                dy = 0;
            }
            if cfg.swap_axes {
                std::mem::swap(&mut dx, &mut dy);
            }
            if cfg.invert_x {
                dx = -dx;
            }
            if cfg.invert_y {
                dy = -dy;
            }
            self.last_delta = (dx, dy);
            self.acc.0 += dx as f64 * cfg.sensitivity;
            self.acc.1 += dy as f64 * cfg.sensitivity;
            let (ix, iy) = (self.acc.0.trunc() as i32, self.acc.1.trunc() as i32);
            self.acc.0 -= ix as f64;
            self.acc.1 -= iy as f64;
            if ix != 0 {
                ev.push(rel(RelativeAxisCode::REL_X, ix));
            }
            if iy != 0 {
                ev.push(rel(RelativeAxisCode::REL_Y, iy));
            }
        }
        self.prev = Some((x, y));

        // Buttons.
        let (b_left, b_right, b_mid) = if side == "left" {
            (s.is("L"), s.is("ZL"), s.is("LSTICK"))
        } else {
            (s.is("R"), s.is("ZR"), s.is("RSTICK"))
        };
        for (i, (now, code)) in
            [(b_left, KeyCode::BTN_LEFT), (b_right, KeyCode::BTN_RIGHT), (b_mid, KeyCode::BTN_MIDDLE)]
                .into_iter()
                .enumerate()
        {
            if now != self.buttons[i] {
                self.buttons[i] = now;
                ev.push(InputEvent::new(EventType::KEY.0, code.0, now as i32));
            }
        }

        // Scroll from the stick: speed grows with deflection past a dead zone.
        let dt = if self.last_t > 0.0 { (s.t - self.last_t).clamp(0.0, 0.05) } else { 0.0 };
        self.last_t = s.t;
        let speed = |v: f64| -> f64 {
            let a = v.abs();
            if a < SCROLL_DEADZONE {
                return 0.0;
            }
            let f = ((a - SCROLL_DEADZONE) / (1.0 - SCROLL_DEADZONE)).min(1.0);
            v.signum() * f.powf(1.6) * SCROLL_LINES_PER_S * cfg.scroll_speed
        };
        // Hi-res wheel units: 120 per detent.
        self.scroll_acc.1 += speed(s.stick.1) * dt * 120.0;
        self.scroll_acc.0 += speed(s.stick.0) * dt * 120.0;
        for (acc, total, hi, lo) in [
            (
                &mut self.scroll_acc.1,
                &mut self.wheel_total.1,
                RelativeAxisCode::REL_WHEEL_HI_RES,
                RelativeAxisCode::REL_WHEEL,
            ),
            (
                &mut self.scroll_acc.0,
                &mut self.wheel_total.0,
                RelativeAxisCode::REL_HWHEEL_HI_RES,
                RelativeAxisCode::REL_HWHEEL,
            ),
        ] {
            let units = acc.trunc() as i64;
            if units == 0 {
                continue;
            }
            *acc -= units as f64;
            ev.push(rel(hi, units as i32));
            // Classic wheel events for programs without hi-res support: one
            // detent per 120 hi-res units crossed.
            let before = total.div_euclid(120);
            *total += units;
            let after = total.div_euclid(120);
            if after != before {
                ev.push(rel(lo, (after - before) as i32));
            }
        }
        mouse.emit(&ev);
    }

    /// Release anything still held when leaving mouse mode.
    pub fn release(&mut self, mouse: &mut VirtualMouse) {
        let mut ev = Vec::new();
        for (i, code) in [KeyCode::BTN_LEFT, KeyCode::BTN_RIGHT, KeyCode::BTN_MIDDLE].into_iter().enumerate() {
            if self.buttons[i] {
                self.buttons[i] = false;
                ev.push(InputEvent::new(EventType::KEY.0, code.0, 0));
            }
        }
        mouse.emit(&ev);
        self.reset();
    }
}
