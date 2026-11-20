// SPDX-License-Identifier: GPL-3.0-or-later
//! joy2con desktop app: an egui window and a tray icon for the joy2con daemon.
//!
//! The app is only a client. It reads state from the daemon's Unix socket and
//! sends actions back; closing the window leaves the driver running, and the
//! tray icon keeps battery levels in view.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use eframe::egui::{
    self, Align2, Color32, ColorImage, CornerRadius, FontId, Frame, Margin, Pos2, Rect, RichText, Sense, Shape, Stroke,
    TextureHandle, TextureOptions, Vec2,
};
use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::art;
use crate::util::socket_path;

// ------------------------------------------------------------------ client

#[derive(Default)]
struct Shared {
    state: Value,
    events: Vec<Value>, // newest first
    connected: bool,
    writer: Option<UnixStream>,
    seq: u64,
}

#[derive(Clone)]
struct Client {
    shared: Arc<Mutex<Shared>>,
}

impl Client {
    fn start(ctx: egui::Context) -> Client {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let s = shared.clone();
        std::thread::Builder::new()
            .name("client".into())
            .spawn(move || {
                loop {
                    if let Ok(stream) = UnixStream::connect(socket_path()) {
                        {
                            let mut g = s.lock();
                            g.connected = true;
                            g.writer = stream.try_clone().ok();
                        }
                        ctx.request_repaint();
                        for line in BufReader::new(stream).lines() {
                            let Ok(line) = line else { break };
                            let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
                            let mut g = s.lock();
                            if let Some(evs) = msg.get("events").and_then(|v| v.as_array()) {
                                let mut new: Vec<Value> = evs.iter().rev().cloned().collect();
                                new.append(&mut g.events);
                                new.truncate(200);
                                g.events = new;
                            }
                            if msg["type"] == "state" {
                                g.state = msg["state"].clone();
                                g.seq += 1;
                            }
                            drop(g);
                            ctx.request_repaint();
                        }
                        let mut g = s.lock();
                        g.connected = false;
                        g.writer = None;
                        drop(g);
                        ctx.request_repaint();
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
            })
            .expect("client thread");
        Client { shared }
    }

    fn action(&self, name: &str, args: Value) {
        let mut g = self.shared.lock();
        if let Some(w) = g.writer.as_mut() {
            let line = json!({"action": name, "args": args}).to_string() + "\n";
            if w.write_all(line.as_bytes()).is_err() {
                g.writer = None;
            }
        }
    }
}

fn start_driver() {
    let ok = std::process::Command::new("systemctl")
        .args(["--user", "start", "joy2con.service"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok && let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).arg("daemon").spawn();
    }
}

// -------------------------------------------------------------------- tray

struct Tray {
    shared: Arc<Mutex<Shared>>,
    show: Arc<AtomicBool>,
    ctx: egui::Context,
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "joy2con".into()
    }
    fn title(&self) -> String {
        "joy2con".into()
    }
    fn icon_name(&self) -> String {
        "input-gaming".into()
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip { title: "joy2con".into(), description: tray_tip(&self.shared.lock()), ..Default::default() }
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        self.show.store(true, Ordering::SeqCst);
        self.ctx.request_repaint();
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        vec![
            StandardItem {
                label: "Mostrar joy2con".into(),
                activate: Box::new(|t: &mut Self| {
                    t.show.store(true, Ordering::SeqCst);
                    t.ctx.request_repaint();
                }),
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            StandardItem {
                label: "Salir de la app (el driver sigue)".into(),
                activate: Box::new(|_| std::process::exit(0)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

fn tray_tip(s: &Shared) -> String {
    if !s.connected {
        return "El driver no está corriendo".into();
    }
    let mut parts = Vec::new();
    if let Some(jcs) = s.state["joycons"].as_object() {
        let mut sides: Vec<_> = jcs.iter().collect();
        sides.sort_by_key(|(k, _)| k.to_string());
        for (side, j) in sides {
            parts.push(format!("{} {}%", side_name_short(side), j["battery_pct"]));
        }
    }
    if parts.is_empty() { "Ningún Joy-Con conectado".into() } else { format!("Joy-Cons: {}", parts.join(", ")) }
}

// --------------------------------------------------------------------- app

#[derive(Clone, PartialEq)]
enum Page {
    Home,
    JoyCon(String),
    Diagnostics,
}

struct App {
    client: Client,
    page: Page,
    textures: HashMap<String, TextureHandle>,
    gyro: HashMap<String, VecDeque<[f64; 3]>>,
    last_seq: u64,
    show: Arc<AtomicBool>,
    has_tray: bool,
    hidden: bool,
}

fn side_name(side: &str) -> &'static str {
    if side == "left" { "Joy-Con izquierdo" } else { "Joy-Con derecho" }
}

fn side_name_short(side: &str) -> &'static str {
    if side == "left" { "Izq." } else { "Der." }
}

fn f(v: &Value) -> f64 {
    v.as_f64().unwrap_or(0.0)
}

fn color_hex(c: Color32) -> String {
    format!("#{:02x}{:02x}{:02x}", c.r(), c.g(), c.b())
}

fn parse_hex(s: Option<&str>) -> Option<Color32> {
    let s = s?.trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(s, 16).ok()?;
    Some(Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

fn tint(base: Color32, over: Color32, a: f32) -> Color32 {
    let m = |b: u8, o: u8| (b as f32 * (1.0 - a) + o as f32 * a).round() as u8;
    Color32::from_rgb(m(base.r(), over.r()), m(base.g(), over.g()), m(base.b(), over.b()))
}

/// A titled group of rows, like a settings card.
fn card<R>(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.add_space(14.0);
    ui.label(RichText::new(title).strong().color(ui.visuals().weak_text_color()));
    ui.add_space(4.0);
    Frame::new()
        .fill(ui.visuals().faint_bg_color)
        .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// One row: a label with a description underneath and an optional value on the right.
fn row(ui: &mut egui::Ui, text: &str, desc: &str, trailing: Option<RichText>) {
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(text);
            if !desc.is_empty() {
                ui.label(RichText::new(desc).small().color(ui.visuals().weak_text_color()));
            }
        });
        if let Some(t) = trailing {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(t);
            });
        }
    });
}

fn banner(ui: &mut egui::Ui, color: Color32, text: &str) {
    Frame::new()
        .fill(color.gamma_multiply(0.18))
        .stroke(Stroke::new(1.0, color))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(text);
        });
    ui.add_space(6.0);
}

