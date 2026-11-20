// SPDX-License-Identifier: GPL-3.0-or-later
//! Original line-art drawings of the Switch 2 Joy-Cons.
//!
//! Flat diagram style: one stroke weight on a plain fill. Each Joy-Con is
//! drawn in layers so the app can put pressed-button highlights between them:
//! `fill_svg` (body and shoulder fill) and `lines_svg` (outlines, symbols,
//! accents); `shoulder_svg` draws one shoulder button's shape for its
//! highlight. `buttons` gives each other control's position in the shared
//! 160 x 440 space; `STICK` each stick's centre, travel and cap radius; `LEDS`
//! the player LED positions on the attaching edge.

pub const W: f32 = 160.0;
pub const H: f32 = 440.0;
const STROKE: f32 = 1.7;

#[derive(Clone, Copy)]
pub enum Shape {
    Circle,
    Rect,
}

/// name, shape, cx, cy, a, b: circle of radius a, or rounded rect a x b.
pub type Button = (&'static str, Shape, f32, f32, f32, f32);

pub fn buttons(side: &str) -> &'static [Button] {
    use Shape::*;
    const LEFT: &[Button] = &[
        ("MINUS", Rect, 62.0, 74.0, 20.0, 8.0),
        ("LSTICK", Circle, 76.0, 140.0, 24.0, 0.0),
        ("UP", Circle, 76.0, 222.0, 14.0, 0.0),
        ("DOWN", Circle, 76.0, 294.0, 14.0, 0.0),
        ("LEFT", Circle, 40.0, 258.0, 14.0, 0.0),
        ("RIGHT", Circle, 112.0, 258.0, 14.0, 0.0),
        ("CAPTURE", Rect, 110.0, 346.0, 22.0, 22.0),
        ("SL_L", Rect, 142.0, 120.0, 8.0, 46.0),
        ("SR_L", Rect, 142.0, 336.0, 8.0, 46.0),
    ];
    const RIGHT: &[Button] = &[
        ("PLUS", Rect, 48.0, 74.0, 20.0, 20.0),
        ("X", Circle, 90.0, 92.0, 14.0, 0.0),
        ("A", Circle, 122.0, 124.0, 14.0, 0.0),
        ("B", Circle, 90.0, 156.0, 14.0, 0.0),
        ("Y", Circle, 58.0, 124.0, 14.0, 0.0),
        ("RSTICK", Circle, 96.0, 240.0, 24.0, 0.0),
        ("HOME", Rect, 58.0, 322.0, 22.0, 22.0),
        ("C", Rect, 102.0, 352.0, 22.0, 22.0),
        ("SL_R", Rect, 18.0, 120.0, 8.0, 46.0),
        ("SR_R", Rect, 18.0, 336.0, 8.0, 46.0),
    ];
    if side == "left" { LEFT } else { RIGHT }
}

fn button(side: &str, name: &str) -> Button {
    *buttons(side).iter().find(|b| b.0 == name).expect("button")
}

/// centre x, y, travel, cap radius
pub fn stick(side: &str) -> (f32, f32, f32, f32) {
    if side == "left" { (76.0, 140.0, 12.0, 24.0) } else { (96.0, 240.0, 12.0, 24.0) }
}

pub fn leds(side: &str) -> (f32, [f32; 4]) {
    let ys = [214.0, 228.0, 242.0, 256.0];
    if side == "left" { (142.0, ys) } else { (18.0, ys) }
}

pub fn shoulders(side: &str) -> [&'static str; 2] {
    if side == "left" { ["L", "ZL"] } else { ["R", "ZR"] }
}

// thickness, full until, gone at (fractions of the edge): front band, back band
const BAND: [(f32, f32, f32); 2] = [(7.0, 0.50, 0.88), (5.0, 0.36, 0.64)];

fn hexc(c: Option<&str>, fallback: &str) -> String {
    match c {
        Some(c) if c.len() == 7 && c.starts_with('#') => c.to_string(),
        _ => fallback.to_string(),
    }
}

