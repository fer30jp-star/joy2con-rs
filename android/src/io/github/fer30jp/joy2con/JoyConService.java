// SPDX-License-Identifier: GPL-3.0-or-later
package io.github.fer30jp.joy2con;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.bluetooth.BluetoothAdapter;
import android.bluetooth.BluetoothDevice;
import android.bluetooth.BluetoothManager;
import android.bluetooth.le.BluetoothLeScanner;
import android.bluetooth.le.ScanCallback;
import android.bluetooth.le.ScanFilter;
import android.bluetooth.le.ScanRecord;
import android.bluetooth.le.ScanResult;
import android.bluetooth.le.ScanSettings;
import android.content.Context;
import android.content.Intent;
import android.content.SharedPreferences;
import android.content.pm.ServiceInfo;
import android.os.IBinder;
import android.os.SystemClock;

import java.text.SimpleDateFormat;
import java.util.ArrayList;
import java.util.Collections;
import java.util.Date;
import java.util.HashMap;
import java.util.HashSet;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.Set;

/**
 * Keeps the Joy-Cons connected while the app is in the background: scans
 * for Joy-Con 2 advertisements, connects each half and routes halves in
 * mouse mode to the accessibility service.
 */
public class JoyConService extends Service implements JoyConLink.Listener {
    static volatile JoyConService instance;

    static final String PREFS = "joy2con";
    private static final String CHANNEL = "joy2con";

    final Map<String, JoyConLink> links = Collections.synchronizedMap(new HashMap<>());
    private final Set<String> connecting = Collections.synchronizedSet(new HashSet<>());
    final Map<String, Seen> seen = Collections.synchronizedMap(new HashMap<>());
    private final List<String> log = Collections.synchronizedList(new ArrayList<>());

    volatile boolean scanning;
    volatile int attempts, failures;
    volatile String lastError = "";
    volatile long lastErrorT, requestedAt;
    volatile String connectingSide;
    private BluetoothLeScanner scanner;

    static final class Seen {
        String mac, side, kind;
        int rssi;
        long t;
    }

    @Override public IBinder onBind(Intent i) { return null; }

    @Override
    public void onCreate() {
        super.onCreate();
        instance = this;
        NotificationManager nm = getSystemService(NotificationManager.class);
        nm.createNotificationChannel(new NotificationChannel(CHANNEL, "Conexión de los Joy-Con", NotificationManager.IMPORTANCE_LOW));
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        Intent open = new Intent(this, MainActivity.class);
        PendingIntent pi = PendingIntent.getActivity(this, 0, open, PendingIntent.FLAG_IMMUTABLE);
        Notification n = new Notification.Builder(this, CHANNEL)
                .setSmallIcon(R.drawable.ic_joycon)
                .setContentTitle("joy2con")
                .setContentText("Manteniendo los Joy-Con conectados")
                .setContentIntent(pi)
                .setOngoing(true)
                .build();
        startForeground(1, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE);
        connectNow();
        return START_STICKY;
    }

    @Override
    public void onDestroy() {
        stopScan();
        synchronized (links) {
            for (JoyConLink l : links.values()) l.close();
            links.clear();
        }
        instance = null;
        super.onDestroy();
    }

    SharedPreferences prefs() { return getSharedPreferences(PREFS, Context.MODE_PRIVATE); }

    // ------------------------------------------------------------- scanning

    /** "Buscar y conectar": forget recent failures and scan right away. */
    void connectNow() {
        attempts = 0;
        failures = 0;
        lastError = "";
        requestedAt = SystemClock.uptimeMillis();
        log("buscando Joy-Con…", false);
        startScan();
    }

    private void startScan() {
        if (scanning || missingSides().isEmpty()) return;
        BluetoothAdapter a = getSystemService(BluetoothManager.class).getAdapter();
        if (a == null || !a.isEnabled()) {
            log("el Bluetooth está apagado", true);
            return;
        }
        scanner = a.getBluetoothLeScanner();
        if (scanner == null) return;
        ScanFilter f = new ScanFilter.Builder().setManufacturerData(Ns2.NINTENDO, new byte[0]).build();
        ScanSettings s = new ScanSettings.Builder().setScanMode(ScanSettings.SCAN_MODE_LOW_LATENCY).build();
        try {
            scanner.startScan(Collections.singletonList(f), s, scanCallback);
            scanning = true;
        } catch (SecurityException e) {
            log("falta el permiso de dispositivos cercanos", true);
        }
    }

