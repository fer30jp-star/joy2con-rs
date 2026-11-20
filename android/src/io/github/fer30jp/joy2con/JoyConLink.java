// SPDX-License-Identifier: GPL-3.0-or-later
package io.github.fer30jp.joy2con;

import android.bluetooth.BluetoothDevice;
import android.bluetooth.BluetoothGatt;
import android.bluetooth.BluetoothGattCallback;
import android.bluetooth.BluetoothGattCharacteristic;
import android.bluetooth.BluetoothGattDescriptor;
import android.bluetooth.BluetoothGattService;
import android.bluetooth.BluetoothProfile;
import android.content.Context;
import android.os.SystemClock;

import java.security.SecureRandom;
import java.util.Arrays;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.TimeUnit;

/**
 * One Joy-Con 2 over Android's GATT API. Setup runs as a straight-line
 * script on its own thread: Android allows one outstanding GATT operation at
 * a time, so every operation waits for its callback before the next starts.
 */
final class JoyConLink {
    interface Listener {
        void onState(JoyConLink link, Ns2.State s);
        void onReady(JoyConLink link);
        void onLost(JoyConLink link, String why);
        void log(String text, boolean warn);
    }

    final String mac;
    volatile String side;
    private final Context ctx;
    private final BluetoothDevice dev;
    private final String hostMac; // this phone's Bluetooth address, if the user entered it
    private final Listener listener;

    volatile boolean ready;
    volatile String serial = "", firmware = "";
    volatile boolean paired;
    volatile Ns2.State state = new Ns2.State();
    volatile byte[] lastRaw = new byte[0];
    final Ns2.StickCal cal = new Ns2.StickCal();

    // link statistics
    volatile long reports;
    volatile double intervalMs, rateHz, worstGapMs;
    private long lastT;
    private final double[] gapWindow = new double[256];
    private int gapPos;

    private BluetoothGatt gatt;
    private BluetoothGattCharacteristic input, command, response;
    private final LinkedBlockingQueue<Integer> ops = new LinkedBlockingQueue<>();
    private final CountDownLatch connected = new CountDownLatch(1);
    private final Map<Integer, LinkedBlockingQueue<byte[]>> pending = new ConcurrentHashMap<>();
    private volatile boolean closing;

    JoyConLink(Context ctx, BluetoothDevice dev, String side, String hostMac, Listener listener) {
        this.ctx = ctx;
        this.dev = dev;
        this.mac = dev.getAddress();
        this.side = side;
        this.hostMac = hostMac;
        this.listener = listener;
    }

    void start() {
        Thread t = new Thread(() -> {
            try {
                setup();
                ready = true;
                listener.onReady(this);
            } catch (Exception e) {
                if (!closing) listener.onLost(this, "no se pudo conectar: " + e.getMessage());
                close();
            }
        }, "joycon-" + side);
        t.start();
    }

    void close() {
        closing = true;
        ready = false;
        BluetoothGatt g = gatt;
        if (g != null) {
            try { g.disconnect(); } catch (SecurityException ignored) {}
            try { g.close(); } catch (SecurityException ignored) {}
        }
        gatt = null;
    }

    // ------------------------------------------------------------- GATT ops

    private void waitOp(String what) throws Exception {
        Integer st = ops.poll(5, TimeUnit.SECONDS);
        if (st == null) throw new Exception("sin respuesta a " + what);
        if (st != BluetoothGatt.GATT_SUCCESS) throw new Exception(what + " falló (" + st + ")");
    }

    private void write(BluetoothGattCharacteristic ch, byte[] v, boolean withResponse) throws Exception {
        ops.clear();
        int type = withResponse ? BluetoothGattCharacteristic.WRITE_TYPE_DEFAULT : BluetoothGattCharacteristic.WRITE_TYPE_NO_RESPONSE;
        int rc = gatt.writeCharacteristic(ch, v, type);
        if (rc != BluetoothGatt.GATT_SUCCESS) throw new Exception("escritura rechazada (" + rc + ")");
        waitOp("escritura");
    }