/// Silhouette: semicircular outer ends, a straight attaching edge.
pub fn body_path(side: &str) -> String {
    if side == "left" {
        "M146,34 L146,426 Q146,434 138,434 L80,434 C40,434 14,404 14,366 L14,102 C14,64 40,26 80,26 L138,26 Q146,26 146,34 Z".into()
    } else {
        "M14,34 L14,426 Q14,434 22,434 L80,434 C120,434 146,404 146,366 L146,102 C146,64 120,26 80,26 L22,26 Q14,26 14,34 Z".into()
    }
}

/// Points along the body's top edge, from the attaching edge round to the
/// outer side, evenly spaced: (x, y, offset direction x, offset direction y).
fn top_edge(side: &str, n: usize) -> Vec<(f32, f32, f32, f32)> {
    let quad = |p0: (f32, f32), c: (f32, f32), p1: (f32, f32), t: f32| {
        let u = 1.0 - t;
        (u * u * p0.0 + 2.0 * u * t * c.0 + t * t * p1.0, u * u * p0.1 + 2.0 * u * t * c.1 + t * t * p1.1)
    };
    let cubic = |p0: (f32, f32), c0: (f32, f32), c1: (f32, f32), p1: (f32, f32), t: f32| {
        let u = 1.0 - t;
        let f =
            |a: f32, b: f32, c: f32, d: f32| u * u * u * a + 3.0 * u * u * t * b + 3.0 * u * t * t * c + t * t * t * d;
        (f(p0.0, c0.0, c1.0, p1.0), f(p0.1, c0.1, c1.1, p1.1))
    };
    let mut raw: Vec<(f32, f32, bool)> = Vec::new();
    for i in 0..20 {
        let (x, y) = quad((146.0, 34.0), (146.0, 26.0), (138.0, 26.0), i as f32 / 20.0);
        raw.push((x, y, true));
    }
    for i in 0..20 {
        raw.push((138.0 - 58.0 * i as f32 / 20.0, 26.0, false));
    }
    for i in 0..60 {
        let (x, y) = cubic((80.0, 26.0), (40.0, 26.0), (14.0, 64.0), (14.0, 102.0), i as f32 / 60.0);
        raw.push((x, y, false));
    }
    for i in 0..21 {
        raw.push((14.0, 102.0 + 48.0 * i as f32 / 20.0, false));
    }
    if side == "right" {
        for p in &mut raw {
            p.0 = 160.0 - p.0;
        }
    }
    let mut dist = vec![0.0f32];
    for w in raw.windows(2) {
        let d = ((w[1].0 - w[0].0).powi(2) + (w[1].1 - w[0].1).powi(2)).sqrt();
        dist.push(dist.last().unwrap() + d);
    }
    let total = *dist.last().unwrap();
    let mut out = Vec::with_capacity(n + 1);
    let mut j = 0;
    for k in 0..=n {
        let target = total * k as f32 / n as f32;
        while j < raw.len() - 2 && dist[j + 1] < target {
            j += 1;
        }
        let f = (target - dist[j]) / (dist[j + 1] - dist[j]).max(1e-9);
        let x = raw[j].0 + (raw[j + 1].0 - raw[j].0) * f;
        let y = raw[j].1 + (raw[j + 1].1 - raw[j].1) * f;
        let (mut tx, mut ty) = (raw[j + 1].0 - raw[j].0, raw[j + 1].1 - raw[j].1);
        let ln = (tx * tx + ty * ty).sqrt().max(1e-9);
        tx /= ln;
        ty /= ln;
        let (nx, ny) = if raw[j].2 {
            (0.0, -1.0)
        } else if side == "left" {
            (-ty, tx)
        } else {
            (ty, -tx)
        };
        out.push((x, y, nx, ny));
    }
    out
}

fn thickness(k: f32, (full, until, gone): (f32, f32, f32)) -> f32 {
    if k <= until {
        return full;
    }
    if k >= gone {
        return 0.0;
    }
    let f = (k - until) / (gone - until);
    full * (1.0 - f * f * (3.0 - 2.0 * f))
}

