// SPDX-License-Identifier: GPL-3.0-or-later
package io.github.fer30jp.joy2con;

import java.security.MessageDigest;
import java.util.ArrayList;
import java.util.List;
import java.util.UUID;

import javax.crypto.Cipher;
import javax.crypto.spec.SecretKeySpec;

/** Switch 2 Joy-Con protocol constants and report decoding (port of joy2con's ns2). */
final class Ns2 {
    private Ns2() {}

    static final int NINTENDO = 0x0553;
    static final int PID_LEFT = 0x2067;
    static final int PID_RIGHT = 0x2066;

    static final UUID INPUT = UUID.fromString("ab7de9be-89fe-49ad-828f-118f09df7fd2");
    static final UUID COMMAND = UUID.fromString("649d4ac9-8eb7-4e6c-af44-1ea54fe5f005");
    static final UUID RESPONSE = UUID.fromString("c765a961-d9d8-4d36-a20a-5315b111836a");
    static final UUID UNKNOWN_W = UUID.fromString("00c5af5d-1964-4e30-8f51-1956f96bd282");
    static final UUID RATE_DESC = UUID.fromString("679d5510-5a24-4dee-9557-95df80486ecb");
    static final UUID CCCD = UUID.fromString("00002902-0000-1000-8000-00805f9b34fb");

    /** Buttons, sticks, IMU, mouse (0x10), rumble: what the console enables on Joy-Cons. */
    static final int FEATURES = 0x37;

    static final byte[] PAIRING_B1 = hex("5cf6ee792cdf05e1ba2b6325c41a5f10");

    /** name, byte offset, mask */
    static final Object[][] BUTTONS = {
        {"Y", 4, 0x01}, {"X", 4, 0x02}, {"B", 4, 0x04}, {"A", 4, 0x08},
        {"SR_R", 4, 0x10}, {"SL_R", 4, 0x20}, {"R", 4, 0x40}, {"ZR", 4, 0x80},
        {"MINUS", 5, 0x01}, {"PLUS", 5, 0x02}, {"RSTICK", 5, 0x04}, {"LSTICK", 5, 0x08},
        {"HOME", 5, 0x10}, {"CAPTURE", 5, 0x20}, {"C", 5, 0x40},
        {"DOWN", 6, 0x01}, {"UP", 6, 0x02}, {"RIGHT", 6, 0x04}, {"LEFT", 6, 0x08},
        {"SR_L", 6, 0x10}, {"SL_L", 6, 0x20}, {"L", 6, 0x40}, {"ZL", 6, 0x80},
    };

    static String sideByPid(int pid) {
        if (pid == PID_LEFT) return "left";
        if (pid == PID_RIGHT) return "right";
        return null;
    }

    static byte[] hex(String s) {
        byte[] out = new byte[s.length() / 2];
        for (int i = 0; i < out.length; i++) out[i] = (byte) Integer.parseInt(s.substring(2 * i, 2 * i + 2), 16);
        return out;
    }

    static int u16(byte[] b, int o) { return (b[o] & 0xFF) | (b[o + 1] & 0xFF) << 8; }
    static int s16(byte[] b, int o) { return (short) u16(b, o); }
    static long u32(byte[] b, int o) { return (u16(b, o) | (long) u16(b, o + 2) << 16) & 0xFFFFFFFFL; }

    static int[] u12Pair(byte[] b, int o) {
        int a = (b[o] & 0xFF) | (b[o + 1] & 0x0F) << 8;
        int c = (b[o + 1] & 0xFF) >> 4 | (b[o + 2] & 0xFF) << 4;
        return new int[] {a, c};
    }

    /** Stick calibration for one axis: neutral and the reach to each end. */
    static final class Axis {
        int neutral = 2048, max = 1500, min = 1500;
        double norm(int raw) {
            int d = raw - neutral;
            return (double) d / (d >= 0 ? max : min);
        }
    }

    static final class StickCal {
        final Axis x = new Axis(), y = new Axis();

        /** Parses 9 bytes of packed 12-bit values; returns false if they look unset. */
        boolean parse(byte[] b, int o) {
            if (b.length < o + 9) return false;
            int[] n = u12Pair(b, o), mx = u12Pair(b, o + 3), mn = u12Pair(b, o + 6);
            for (int v : new int[] {n[0], n[1], mx[0], mx[1], mn[0], mn[1]}) if (v == 0) return false;
            if (n[0] == 0xFFF || n[1] == 0xFFF || mx[0] == 0xFFF) return false;
            x.neutral = n[0]; x.max = mx[0]; x.min = mn[0];
            y.neutral = n[1]; y.max = mx[1]; y.min = mn[1];
            return true;
        }
    }

