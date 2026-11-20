#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# One-time system setup for joy2con. Run as root (pkexec or sudo).
# Usage: setup-bluetooth.sh [INTERVAL_UNITS]   (1.25 ms units; 6 = 7.5 ms, 12 = 15 ms)
#
# 1. LE connection interval. The Joy-Con 2 never asks for a faster interval
#    itself, so the host default decides its report rate: BlueZ's default
#    (30-50 ms) gives ~20 reports/s, 7.5 ms gives ~133. Some adapters (e.g.
#    UGREEN BT5.4) are more stable at 15 ms.
# 2. BlueZ experimental interfaces.
#
# A copy of the original file is kept at main.conf.joy2con.bak.
set -euo pipefail

units=${1:-6}
[[ $units =~ ^[0-9]+$ ]] || { echo "interval must be a number of 1.25 ms units" >&2; exit 1; }
conf=/etc/bluetooth/main.conf
[[ -f $conf.joy2con.bak ]] || cp -a "$conf" "$conf.joy2con.bak"

set_key() {  # set_key SECTION KEY VALUE: uncomment or replace KEY, else add it to [SECTION]
    local section=$1 key=$2 value=$3
    if grep -qE "^#?\s*$key\s*=" "$conf"; then
        sed -i -E "s|^#?\s*$key\s*=.*|$key = $value|" "$conf"
    elif grep -qE "^\[$section\]" "$conf"; then
        sed -i -E "/^\[$section\]/a $key = $value" "$conf"
    else
        printf '\n[%s]\n%s = %s\n' "$section" "$key" "$value" >> "$conf"
    fi
}

set_key General Experimental true
set_key LE MinConnectionInterval "$units"
set_key LE MaxConnectionInterval "$units"
set_key LE ConnectionLatency 0

systemctl restart bluetooth
echo "joy2con: Bluetooth configured:"
grep -nE '^(Experimental|MinConnectionInterval|MaxConnectionInterval|ConnectionLatency)\b' "$conf"