/// Outline of one shoulder button's visible band.
pub fn shoulder_path(side: &str, name: &str) -> String {
    let edge = top_edge(side, 90);
    let n = (edge.len() - 1) as f32;
    let mut inner = Vec::new();
    let mut outer = Vec::new();
    for (k, (x, y, nx, ny)) in edge.iter().enumerate() {
        let f = k as f32 / n;
        let w0 = thickness(f, BAND[0]);
        let (lo, hi) = if name == "L" || name == "R" { (0.0, w0) } else { (w0, w0 + thickness(f, BAND[1])) };
        inner.push((x + nx * lo, y + ny * lo));
        outer.push((x + nx * hi, y + ny * hi));
    }
    let pts: Vec<String> = outer.iter().chain(inner.iter().rev()).map(|(x, y)| format!("{x:.1},{y:.1}")).collect();
    format!("M{} Z", pts.join(" L"))
}

fn svg(body: &[String]) -> String {
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {W} {H}\" width=\"{W}\" height=\"{H}\">\n{}\n</svg>",
        body.join("\n")
    )
}

pub fn shoulder_svg(side: &str, name: &str, color: Option<&str>) -> String {
    let color = hexc(color, "#ffffff");
    svg(&[format!("<path d=\"{}\" fill=\"{color}\"/>", shoulder_path(side, name))])
}

pub fn fill_svg(side: &str, fill: Option<&str>) -> String {
    let fill = hexc(fill, "#2a2c30");
    let mut g: Vec<String> =
        shoulders(side).iter().map(|n| format!("<path d=\"{}\" fill=\"{fill}\"/>", shoulder_path(side, n))).collect();
    g.push(format!("<path d=\"{}\" fill=\"{fill}\"/>", body_path(side)));
    svg(&g)
}

/// The silhouette traced in a heavier stroke, shown while the pointer is on it.
pub fn outline_svg(side: &str, color: Option<&str>) -> String {
    let color = hexc(color, "#ffffff");
    let stroke =
        format!("fill=\"none\" stroke=\"{color}\" stroke-width=\"{}\" stroke-linejoin=\"round\"", STROKE * 1.9);
    let mut g: Vec<String> =
        shoulders(side).iter().rev().map(|n| format!("<path d=\"{}\" {stroke}/>", shoulder_path(side, n))).collect();
    g.push(format!("<path d=\"{}\" {stroke}/>", body_path(side)));
    svg(&g)
}