fn banner_action(ui: &mut egui::Ui, color: Color32, text: &str, action: &str) -> bool {
    let mut clicked = false;
    Frame::new()
        .fill(color.gamma_multiply(0.18))
        .stroke(Stroke::new(1.0, color))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                ui.label(text);
                if ui.button(action).clicked() {
                    clicked = true;
                }
            });
        });
    ui.add_space(6.0);
    clicked
}

struct ArtArgs<'a> {
    side: &'a str,
    accent: Color32,
    buttons: Vec<String>,
    stick: (f32, f32),
    player_leds: u8,
    live: bool,
    highlighted: bool,
    width: f32,
}

impl App {
    fn texture(
        &mut self,
        ctx: &egui::Context,
        key: String,
        svg: impl FnOnce() -> String,
        px: [u32; 2],
    ) -> egui::TextureId {
        let full = format!("{key}@{}x{}", px[0], px[1]);
        if let Some(t) = self.textures.get(&full) {
            return t.id();
        }
        let img = art::rasterize(&svg(), px[0], px[1])
            .map(|(data, size)| ColorImage::from_rgba_premultiplied(size, &data))
            .unwrap_or_else(|| ColorImage::new([1, 1], vec![Color32::TRANSPARENT]));
        let handle = ctx.load_texture(full.clone(), img, TextureOptions::LINEAR);
        let id = handle.id();
        self.textures.insert(full, handle);
        id
    }