    private void writeDesc(BluetoothGattDescriptor d, byte[] v) throws Exception {
        ops.clear();
        int rc = gatt.writeDescriptor(d, v);
        if (rc != BluetoothGatt.GATT_SUCCESS) throw new Exception("escritura de descriptor rechazada (" + rc + ")");
        waitOp("descriptor");
    }

    private void enableNotify(BluetoothGattCharacteristic ch) throws Exception {
        gatt.setCharacteristicNotification(ch, true);
        BluetoothGattDescriptor d = ch.getDescriptor(Ns2.CCCD);
        if (d != null) writeDesc(d, new byte[] {1, 0});
    }

    /** Send a command on the command characteristic and wait for its response. */
    byte[] command(int cmd, int sub, byte[] data) throws Exception {
        int key = cmd << 8 | sub;
        LinkedBlockingQueue<byte[]> q = new LinkedBlockingQueue<>();
        pending.put(key, q);
        byte[] pkt = new byte[8 + data.length];
        pkt[0] = (byte) cmd; pkt[1] = (byte) 0x91; pkt[2] = 1; pkt[3] = (byte) sub;
        pkt[5] = (byte) data.length;
        System.arraycopy(data, 0, pkt, 8, data.length);
        try {
            for (int attempt = 0; attempt < 2; attempt++) {
                synchronized (this) {
                    write(command, pkt, false);
                }
                byte[] r = q.poll(1500, TimeUnit.MILLISECONDS);
                if (r != null) return r;
            }
            throw new Exception(String.format("sin respuesta al comando %02x/%02x", cmd, sub));
        } finally {
            pending.remove(key);
        }
    }

    byte[] readFlash(int addr, int length) throws Exception {
        byte[] out = new byte[length];
        int got = 0;
        while (got < length) {
            int n = Math.min(0x4F, length - got);
            int a = addr + got;
            byte[] d = {(byte) n, 0x7E, 0, 0, (byte) a, (byte) (a >> 8), (byte) (a >> 16), (byte) (a >> 24)};
            byte[] r = command(0x02, 0x04, d);
            if (r.length < 8 + n) throw new Exception("lectura de flash corta");
            System.arraycopy(r, 8, out, got, n);
            got += n;
        }
        return out;
    }

    void setLeds(int mask) {
        try {
            command(0x09, 0x07, new byte[] {(byte) (mask & 0x0F), 0, 0, 0, 0, 0, 0, 0});
            command(0x09, 0x08, new byte[] {0, 0, 0, 0});
        } catch (Exception ignored) {}
    }

    /** Built-in vibration samples: 3 = connect click, 2 = search tone. */
    void playSample(int id) {
        new Thread(() -> {
            try { command(0x0A, 0x02, new byte[] {(byte) id, 0, 0, 0}); } catch (Exception ignored) {}
        }).start();
    }

    // ---------------------------------------------------------------- setup

