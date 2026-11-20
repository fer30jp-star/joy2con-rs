// SPDX-License-Identifier: GPL-3.0-or-later
//! joy2con: Switch 2 Joy-Con driver and app for Linux (Rust port).
//!
//! Usage:
//!   joy2con                 open the app (same as `joy2con app`)
//!   joy2con app [--hidden]  open the app; --hidden starts in the tray
//!   joy2con daemon [-v]     run the driver (normally the systemd user service)
//!   joy2con status          print what the driver sees
//!   joy2con setup [--interval-ms 7.5|15] | --restore
//!                           one-time Bluetooth settings (asks for your password)

mod app;
mod art;
mod att;
mod daemon;
mod mouse;
mod ns2;
mod server;
mod switch1;
mod uhid;
mod util;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

const SETUP_SCRIPT: &str = include_str!("../data/setup-bluetooth.sh");

fn status() -> i32 {
    let Ok(s) = UnixStream::connect(util::socket_path()) else {
        println!("joy2con driver is not running (systemctl --user start joy2con)");
        return 1;
    };
    for line in BufReader::new(s).lines().map_while(Result::ok) {
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        if msg["type"] != "state" {
            continue;
        }
        let st = &msg["state"];
        println!("mode: {}, scanning: {}", st["mode"].as_str().unwrap_or("?"), st["scanning"].as_str().unwrap_or("?"));
        if let Some(jcs) = st["joycons"].as_object() {
            for (side, j) in jcs {
                println!(
                    "  {side}: {} {:.0} Hz, battery {}%, {}{}",
                    j["mac"].as_str().unwrap_or("?"),
                    j["rate_hz"].as_f64().unwrap_or(0.0),
                    j["battery_pct"],
                    if j["info"]["paired"].as_bool().unwrap_or(false) { "paired" } else { "not paired" },
                    if j["mouse_mode"].as_bool().unwrap_or(false) { ", mouse mode" } else { "" },
                );
            }
        }
        if let Some(emus) = st["emulated"].as_object() {
            for e in emus.values() {
                let users: Vec<&str> =
                    e["consumers"].as_array().into_iter().flatten().filter_map(|c| c["name"].as_str()).collect();
                println!(
                    "  emulated {}: used by {}",
                    e["name"].as_str().unwrap_or("?"),
                    if users.is_empty() { "nobody".to_string() } else { users.join(", ") }
                );
            }
        }
        return 0;
    }
    1
}

fn setup(args: &[String]) -> i32 {
    let interval = args
        .iter()
        .position(|a| a == "--interval-ms")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(7.5);
    // BlueZ counts the LE connection interval in 1.25 ms units.
    let units = if args.iter().any(|a| a == "--restore") {
        "restore".to_string()
    } else {
        ((interval / 1.25).round() as i64).clamp(6, 3200).to_string()
    };
    let path = std::env::temp_dir().join(format!("joy2con-setup-{}.sh", std::process::id()));
    if let Err(e) = std::fs::File::create(&path).and_then(|mut f| f.write_all(SETUP_SCRIPT.as_bytes())) {
        eprintln!("could not write the setup script: {e}");
        return 1;
    }
    let code = std::process::Command::new("pkexec")
        .arg("bash")
        .arg(&path)
        .arg(&units)
        .status()
        .map(|s| s.code().unwrap_or(1))
        .unwrap_or_else(|e| {
            eprintln!("pkexec: {e}");
            1
        });
    let _ = std::fs::remove_file(&path);
    code
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, rest) = match args.first() {
        Some(a) if !a.starts_with('-') => (a.as_str(), args[1..].to_vec()),
        _ => ("app", args.clone()),
    };
    let code = match cmd {
        "daemon" => match daemon::run(rest.iter().any(|a| a == "-v" || a == "--verbose")) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("joy2con daemon: {e}");
                1
            }
        },
        "status" => status(),
        "setup" => setup(&rest),
        "app" | "ui" => app::run(&rest),
        _ => {
            println!("{}", include_str!("usage.txt"));
            2
        }
    };
    std::process::exit(code);
}