    /// The live Joy-Con drawing: line art in the theme's colours, the
    /// controller's accent colour on pressed buttons, the stick cap following
    /// the stick, and the player lights the host has set.
    fn draw_joycon(&mut self, ui: &mut egui::Ui, a: ArtArgs) -> egui::Response {
        let size = Vec2::new(a.width, a.width * art::H / art::W);
        let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
        let ctx = ui.ctx().clone();
        let ppp = ctx.pixels_per_point();
        let px = [(size.x * ppp).ceil() as u32, (size.y * ppp).ceil() as u32];
        let bg = ui.visuals().panel_fill;
        let ink = ui.visuals().text_color();
        let fill = tint(bg, ink, 0.06);
        let line = tint(bg, ink, 0.75);
        let opacity = if a.live || a.highlighted { 1.0 } else { 0.35 };
        let white = Color32::WHITE.gamma_multiply(opacity);
        let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
        let s = size.x / art::W;
        let at = |x: f32, y: f32| rect.min + Vec2::new(x * s, y * s);
        let side = a.side.to_string();
        let painter = ui.painter_at(rect.expand(8.0));

        if a.highlighted {
            painter.rect_filled(
                rect.expand(6.0),
                CornerRadius::same(10),
                ui.visuals().widgets.hovered.bg_fill.gamma_multiply(0.35),
            );
        }
        let (fh, lh, ah) = (color_hex(fill), color_hex(line), color_hex(a.accent));
        let t_fill = self.texture(&ctx, format!("{side}/fill/{fh}"), || art::fill_svg(&side, Some(&fh)), px);
        painter.image(t_fill, rect, uv, white);
        // pressed buttons fill with the accent colour, under the outlines
        let pressed = |n: &str| a.buttons.iter().any(|b| b == n);
        for (name, shape, cx, cy, w, h) in art::buttons(a.side) {
            if !pressed(name) {
                continue;
            }
            let c = a.accent.gamma_multiply(0.7 * opacity);
            match shape {
                art::Shape::Circle => {
                    painter.circle_filled(at(*cx, *cy), w * s, c);
                }
                art::Shape::Rect => {
                    let r = Rect::from_center_size(at(*cx, *cy), Vec2::new(w * s, h * s));
                    painter.rect_filled(r, CornerRadius::same((w.min(*h) * s * 0.25) as u8), c);
                }
            }
        }
        for name in art::shoulders(a.side) {
            if pressed(name) {
                let t = self.texture(
                    &ctx,
                    format!("{side}/shoulder/{name}/{ah}"),
                    || art::shoulder_svg(&side, name, Some(&ah)),
                    px,
                );
                painter.image(t, rect, uv, Color32::WHITE.gamma_multiply(0.7 * opacity));
            }
        }
        let t_lines =
            self.texture(&ctx, format!("{side}/lines/{lh}/{ah}"), || art::lines_svg(&side, Some(&lh), Some(&ah)), px);
        painter.image(t_lines, rect, uv, white);
        if a.highlighted {
            let t = self.texture(&ctx, format!("{side}/outline/{ah}"), || art::outline_svg(&side, Some(&ah)), px);
            painter.image(t, rect, uv, Color32::WHITE);
        }
        // stick cap
        let (scx, scy, travel, cap) = art::stick(a.side);
        let click = pressed(if a.side == "left" { "LSTICK" } else { "RSTICK" });
        let center = at(scx + a.stick.0.clamp(-1.2, 1.2) * travel, scy - a.stick.1.clamp(-1.2, 1.2) * travel);
        let cap_fill = if click { tint(fill, a.accent, 0.7) } else { fill };
        painter.circle(
            center,
            cap * s,
            cap_fill.gamma_multiply(opacity),
            Stroke::new(1.7 * s, line.gamma_multiply(opacity)),
        );
        painter.circle_stroke(center, cap * s * 0.58, Stroke::new(1.36 * s, line.gamma_multiply(0.5 * opacity)));
        // player lights
        let (lx, ys) = art::leds(a.side);
        let t = ctx.input(|i| i.time);
        for (i, y) in ys.iter().enumerate() {
            let flashing = (a.player_leds >> 4 >> i) & 1 == 1;
            let on = a.live && (((a.player_leds >> i) & 1) == 1 || flashing);
            if on {
                let blink =
                    if flashing { (0.15 + 0.85 * ((t * std::f64::consts::PI / 0.6).sin().abs())) as f32 } else { 1.0 };
                let r = Rect::from_center_size(at(lx, *y), Vec2::new(4.0 * s, 6.0 * s));
                painter.rect_filled(
                    r,
                    CornerRadius::same(s as u8),
                    Color32::from_rgb(0x7d, 0xff, 0xa0).gamma_multiply(blink),
                );
                if flashing {
                    ctx.request_repaint_after(Duration::from_millis(30));
                }
            }
        }
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    }