    private void setup() throws Exception {
        gatt = dev.connectGatt(ctx, false, callback, BluetoothDevice.TRANSPORT_LE);
        if (gatt == null) throw new Exception("connectGatt devolvió null");
        if (!connected.await(20, TimeUnit.SECONDS)) throw new Exception("tiempo de conexión agotado");
        gatt.requestConnectionPriority(BluetoothGatt.CONNECTION_PRIORITY_HIGH);
        ops.clear();
        if (gatt.requestMtu(512)) {
            ops.poll(3, TimeUnit.SECONDS);
        }
        ops.clear();
        if (!gatt.discoverServices()) throw new Exception("no se pudo descubrir servicios");
        waitOp("descubrimiento de servicios");
        BluetoothGattCharacteristic unknownW = null;
        for (BluetoothGattService svc : gatt.getServices()) {
            for (BluetoothGattCharacteristic ch : svc.getCharacteristics()) {
                if (ch.getUuid().equals(Ns2.INPUT)) input = ch;
                else if (ch.getUuid().equals(Ns2.COMMAND)) command = ch;
                else if (ch.getUuid().equals(Ns2.RESPONSE)) response = ch;
                else if (ch.getUuid().equals(Ns2.UNKNOWN_W)) unknownW = ch;
            }
        }
        if (input == null || command == null || response == null) throw new Exception("no parece un Joy-Con 2 (faltan características)");
        if (unknownW != null) {
            try { write(unknownW, new byte[] {1, 0}, true); } catch (Exception ignored) {}
        }
        enableNotify(response);
        readIdentity();
        if (hostMac != null && !hostMac.isEmpty()) {
            try {
                ensurePaired();
            } catch (Exception e) {
                listener.log(side + ": no se pudo emparejar (" + e.getMessage() + "); esta sesión funciona igual", true);
            }
        }
        command(0x0C, 0x02, new byte[] {(byte) Ns2.FEATURES, 0, 0, 0});
        command(0x0C, 0x04, new byte[] {(byte) Ns2.FEATURES, 0, 0, 0});
        BluetoothGattDescriptor rate = input.getDescriptor(Ns2.RATE_DESC);
        if (rate != null) {
            try { writeDesc(rate, new byte[] {(byte) 0x85, 0}); } catch (Exception ignored) {}
        }
        enableNotify(input);
        setLeds(0x01);
        playSample(3);
    }

    private void readIdentity() throws Exception {
        byte[] fw = command(0x10, 0x01, new byte[0]);
        firmware = fw.length >= 3 ? (fw[0] & 0xFF) + "." + (fw[1] & 0xFF) + "." + (fw[2] & 0xFF) : "?";
        byte[] f = readFlash(0x13000, 0x40);
        int end = 2;
        while (end < 0x12 && f[end] != 0) end++;
        serial = new String(f, 2, end - 2, "UTF-8");
        String s = Ns2.sideByPid(Ns2.u16(f, 0x14));
        if (s != null) side = s;
        byte[] c = readFlash(0x130A8, 9);
        cal.parse(c, 0);
        byte[] user = readFlash(0x1FC040, 11);
        if ((user[0] & 0xFF) == 0xB2 && (user[1] & 0xFF) == 0xA1) cal.parse(user, 2);
    }

    private String[] pairingEntries() throws Exception {
        byte[] raw = readFlash(0x1FA000, 0x40);
        int count = (raw[0] & 0xFF) == 0xFF ? 0 : Math.min(raw[0] & 0xFF, 2);
        String[] out = new String[count];
        for (int i = 0; i < count; i++) {
            StringBuilder sb = new StringBuilder();
            for (int j = 0; j < 6; j++) {
                sb.append(String.format("%02X", raw[0x08 + i * 0x28 + j] & 0xFF));
                if (j < 5) sb.append(':');
            }
            out[i] = sb.toString();
        }
        return out;
    }