pub fn lines_svg(side: &str, line: Option<&str>, accent: Option<&str>) -> String {
    let line = hexc(line, "#c8cacf");
    let accent = hexc(accent, if side == "left" { "#9be1e6" } else { "#ff8c5f" });
    let stroke = format!("fill=\"none\" stroke=\"{line}\" stroke-width=\"{STROKE}\" stroke-linejoin=\"round\"");
    let mut g = Vec::new();
    for n in shoulders(side).iter().rev() {
        g.push(format!("<path d=\"{}\" {stroke}/>", shoulder_path(side, n)));
    }
    // attaching edge: the accent strip with the player LED windows
    let sx = if side == "left" { 138 } else { 14 };
    g.push(format!(
        "<rect x=\"{sx}\" y=\"36\" width=\"8\" height=\"388\" rx=\"3\" fill=\"{accent}\" fill-opacity=\"0.9\"/>"
    ));
    let (lx, ys) = leds(side);
    for y in ys {
        g.push(format!(
            "<rect x=\"{}\" y=\"{}\" width=\"4\" height=\"6\" rx=\"1\" fill=\"#000\" fill-opacity=\"0.45\"/>",
            lx - 2.0,
            y - 3.0
        ));
    }
    g.push(format!("<path d=\"{}\" {stroke}/>", body_path(side)));

    let circle = |g: &mut Vec<String>, name: &str, text: Option<&str>| -> (f32, f32) {
        let (_, _, cx, cy, r, _) = button(side, name);
        g.push(format!("<circle cx=\"{cx}\" cy=\"{cy}\" r=\"{r}\" {stroke}/>"));
        if let Some(t) = text {
            g.push(format!(
                "<text x=\"{cx}\" y=\"{}\" font-family=\"sans-serif\" font-size=\"12.5\" fill=\"{line}\" text-anchor=\"middle\">{t}</text>",
                cy + 4.3
            ));
        }
        (cx, cy)
    };
    let square = |g: &mut Vec<String>, name: &str| -> (f32, f32) {
        let (_, _, cx, cy, w, _) = button(side, name);
        g.push(format!(
            "<rect x=\"{}\" y=\"{}\" width=\"{w}\" height=\"{w}\" rx=\"5\" {stroke}/>",
            cx - w / 2.0,
            cy - w / 2.0
        ));
        (cx, cy)
    };

    // stick collar in the accent colour; the moving cap is drawn by the app
    let (scx, scy, _, cap) = stick(side);
    g.push(format!(
        "<circle cx=\"{scx}\" cy=\"{scy}\" r=\"{}\" fill=\"none\" stroke=\"{accent}\" stroke-width=\"2.6\"/>",
        cap + 7.0
    ));
    g.push(format!("<circle cx=\"{scx}\" cy=\"{scy}\" r=\"{}\" {stroke} stroke-opacity=\"0.35\"/>", cap + 10.5));

    if side == "left" {
        let (_, _, cx, cy, _, _) = button("left", "MINUS");
        g.push(format!(
            "<rect x=\"{}\" y=\"{}\" width=\"14\" height=\"3\" rx=\"1.5\" fill=\"{line}\"/>",
            cx - 7.0,
            cy - 1.5
        ));
        for (name, d) in [
            ("UP", "M0,-5 L4.6,3 L-4.6,3 Z"),
            ("DOWN", "M0,5 L4.6,-3 L-4.6,-3 Z"),
            ("LEFT", "M-5,0 L3,-4.6 L3,4.6 Z"),
            ("RIGHT", "M5,0 L-3,-4.6 L-3,4.6 Z"),
        ] {
            let (cx, cy) = circle(&mut g, name, None);
            g.push(format!("<path transform=\"translate({cx},{cy})\" d=\"{d}\" fill=\"{line}\"/>"));
        }
        let (cx, cy) = square(&mut g, "CAPTURE");
        g.push(format!("<circle cx=\"{cx}\" cy=\"{cy}\" r=\"5\" {stroke}/>"));
    } else {
        let (_, _, cx, cy, _, _) = button("right", "PLUS");
        g.push(format!(
            "<path transform=\"translate({cx},{cy})\" d=\"M-7,-1.5 h5.5 v-5.5 h3 v5.5 h5.5 v3 h-5.5 v5.5 h-3 v-5.5 h-5.5 Z\" fill=\"{line}\"/>"
        ));
        for name in ["X", "A", "B", "Y"] {
            circle(&mut g, name, Some(name));
        }
        let (cx, cy) = square(&mut g, "HOME");
        g.push(format!(
            "<path transform=\"translate({cx},{cy})\" d=\"M0,-6 L6,0 L4,0 L4,5 L-4,5 L-4,0 L-6,0 Z\" {stroke}/>"
        ));
        let (cx, cy) = square(&mut g, "C");
        g.push(format!(
            "<path transform=\"translate({cx},{cy})\" d=\"M4.2,-3.8 A5.6,5.6 0 1 0 4.2,3.8\" {stroke} stroke-linecap=\"round\"/>"
        ));
    }
    svg(&g)
}

/// Rasterize an SVG to premultiplied RGBA at the given pixel size.
pub fn rasterize(svg: &str, width: u32, height: u32) -> Option<(Vec<u8>, [usize; 2])> {
    use resvg::{tiny_skia, usvg};
    static FONTS: std::sync::OnceLock<std::sync::Arc<usvg::fontdb::Database>> = std::sync::OnceLock::new();
    let fonts = FONTS.get_or_init(|| {
        let mut db = usvg::fontdb::Database::new();
        db.load_system_fonts();
        std::sync::Arc::new(db)
    });
    let opt = usvg::Options { fontdb: fonts.clone(), ..Default::default() };
    let tree = usvg::Tree::from_str(svg, &opt).ok()?;
    let mut pixmap = tiny_skia::Pixmap::new(width.max(1), height.max(1))?;
    let size = tree.size();
    let t = tiny_skia::Transform::from_scale(width as f32 / size.width(), height as f32 / size.height());
    resvg::render(&tree, t, &mut pixmap.as_mut());
    Some((pixmap.data().to_vec(), [width as usize, height as usize]))
}