    /** One decoded input report. */
    static final class State {
        long t;                 // SystemClock.uptimeMillis of arrival
        int b4, b5, b6;
        double sx, sy;          // calibrated stick, -1..1, Y up
        int[] mouse = new int[4];
        int batteryMv, charge;
        double[] accel = new double[3]; // g
        double[] gyro = new double[3];  // deg/s (uncalibrated scale)
        boolean imu;

        boolean is(String name) {
            for (Object[] b : BUTTONS) {
                if (b[0].equals(name)) {
                    int o = (Integer) b[1], m = (Integer) b[2];
                    int v = o == 4 ? b4 : o == 5 ? b5 : b6;
                    return (v & m) != 0;
                }
            }
            return false;
        }

        List<String> pressed() {
            List<String> out = new ArrayList<>();
            for (Object[] b : BUTTONS) if (is((String) b[0])) out.add((String) b[0]);
            return out;
        }
    }

    static State decode(byte[] r, String side, StickCal cal, long t) {
        if (r.length < 0x3C) return null;
        State s = new State();
        s.t = t;
        s.b4 = r[4] & 0xFF; s.b5 = r[5] & 0xFF; s.b6 = r[6] & 0xFF;
        int[] raw = "right".equals(side) ? u12Pair(r, 13) : u12Pair(r, 10);
        s.sx = clamp(cal.x.norm(raw[0]), -1.25, 1.25);
        s.sy = clamp(cal.y.norm(raw[1]), -1.25, 1.25);
        for (int i = 0; i < 4; i++) s.mouse[i] = u16(r, 0x10 + 2 * i);
        s.batteryMv = u16(r, 0x1F);
        s.charge = r[0x21] & 0xFF;
        if (u32(r, 0x2A) != 0) {
            for (int i = 0; i < 3; i++) {
                s.accel[i] = s16(r, 0x30 + 2 * i) / 4096.0;
                s.gyro[i] = Math.toDegrees(s16(r, 0x36 + 2 * i) * 34.8 / 32767.0);
            }
            s.imu = true;
        }
        return s;
    }

    static double clamp(double v, double lo, double hi) { return Math.max(lo, Math.min(hi, v)); }

    private static final int[][] DISCHARGE = {
        {3300, 0}, {3450, 5}, {3600, 10}, {3680, 20}, {3730, 30}, {3770, 40},
        {3810, 50}, {3860, 60}, {3920, 70}, {3990, 80}, {4070, 90}, {4150, 100},
    };

    static int batteryPercent(int mv) {
        if (mv <= DISCHARGE[0][0]) return 0;
        for (int i = 1; i < DISCHARGE.length; i++) {
            int[] a = DISCHARGE[i - 1], b = DISCHARGE[i];
            if (mv <= b[0]) return (int) Math.round(a[1] + (b[1] - a[1]) * (double) (mv - a[0]) / (b[0] - a[0]));
        }
        return 100;
    }

    /** The joy2con pairing key for a host address, the same one the Linux driver uses. */
    static byte[] pairingKey(String host) {
        try {
            byte[] d = MessageDigest.getInstance("SHA-256").digest(("joy2con pairing key " + host).getBytes("UTF-8"));
            byte[] k = new byte[16];
            System.arraycopy(d, 0, k, 0, 16);
            return k;
        } catch (Exception e) {
            throw new RuntimeException(e);
        }
    }

    static byte[] aesEcb(byte[] key, byte[] block) throws Exception {
        Cipher c = Cipher.getInstance("AES/ECB/NoPadding");
        c.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(key, "AES"));
        return c.doFinal(block);
    }

    static byte[] reverse(byte[] b) {
        byte[] out = new byte[b.length];
        for (int i = 0; i < b.length; i++) out[i] = b[b.length - 1 - i];
        return out;
    }

    /** "AA:BB:..." from little-endian bytes. */
    static String macFromLe(byte[] b, int o) {
        StringBuilder sb = new StringBuilder();
        for (int i = 5; i >= 0; i--) {
            sb.append(String.format("%02X", b[o + i] & 0xFF));
            if (i > 0) sb.append(':');
        }
        return sb.toString();
    }

    static String hexString(byte[] b, int max) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < Math.min(b.length, max); i++) sb.append(String.format("%02x", b[i] & 0xFF));
        return sb.toString();
    }
}