    private void ensurePaired() throws Exception {
        String ours = hostMac.toUpperCase();
        String[] parts = ours.split(":");
        StringBuilder rev = new StringBuilder();
        for (int i = parts.length - 1; i >= 0; i--) { rev.append(parts[i]); if (i > 0) rev.append(':'); }
        for (String e : pairingEntries()) {
            if (e.equals(ours) || e.equals(rev.toString())) { paired = true; return; }
        }
        byte[] host = new byte[6];
        for (int i = 0; i < 6; i++) host[i] = (byte) Integer.parseInt(parts[5 - i], 16); // little-endian
        byte[] d = new byte[14];
        d[0] = 0; d[1] = 2;
        System.arraycopy(host, 0, d, 2, 6);
        System.arraycopy(host, 0, d, 8, 6);
        d[8] = (byte) (host[0] - 1); // the console sends addr and addr-1
        command(0x15, 0x01, d);
        byte[] key = Ns2.pairingKey(ours);
        byte[] a1 = new byte[16];
        for (int i = 0; i < 16; i++) a1[i] = (byte) (key[i] ^ Ns2.PAIRING_B1[i]);
        byte[] r = command(0x15, 0x04, concat(new byte[] {0}, a1));
        if (r.length < 17) throw new Exception("respuesta corta");
        byte[] ltk = new byte[16];
        for (int i = 0; i < 16; i++) ltk[i] = (byte) (a1[i] ^ r[1 + i]);
        byte[] a2 = new byte[16];
        new SecureRandom().nextBytes(a2);
        r = command(0x15, 0x02, concat(new byte[] {0}, a2));
        byte[] expect = Ns2.aesEcb(Ns2.reverse(ltk), Ns2.reverse(a2));
        if (r.length < 17 || !Arrays.equals(Arrays.copyOfRange(r, 1, 17), expect)) throw new Exception("la confirmación de la clave no coincide");
        command(0x15, 0x03, new byte[] {0});
        paired = true;
        listener.log(side + ": emparejado con este teléfono", false);
    }

    private static byte[] concat(byte[] a, byte[] b) {
        byte[] out = Arrays.copyOf(a, a.length + b.length);
        System.arraycopy(b, 0, out, a.length, b.length);
        return out;
    }

    // ------------------------------------------------------------ callbacks

    private final BluetoothGattCallback callback = new BluetoothGattCallback() {
        @Override
        public void onConnectionStateChange(BluetoothGatt g, int status, int newState) {
            if (newState == BluetoothProfile.STATE_CONNECTED) {
                connected.countDown();
            } else if (newState == BluetoothProfile.STATE_DISCONNECTED) {
                boolean wasReady = ready;
                ready = false;
                ops.offer(-1);
                if (!closing) {
                    closing = true;
                    try { g.close(); } catch (SecurityException ignored) {}
                    gatt = null;
                    listener.onLost(JoyConLink.this, wasReady ? "desconectado" : "se cortó la conexión (" + status + ")");
                }
            }
        }

        @Override public void onMtuChanged(BluetoothGatt g, int mtu, int status) { ops.offer(status); }
        @Override public void onServicesDiscovered(BluetoothGatt g, int status) { ops.offer(status); }
        @Override public void onCharacteristicWrite(BluetoothGatt g, BluetoothGattCharacteristic c, int status) { ops.offer(status); }
        @Override public void onDescriptorWrite(BluetoothGatt g, BluetoothGattDescriptor d, int status) { ops.offer(status); }

        @Override
        public void onCharacteristicChanged(BluetoothGatt g, BluetoothGattCharacteristic c, byte[] value) {
            if (c.getUuid().equals(Ns2.INPUT)) {
                onReport(value);
            } else if (c.getUuid().equals(Ns2.RESPONSE) && value.length >= 8) {
                LinkedBlockingQueue<byte[]> q = pending.get((value[0] & 0xFF) << 8 | (value[3] & 0xFF));
                if (q != null) q.offer(Arrays.copyOfRange(value, 8, value.length));
            }
        }
    };

    private void onReport(byte[] r) {
        long t = SystemClock.uptimeMillis();
        Ns2.State s = Ns2.decode(r, side, cal, t);
        if (s == null) return;
        if (lastT > 0) {
            double dt = t - lastT;
            if (intervalMs == 0) intervalMs = Math.max(1, dt);
            intervalMs += (Math.min(dt, 3 * intervalMs) - intervalMs) * 0.02;
            intervalMs = Math.max(0.5, intervalMs);
            rateHz = 1000.0 / intervalMs;
            gapWindow[gapPos++ % gapWindow.length] = dt;
            double w = 0;
            for (double v : gapWindow) w = Math.max(w, v);
            worstGapMs = w;
        }
        lastT = t;
        reports++;
        lastRaw = r;
        state = s;
        listener.onState(this, s);
    }
}