    fn joycon_art_args<'a>(&self, st: &Value, side: &'a str, width: f32, highlighted: bool) -> ArtArgs<'a> {
        let jc = &st["joycons"][side];
        let live = jc.is_object();
        let known = known_for(st, side);
        let colors = if live { &jc["info"]["colors"] } else { &known["colors"] };
        let accent = parse_hex(colors["highlight"].as_str()).unwrap_or(if side == "left" {
            Color32::from_rgb(0x9b, 0xe1, 0xe6)
        } else {
            Color32::from_rgb(0xff, 0x8c, 0x5f)
        });
        ArtArgs {
            side,
            accent,
            buttons: jc["buttons"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
            stick: (f(&jc["stick"][0]) as f32, f(&jc["stick"][1]) as f32),
            player_leds: leds_for(st, side),
            live,
            highlighted,
            width,
        }
    }

    // ------------------------------------------------------------- pages

    fn home(&mut self, ui: &mut egui::Ui, st: &Value, connected: bool) {
        if !connected
            && banner_action(
                ui,
                Color32::from_rgb(0xe5, 0x48, 0x4d),
                "El driver de joy2con no está corriendo.",
                "Iniciar",
            )
        {
            start_driver();
        }
        if connected && st["foreign_discovery"].as_bool().unwrap_or(false) {
            banner(
                ui,
                Color32::from_rgb(0xf5, 0xa5, 0x24),
                "Otro programa está buscando dispositivos Bluetooth, así que los Joy-Con no pueden conectarse y pierden entradas. Cierra la ventana \"Añadir dispositivo\".",
            );
        }
        if let Some(err) = st["mouse_error"].as_str() {
            banner(ui, Color32::from_rgb(0xf5, 0xa5, 0x24), err);
        }
        for s in st["seen"].as_array().into_iter().flatten() {
            if s["kind"] == "other-host" {
                let side = s["side"].as_str().unwrap_or("");
                let mac = s["mac"].as_str().unwrap_or("").to_string();
                let text = format!("Hay un {} cerca que está emparejado con otro equipo.", side_name(side));
                if banner_action(ui, Color32::from_rgb(0x3e, 0x7b, 0xfa), &text, "Usar con esta PC") {
                    self.client.action("takeover", json!({"mac": mac}));
                }
            }
        }

        // Both Joy-Cons side by side, live; clicking one opens its page.
        ui.add_space(12.0);
        let width = 120.0f32.min((ui.available_width() - 60.0) / 2.0).max(60.0);
        ui.horizontal(|ui| {
            let total = width * 2.0 + 40.0;
            ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
            for side in ["left", "right"] {
                ui.vertical(|ui| {
                    let hovered_id = ui.make_persistent_id(("hover", side));
                    let hot = ui.ctx().data(|d| d.get_temp::<bool>(hovered_id)).unwrap_or(false);
                    let args = self.joycon_art_args(st, side, width, hot);
                    let resp = self.draw_joycon(ui, args);
                    ui.ctx().data_mut(|d| d.insert_temp(hovered_id, resp.hovered()));
                    if resp.clicked() {
                        self.page = Page::JoyCon(side.into());
                    }
                    let jc = &st["joycons"][side];
                    let text = if jc.is_object() {
                        let pct = jc["battery_pct"].as_i64().unwrap_or(0);
                        format!("{pct}%{}", if charging(jc) { " (cargando)" } else { "" })
                    } else if known_for(st, side).is_object() {
                        "Dormido".into()
                    } else {
                        "Sin emparejar".into()
                    };
                    ui.add_space(6.0);
                    ui.vertical_centered(|ui| {
                        ui.set_width(width);
                        ui.label(RichText::new(text).color(if jc.is_object() {
                            ui.visuals().text_color()
                        } else {
                            ui.visuals().weak_text_color()
                        }));
                    });
                });
                ui.add_space(40.0);
            }
        });
        if !(st["joycons"]["left"].is_object() && st["joycons"]["right"].is_object()) {
            ui.add_space(8.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new("Presiona cualquier botón de un Joy-Con para despertarlo. Para emparejar uno nuevo, presiona el botón sync de su riel.")
                        .color(ui.visuals().weak_text_color()),
                );
            });
        }

        card(ui, "Controles", |ui| {
            for (i, side) in ["left", "right"].into_iter().enumerate() {
                if i > 0 {
                    ui.separator();
                }
                let jc = &st["joycons"][side];
                let desc = if jc.is_object() {
                    let base = if jc["ready"].as_bool().unwrap_or(false) { "Conectado" } else { "Conectando" };
                    if jc["mouse_mode"].as_bool().unwrap_or(false) {
                        format!("{base}, en modo ratón")
                    } else {
                        base.to_string()
                    }
                } else if known_for(st, side).is_object() {
                    "Dormido".into()
                } else {
                    "Sin emparejar".into()
                };
                let r = ui.add(egui::Button::new(format!("{}  ·  {desc}", side_name(side))).frame(false));
                if r.clicked() {
                    self.page = Page::JoyCon(side.into());
                }
            }
        });

        card(ui, "En juegos", |ui| {
            let mode = st["mode"].as_str().unwrap_or("pro");
            ui.horizontal(|ui| {
                ui.label("Aparecer como");
                egui::ComboBox::from_id_salt("mode")
                    .selected_text(if mode == "joycons" { "Dos Joy-Con separados" } else { "Un Pro Controller" })
                    .show_ui(ui, |ui| {
                        if ui.selectable_label(mode != "joycons", "Un Pro Controller").clicked() {
                            self.client.action("mode", json!({"mode": "pro"}));
                        }
                        if ui.selectable_label(mode == "joycons", "Dos Joy-Con separados").clicked() {
                            self.client.action("mode", json!({"mode": "joycons"}));
                        }
                    });
            });
            if mode != "joycons" {
                ui.separator();
                let src = st["imu_source"].as_str().unwrap_or("right");
                ui.horizontal(|ui| {
                    ui.label("Controles de movimiento desde");
                    egui::ComboBox::from_id_salt("imu").selected_text(side_name(src)).show_ui(ui, |ui| {
                        for s in ["right", "left"] {
                            if ui.selectable_label(src == s, side_name(s)).clicked() {
                                self.client.action("imu_source", json!({"source": s}));
                            }
                        }
                    });
                });
            }
            ui.separator();
            let mut drift = st["gyro_autocal"].as_bool().unwrap_or(true);
            if ui.checkbox(&mut drift, "Corregir la deriva del giroscopio").changed() {
                self.client.action("gyro_autocal", json!({"enabled": drift}));
            }
            ui.label(
                RichText::new("Recalibra el giroscopio cada vez que un Joy-Con se queda quieto.")
                    .small()
                    .color(ui.visuals().weak_text_color()),
            );
            ui.separator();
            let names = consumers(st);
            row(ui, "En uso por", if names.is_empty() { "Ningún programa ahora" } else { &names }, None);
        });

        self.mouse_card(ui, st, None);
    }

    fn mouse_card(&mut self, ui: &mut egui::Ui, st: &Value, only: Option<&str>) {
        let m = st["mouse"].clone();
        card(ui, "Modo ratón", |ui| {
            ui.label(
                RichText::new(
                    "Usa un Joy-Con como ratón con su sensor óptico: apóyalo sobre la mesa con el riel hacia abajo. \
                     Hombro (L/R) = clic izquierdo, gatillo (ZL/ZR) = clic derecho, presionar el stick = clic central, \
                     stick = rueda. Mantén SL + SR un segundo para activarlo o desactivarlo desde el propio control.",
                )
                .small()
                .color(ui.visuals().weak_text_color()),
            );
            ui.add_space(6.0);
            let sides: Vec<&str> = match only {
                Some(s) => vec![s],
                None => vec!["left", "right"],
            };
            for side in sides {
                let mut on = m[side].as_bool().unwrap_or(false);
                if ui.checkbox(&mut on, format!("{} como ratón", side_name(side))).changed() {
                    self.client.action("mouse_mode", json!({"side": side, "enabled": on}));
                }
            }
            ui.separator();
            let mut sens = m["sensitivity"].as_f64().unwrap_or(1.0);
            if ui.add(egui::Slider::new(&mut sens, 0.1..=8.0).logarithmic(true).text("Sensibilidad")).changed() {
                self.client.action("mouse_settings", json!({"sensitivity": sens}));
            }
            let mut scroll = m["scroll_speed"].as_f64().unwrap_or(1.0);
            if ui
                .add(egui::Slider::new(&mut scroll, 0.1..=5.0).logarithmic(true).text("Velocidad de la rueda"))
                .changed()
            {
                self.client.action("mouse_settings", json!({"scroll_speed": scroll}));
            }
            ui.horizontal_wrapped(|ui| {
                for (key, label) in
                    [("invert_x", "Invertir X"), ("invert_y", "Invertir Y"), ("swap_axes", "Intercambiar ejes")]
                {
                    let mut v = m[key].as_bool().unwrap_or(false);
                    if ui.checkbox(&mut v, label).changed() {
                        self.client.action("mouse_settings", json!({ key: v }));
                    }
                }
            });
        });
    }

    fn joycon_page(&mut self, ui: &mut egui::Ui, st: &Value, side: &str) {
        let jc = st["joycons"][side].clone();
        let live = jc.is_object();
        let known = known_for(st, side);
        let info = if live { jc["info"].clone() } else { known.clone() };

        ui.add_space(12.0);
        ui.vertical_centered(|ui| {
            let args = self.joycon_art_args(st, side, 110.0, false);
            self.draw_joycon(ui, args);
        });

        card(ui, "Estado", |ui| {
            let conn = if !live {
                if known.is_object() {
                    "Dormido. Presiona cualquier botón para reconectarlo."
                } else {
                    "No está emparejado con esta PC."
                }
            } else if jc["ready"].as_bool().unwrap_or(false) {
                "Conectado y emparejado con esta PC"
            } else {
                "Conectando"
            };
            row(ui, "Conexión", conn, None);
            if live {
                ui.separator();
                let mv = f(&jc["battery_mv"]);
                let state = if jc["charge"] == 0x20 {
                    "carga completa"
                } else if charging(&jc) {
                    "cargando"
                } else {
                    "sin cargar"
                };
                let pct = jc["battery_pct"].as_i64().unwrap_or(0);
                let desc = if mv > 0.0 {
                    format!("{:.2} V, {state}", mv / 1000.0)
                } else {
                    "Este control no reporta su batería".into()
                };
                let mut t = RichText::new(format!("{pct}%"));
                if pct < 15 && !charging(&jc) && mv > 0.0 {
                    t = t.color(Color32::from_rgb(0xe5, 0x48, 0x4d));
                }
                row(ui, "Batería", &desc, Some(t));
                ui.separator();
                let rate = f(&jc["rate_hz"]);
                let desc = if rate > 0.0 && rate < 100.0 {
                    "Menos de los 133 por segundo que puede enviar un Joy-Con. Otro dispositivo Bluetooth o una búsqueda puede estar ocupando la radio.".to_string()
                } else {
                    format!("Pausa más larga en los últimos dos segundos: {:.0} ms", f(&jc["worst_gap_ms"]))
                };
                row(ui, "Entradas", &desc, Some(RichText::new(format!("{rate:.0} por segundo"))));
            }
        });

        if live {
            card(ui, "Movimiento", |ui| {
                self.gyro_graph(ui, side, &jc);
                if ui.button("Recentrar").on_hover_text("Toma la dirección actual como el frente.").clicked() {
                    self.client.action("reset_orientation", json!({}));
                }
            });
            self.mouse_card(ui, st, Some(side));
            if jc["mouse_mode"].as_bool().unwrap_or(false) {
                let d = &jc["mouse_delta"];
                ui.label(
                    RichText::new(format!(
                        "Sensor óptico: X {} Y {} (último movimiento {}, {})",
                        jc["mouse"][0], jc["mouse"][1], d[0], d[1]
                    ))
                    .small()
                    .monospace()
                    .color(ui.visuals().weak_text_color()),
                );
            }
            card(ui, "Probar", |ui| {
                ui.horizontal(|ui| {
                    if ui.button("Vibrar").clicked() {
                        self.client.action("rumble", json!({"side": side, "amp": 0.4, "seconds": 0.35}));
                    }
                    if ui.button("Reproducir un sonido").on_hover_text("Útil para encontrarlo.").clicked() {
                        self.client.action("sample", json!({"side": side, "id": 2}));
                    }
                });
            });
        }

        card(ui, "Detalles", |ui| {
            row(ui, "Número de serie", info["serial"].as_str().unwrap_or("Desconocido"), None);
            ui.separator();
            row(ui, "Firmware", info["firmware"].as_str().unwrap_or("Desconocido"), None);
            ui.separator();
            let mac = if live { jc["mac"].as_str() } else { known["mac"].as_str() };
            row(ui, "Dirección Bluetooth", mac.unwrap_or("Desconocida"), None);
        });

        if live {
            ui.add_space(14.0);
            if ui.button("Desconectar").on_hover_text("Se reconecta cuando presionas uno de sus botones.").clicked() {
                self.client.action("disconnect", json!({"side": side}));
            }
        }
    }

    /// Rolling plot of the rotation rate (degrees per second) about the
    /// three axes, with the current orientation underneath.
    fn gyro_graph(&mut self, ui: &mut egui::Ui, side: &str, jc: &Value) {
        const COUNT: usize = 240;
        let axes = [
            ("Cabeceo", Color32::from_rgb(0xe5, 0x48, 0x4d)),
            ("Alabeo", Color32::from_rgb(0x30, 0xa4, 0x6c)),
            ("Guiñada", Color32::from_rgb(0x3e, 0x7b, 0xfa)),
        ];
        let samples = self.gyro.entry(side.to_string()).or_default();
        let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 96.0), Sense::hover());
        let painter = ui.painter_at(rect);
        let ink = ui.visuals().text_color();
        painter.line_segment(
            [Pos2::new(rect.left(), rect.center().y), Pos2::new(rect.right(), rect.center().y)],
            Stroke::new(1.0, ink.gamma_multiply(0.15)),
        );
        let peak = samples.iter().flat_map(|s| s.iter()).fold(90.0f64, |p, v| p.max(v.abs()));
        let k = (rect.height() / 2.0 - 2.0) / peak as f32;
        for (a, (_, color)) in axes.iter().enumerate() {
            let pts: Vec<Pos2> = samples
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    Pos2::new(
                        rect.left() + i as f32 / (COUNT - 1) as f32 * rect.width(),
                        rect.center().y - s[a] as f32 * k,
                    )
                })
                .collect();
            if pts.len() > 1 {
                painter.add(Shape::line(pts, Stroke::new(1.5, *color)));
            }
        }
        painter.text(
            rect.left_top() + Vec2::new(2.0, 2.0),
            Align2::LEFT_TOP,
            format!("{peak:.0}°/s"),
            FontId::proportional(11.0),
            ui.visuals().weak_text_color(),
        );
        let q = [f(&jc["quat"][0]), f(&jc["quat"][1]), f(&jc["quat"][2]), f(&jc["quat"][3])];
        let ang = angles(q);
        ui.horizontal(|ui| {
            for (i, (name, color)) in axes.iter().enumerate() {
                let (r, _) = ui.allocate_exact_size(Vec2::new(12.0, 10.0), Sense::hover());
                ui.painter().line_segment([r.left_center(), r.right_center()], Stroke::new(2.0, *color));
                ui.label(format!("{name} {:.0}°", ang[i]));
                ui.add_space(8.0);
            }
        });
    }

    fn diagnostics(&mut self, ui: &mut egui::Ui, st: &Value, events: &[Value]) {
        let emus = st["emulated"].as_object().cloned().unwrap_or_default();
        card(ui, "Controles virtuales", |ui| {
            if emus.is_empty() {
                row(ui, "Ninguno", "Aparece uno cuando se conecta un Joy-Con.", None);
            }
            for (i, e) in emus.values().enumerate() {
                if i > 0 {
                    ui.separator();
                }
                let mut nodes: Vec<String> = e["nodes"]["hidraw"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect();
                nodes.extend(
                    e["nodes"]["event"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|v| v["node"].as_str().map(|s| s.replace("/dev/input/", ""))),
                );
                let title = format!(
                    "Nintendo Switch {}{}",
                    e["name"].as_str().unwrap_or("?"),
                    if e["retiring"].as_bool().unwrap_or(false) { " (eliminándose)" } else { "" }
                );
                row(
                    ui,
                    &title,
                    &if nodes.is_empty() { "Aún sin nodos de dispositivo".into() } else { nodes.join(", ") },
                    None,
                );
                let users: Vec<String> = e["consumers"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|c| format!("{} ({})", c["name"].as_str().unwrap_or("?"), c["pid"]))
                    .collect();
                row(
                    ui,
                    "Abierto en",
                    &if users.is_empty() { "Ningún programa".into() } else { users.join(", ") },
                    None,
                );
                row(
                    ui,
                    "Ajustes del host",
                    &format!(
                        "Modo de reporte 0x{}, movimiento {}, vibración {}",
                        e["mode"].as_str().unwrap_or("?"),
                        if e["imu"].as_bool().unwrap_or(false) { "activado" } else { "desactivado" },
                        if e["vibration"].as_bool().unwrap_or(false) { "activada" } else { "desactivada" }
                    ),
                    None,
                );
                row(
                    ui,
                    "Tráfico",
                    &format!("{} reportes enviados, {} comandos respondidos", e["reports"], e["subcommands"]),
                    None,
                );
            }
        });
        card(ui, "Enlaces Bluetooth", |ui| {
            for (i, side) in ["left", "right"].into_iter().enumerate() {
                if i > 0 {
                    ui.separator();
                }
                let j = &st["joycons"][side];
                let desc = if j.is_object() {
                    format!(
                        "Cada {:.1} ms (variación {:.1} ms), {} reportes tardíos, {} recibidos",
                        f(&j["interval_ms"]),
                        f(&j["jitter_ms"]),
                        j["gaps"],
                        j["reports"]
                    )
                } else {
                    "No conectado".into()
                };
                row(ui, side_name(side), &desc, None);
                if let Some(raw) = j["raw"].as_str() {
                    let pretty: Vec<String> =
                        raw.as_bytes().chunks(2).map(|c| String::from_utf8_lossy(c).to_string()).collect();
                    for (n, chunk) in pretty.chunks(16).enumerate() {
                        ui.label(
                            RichText::new(format!("{:02x}: {}", n * 16, chunk.join(" ")))
                                .monospace()
                                .small()
                                .color(ui.visuals().weak_text_color()),
                        );
                    }
                }
            }
        });
        card(ui, "Registro", |ui| {
            if events.is_empty() {
                ui.label("Nada registrado aún");
            }
            for e in events.iter().take(100) {
                let t = e["t"].as_f64().unwrap_or(0.0) as i64;
                let lt = local_hms(t);
                let warn = e["level"] == "warn";
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(lt).monospace().small().color(ui.visuals().weak_text_color()));
                    let text = RichText::new(e["text"].as_str().unwrap_or(""));
                    ui.label(if warn { text.color(Color32::from_rgb(0xf5, 0xa5, 0x24)) } else { text });
                });
            }
        });
    }
}

