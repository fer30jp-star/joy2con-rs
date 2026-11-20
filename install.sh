#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Builds and installs joy2con for the current user, without root:
#   ~/.local/bin/joy2con                     the program (driver, app, tools)
#   ~/.config/systemd/user/joy2con.service   the driver as a user service
#   ~/.local/share/applications/joy2con.desktop and the icon
#
#   ./install.sh               build, install or update, restart the driver
#   ./install.sh --uninstall   remove all of the above (keeps ~/.config/joy2con)
set -euo pipefail

src=$(cd "$(dirname "$0")" && pwd)
data=${XDG_DATA_HOME:-$HOME/.local/share}
config=${XDG_CONFIG_HOME:-$HOME/.config}
bin=$HOME/.local/bin/joy2con
unit=$config/systemd/user/joy2con.service
desktop=$data/applications/joy2con.desktop
icon=$data/icons/hicolor/scalable/apps/joy2con.svg

say() { printf '\033[1m%s\033[0m\n' "$*"; }
warn() { printf '\033[33mwarning:\033[0m %s\n' "$*"; }

if [[ ${1:-} == --uninstall ]]; then
    systemctl --user disable --now joy2con.service 2>/dev/null || true
    rm -f "$bin" "$unit" "$desktop" "$icon"
    systemctl --user daemon-reload 2>/dev/null || true
    say "joy2con removed. Your settings in $config/joy2con were kept."
    exit 0
fi

say "Building joy2con (release)"
cargo build --release --manifest-path "$src/Cargo.toml"

was_active=false
systemctl --user is-active --quiet joy2con.service && was_active=true

mkdir -p "$(dirname "$bin")" "$(dirname "$unit")" "$(dirname "$desktop")" "$(dirname "$icon")"
rm -f "$bin"   # may be a symlink to an older (Python) joy2con
install -m755 "$src/target/release/joy2con" "$bin"
install -m644 "$src/data/joy2con.service" "$unit"
install -m644 "$src/data/joy2con.desktop" "$desktop"
install -m644 "$src/data/joy2con.svg" "$icon"
systemctl --user daemon-reload

if $was_active; then
    systemctl --user restart joy2con.service
    say "Driver restarted."
fi

if [[ ! -w /dev/uhid ]] || [[ ! -w /dev/uinput ]]; then
    warn "you can't write to /dev/uhid or /dev/uinput yet (needed for the virtual controller and mouse mode). Run once:"
    echo "  sudo install -Dm644 $src/data/70-joy2con.rules /etc/udev/rules.d/70-joy2con.rules"
    echo "  sudo udevadm control --reload && sudo udevadm trigger"
fi

say "Done. Next:"
echo "  joy2con setup                           # one-time Bluetooth settings (add --interval-ms 15 for picky adapters)"
echo "  systemctl --user enable --now joy2con   # start the driver now and at every login"
echo "  joy2con                                 # open the app"