    private void stopScan() {
        if (scanning && scanner != null) {
            try { scanner.stopScan(scanCallback); } catch (Exception ignored) {}
        }
        scanning = false;
    }

    List<String> missingSides() {
        List<String> out = new ArrayList<>();
        for (String s : new String[] {"left", "right"}) if (!links.containsKey(s)) out.add(s);
        return out;
    }

    private final ScanCallback scanCallback = new ScanCallback() {
        @Override
        public void onScanResult(int type, ScanResult result) {
            ScanRecord rec = result.getScanRecord();
            if (rec == null) return;
            byte[] d = rec.getManufacturerSpecificData(Ns2.NINTENDO);
            if (d == null || d.length < 16) return;
            String side = Ns2.sideByPid(Ns2.u16(d, 5));
            if (side == null) return;
            String host = Ns2.macFromLe(d, 10);
            Seen s = new Seen();
            s.mac = result.getDevice().getAddress();
            s.side = side;
            s.kind = host.equals("00:00:00:00:00:00") ? "sync" : "reconnect";
            s.rssi = result.getRssi();
            s.t = SystemClock.uptimeMillis();
            seen.put(s.mac, s);
            if (links.containsKey(side) || connecting.contains(s.mac)) return;
            connect(result.getDevice(), side, s.kind);
        }

        @Override
        public void onScanFailed(int code) {
            scanning = false;
            log("la búsqueda falló (" + code + ")", true);
        }
    };

    private void connect(BluetoothDevice dev, String side, String kind) {
        connecting.add(dev.getAddress());
        attempts++;
        connectingSide = side;
        log((side.equals("left") ? "izquierdo" : "derecho") + (kind.equals("sync") ? " en modo SYNC" : " reconectando") + "; conectando", false);
        String host = prefs().getString("host_mac", "").trim().toUpperCase(Locale.ROOT);
        if (!host.matches("([0-9A-F]{2}:){5}[0-9A-F]{2}")) host = null;
        JoyConLink link = new JoyConLink(this, dev, side, host, this);
        link.start();
    }

    // ------------------------------------------------------------ listeners

    @Override
    public void onReady(JoyConLink link) {
        connecting.remove(link.mac);
        connectingSide = null;
        JoyConLink old = links.put(link.side, link);
        if (old != null && old != link) old.close();
        seen.remove(link.mac);
        log(sideName(link.side) + " conectado (" + link.serial + ", firmware " + link.firmware + ")", false);
        if (missingSides().isEmpty()) stopScan();
    }

    @Override
    public void onLost(JoyConLink link, String why) {
        connecting.remove(link.mac);
        if (link.side.equals(connectingSide)) connectingSide = null;
        boolean wasLinked = links.get(link.side) == link;
        if (wasLinked) links.remove(link.side);
        else {
            failures++;
            lastError = sideName(link.side) + ": " + why;
            lastErrorT = SystemClock.uptimeMillis();
        }
        MouseService m = MouseService.instance;
        if (m != null) m.release(link.side);
        log(sideName(link.side) + ": " + why, !wasLinked);
        startScan();
    }

    @Override
    public void onState(JoyConLink link, Ns2.State s) {
        if (isMouse(link.side)) {
            MouseService m = MouseService.instance;
            if (m != null) m.feed(link.side, s);
        }
    }

    boolean isMouse(String side) { return prefs().getBoolean("mouse_" + side, false); }

    @Override
    public void log(String text, boolean warn) {
        String line = new SimpleDateFormat("HH:mm:ss", Locale.ROOT).format(new Date()) + (warn ? "  ⚠ " : "  ") + text;
        synchronized (log) {
            log.add(0, line);
            while (log.size() > 100) log.remove(log.size() - 1);
        }
    }

    List<String> logLines() {
        synchronized (log) { return new ArrayList<>(log); }
    }

    static String sideName(String side) { return "left".equals(side) ? "Joy-Con izquierdo" : "Joy-Con derecho"; }
}