fn local_hms(t: i64) -> String {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let tt = t as libc::time_t;
    unsafe { libc::localtime_r(&tt, &mut tm) };
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

fn known_for(st: &Value, side: &str) -> Value {
    st["known"]
        .as_object()
        .and_then(|k| {
            k.iter().find(|(_, v)| v["side"] == side).map(|(mac, v)| {
                let mut v = v.clone();
                if v.get("mac").is_none() {
                    v["mac"] = json!(mac);
                }
                v
            })
        })
        .unwrap_or(Value::Null)
}

fn leds_for(st: &Value, side: &str) -> u8 {
    let e = &st["emulated"];
    let d = if e["pro"].is_object() { &e["pro"] } else { &e[side] };
    d["player_leds"].as_u64().unwrap_or(0) as u8
}

fn charging(jc: &Value) -> bool {
    let c = jc["charge"].as_u64().unwrap_or(0);
    c > 0 && c != 0x20
}

fn consumers(st: &Value) -> String {
    let mut names: Vec<String> = Vec::new();
    for e in st["emulated"].as_object().into_iter().flat_map(|m| m.values()) {
        for c in e["consumers"].as_array().into_iter().flatten() {
            if let Some(n) = c["name"].as_str()
                && !names.iter().any(|x| x == n)
            {
                names.push(n.to_string());
            }
        }
    }
    names.join(", ")
}

/// Degrees of tilt about the controller's own axes from its orientation.
fn angles(q: [f64; 4]) -> [f64; 3] {
    let [w, x, y, z] = q;
    [
        (2.0 * (w * x + y * z)).atan2(1.0 - 2.0 * (x * x + y * y)).to_degrees(),
        (2.0 * (w * y - z * x)).clamp(-1.0, 1.0).asin().to_degrees(),
        (2.0 * (w * z + x * y)).atan2(1.0 - 2.0 * (y * y + z * z)).to_degrees(),
    ]
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.show.swap(false, Ordering::SeqCst) {
            self.hidden = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        if ctx.input(|i| i.viewport().close_requested()) && self.has_tray {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            self.hidden = true;
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let (st, events, connected, seq) = {
            let g = self.client.shared.lock();
            (g.state.clone(), g.events.clone(), g.connected, g.seq)
        };
        if seq != self.last_seq {
            self.last_seq = seq;
            if let Some(jcs) = st["joycons"].as_object() {
                for (side, j) in jcs {
                    let g = [f(&j["gyro"][0]), f(&j["gyro"][1]), f(&j["gyro"][2])];
                    let buf = self.gyro.entry(side.clone()).or_default();
                    buf.push_back(g);
                    while buf.len() > 240 {
                        buf.pop_front();
                    }
                }
            }
        }
        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if self.page != Page::Home && ui.button("⏴ Atrás").clicked() {
                    self.page = Page::Home;
                }
                let title = match &self.page {
                    Page::Home => "Joy-Cons".to_string(),
                    Page::JoyCon(s) => side_name(s).to_string(),
                    Page::Diagnostics => "Diagnóstico".to_string(),
                };
                ui.heading(title);
                if self.page == Page::Home {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Diagnóstico").clicked() {
                            self.page = Page::Diagnostics;
                        }
                    });
                }
            });
            ui.add_space(4.0);
        });
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.set_max_width(640.0_f32.min(ui.available_width()));
                match self.page.clone() {
                    Page::Home => self.home(ui, &st, connected),
                    Page::JoyCon(side) => self.joycon_page(ui, &st, &side),
                    Page::Diagnostics => self.diagnostics(ui, &st, &events),
                }
                ui.add_space(16.0);
            });
        });
    }
}

pub fn run(args: &[String]) -> i32 {
    let hidden = args.iter().any(|a| a == "--hidden");
    let icon = crate::art::rasterize(include_str!("../data/joy2con.svg"), 64, 64).map(|(rgba, [w, h])| {
        // the window icon wants straight (not premultiplied) alpha
        let mut rgba = rgba;
        for px in rgba.chunks_mut(4) {
            let a = px[3] as f32 / 255.0;
            if a > 0.0 {
                for c in &mut px[..3] {
                    *c = ((*c as f32 / a).round()).min(255.0) as u8;
                }
            }
        }
        egui::IconData { rgba, width: w as u32, height: h as u32 }
    });
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("joy2con")
        .with_app_id("joy2con")
        .with_inner_size([520.0, 760.0])
        .with_min_inner_size([360.0, 480.0])
        .with_visible(!hidden);
    if let Some(icon) = icon {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    let result = eframe::run_native(
        "joy2con",
        options,
        Box::new(move |cc| {
            let client = Client::start(cc.egui_ctx.clone());
            let show = Arc::new(AtomicBool::new(false));
            let tray = Tray { shared: client.shared.clone(), show: show.clone(), ctx: cc.egui_ctx.clone() };
            let has_tray = {
                use ksni::blocking::TrayMethods;
                match tray.spawn() {
                    Ok(handle) => {
                        // keep the tooltip's battery levels current
                        let h = handle.clone();
                        std::thread::spawn(move || {
                            loop {
                                std::thread::sleep(Duration::from_secs(5));
                                if h.update(|_| {}).is_none() {
                                    break;
                                }
                            }
                        });
                        std::mem::forget(handle);
                        true
                    }
                    Err(e) => {
                        log::warn!("no tray icon: {e}");
                        false
                    }
                }
            };
            Ok(Box::new(App {
                client,
                page: Page::Home,
                textures: HashMap::new(),
                gyro: HashMap::new(),
                last_seq: 0,
                show,
                has_tray,
                hidden,
            }))
        }),
    );
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("joy2con app: {e}");
            1
        }
    }
}
