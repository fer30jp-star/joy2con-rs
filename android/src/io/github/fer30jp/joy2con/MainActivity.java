// SPDX-License-Identifier: GPL-3.0-or-later
package io.github.fer30jp.joy2con;

import android.Manifest;
import android.app.Activity;
import android.content.Context;
import android.content.Intent;
import android.content.SharedPreferences;
import android.content.pm.PackageManager;
import android.content.res.Configuration;
import android.graphics.Canvas;
import android.graphics.Color;
import android.graphics.Paint;
import android.graphics.Typeface;
import android.graphics.drawable.GradientDrawable;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.os.SystemClock;
import android.provider.Settings;
import android.text.Editable;
import android.text.TextWatcher;
import android.view.View;
import android.view.ViewGroup;
import android.widget.Button;
import android.widget.EditText;
import android.widget.LinearLayout;
import android.widget.RadioButton;
import android.widget.RadioGroup;
import android.widget.ScrollView;
import android.widget.SeekBar;
import android.widget.Switch;
import android.widget.TextView;

import java.util.List;

public class MainActivity extends Activity {
    private final Handler ui = new Handler(Looper.getMainLooper());
    private LinearLayout root;
    private TextView statusTitle, statusDetail, logView, a11yStatus;
    private View statusDot;
    private final Side[] sides = new Side[2];
    private boolean dark;
    private int fg, weak, card;

    private final class Side {
        final String name;
        TextView title, info, buttons, optical;
        StickView stick;
        Switch mouse;

        Side(String name) { this.name = name; }
    }

    @Override
    protected void onCreate(Bundle b) {
        super.onCreate(b);
        dark = (getResources().getConfiguration().uiMode & Configuration.UI_MODE_NIGHT_MASK) == Configuration.UI_MODE_NIGHT_YES;
        fg = dark ? 0xFFECECEC : 0xFF1B1B1F;
        weak = dark ? 0xFF9A9AA2 : 0xFF6A6A72;
        card = dark ? 0xFF22232A : 0xFFF1F1F5;
        buildUi();
        askPermissions();
    }

    @Override
    protected void onResume() {
        super.onResume();
        ui.post(tick);
    }

    @Override
    protected void onPause() {
        super.onPause();
        ui.removeCallbacks(tick);
    }

    private void askPermissions() {
        String[] perms = {Manifest.permission.BLUETOOTH_SCAN, Manifest.permission.BLUETOOTH_CONNECT, Manifest.permission.POST_NOTIFICATIONS};
        boolean all = true;
        for (String p : perms) if (checkSelfPermission(p) != PackageManager.PERMISSION_GRANTED) all = false;
        if (all) startService();
        else requestPermissions(perms, 1);
    }

    @Override
    public void onRequestPermissionsResult(int code, String[] perms, int[] results) {
        if (checkSelfPermission(Manifest.permission.BLUETOOTH_SCAN) == PackageManager.PERMISSION_GRANTED
                && checkSelfPermission(Manifest.permission.BLUETOOTH_CONNECT) == PackageManager.PERMISSION_GRANTED) {
            startService();
        }
    }

    private void startService() {
        startForegroundService(new Intent(this, JoyConService.class));
    }

    private SharedPreferences prefs() { return getSharedPreferences(JoyConService.PREFS, Context.MODE_PRIVATE); }

    // ------------------------------------------------------------------- UI

    private int dp(float v) { return Math.round(v * getResources().getDisplayMetrics().density); }

    private TextView text(String s, float sp, int color, boolean bold) {
        TextView t = new TextView(this);
        t.setText(s);
        t.setTextSize(sp);
        t.setTextColor(color);
        if (bold) t.setTypeface(Typeface.DEFAULT_BOLD);
        return t;
    }

    private LinearLayout card(String title) {
        TextView h = text(title, 14, weak, true);
        h.setPadding(dp(4), dp(18), 0, dp(6));
        root.addView(h);
        LinearLayout c = new LinearLayout(this);
        c.setOrientation(LinearLayout.VERTICAL);
        c.setPadding(dp(14), dp(12), dp(14), dp(12));
        GradientDrawable bg = new GradientDrawable();
        bg.setColor(card);
        bg.setCornerRadius(dp(14));
        c.setBackground(bg);
        root.addView(c);
        return c;
    }

    private void buildUi() {
        ScrollView sv = new ScrollView(this);
        sv.setBackgroundColor(dark ? 0xFF121317 : 0xFFFFFFFF);
        sv.setFitsSystemWindows(true);
        root = new LinearLayout(this);
        root.setOrientation(LinearLayout.VERTICAL);
        root.setPadding(dp(16), dp(16), dp(16), dp(32));
        sv.addView(root);
        setContentView(sv);

        root.addView(text("joy2con", 26, fg, true));
        root.addView(text("Joy-Con de Switch 2 (y NYXI) en Android", 14, weak, false));

        // Connection summary and button.
        LinearLayout c = card("Conexión");
        LinearLayout row = new LinearLayout(this);
        row.setGravity(android.view.Gravity.CENTER_VERTICAL);
        statusDot = new View(this);
        GradientDrawable dot = new GradientDrawable();
        dot.setShape(GradientDrawable.OVAL);
        dot.setColor(0xFF3E7BFA);
        statusDot.setBackground(dot);
        row.addView(statusDot, new LinearLayout.LayoutParams(dp(10), dp(10)));
        statusTitle = text("", 16, fg, true);
        statusTitle.setPadding(dp(10), 0, 0, 0);
        row.addView(statusTitle);
        c.addView(row);
        statusDetail = text("", 13, weak, false);
        statusDetail.setPadding(0, dp(4), 0, dp(8));
        c.addView(statusDetail);
        Button connect = new Button(this);
        connect.setText("Buscar y conectar");
        connect.setOnClickListener(v -> {
            if (JoyConService.instance == null) askPermissions();
            else JoyConService.instance.connectNow();
        });
        c.addView(connect);

        for (int i = 0; i < 2; i++) {
            String name = i == 0 ? "left" : "right";
            Side s = new Side(name);
            sides[i] = s;
            LinearLayout k = card(JoyConService.sideName(name));
            LinearLayout r = new LinearLayout(this);
            s.stick = new StickView(this);
            r.addView(s.stick, new LinearLayout.LayoutParams(dp(84), dp(84)));
            LinearLayout col = new LinearLayout(this);
            col.setOrientation(LinearLayout.VERTICAL);
            col.setPadding(dp(14), 0, 0, 0);
            s.title = text("", 15, fg, true);
            s.info = text("", 13, weak, false);
            s.buttons = text("", 13, fg, false);
            s.optical = text("", 12, weak, false);
            s.optical.setTypeface(Typeface.MONOSPACE);
            col.addView(s.title);
            col.addView(s.info);
            col.addView(s.buttons);
            col.addView(s.optical);
            r.addView(col, new LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1));
            k.addView(r);
            s.mouse = new Switch(this);
            s.mouse.setText("Usar como ratón");
            s.mouse.setTextColor(fg);
            s.mouse.setChecked(prefs().getBoolean("mouse_" + name, false));
            s.mouse.setOnCheckedChangeListener((v, on) -> {
                prefs().edit().putBoolean("mouse_" + name, on).apply();
                MouseService m = MouseService.instance;
                if (m != null) {
                    m.release(name);
                }
            });
            k.addView(s.mouse);
        }

        // Mouse settings.
        LinearLayout m = card("Modo ratón");
        m.addView(text("Mueve un cursor en pantalla y toca donde está. Hombro (L/R) = tocar, gatillo (ZL/ZR) = "
                + "mantener presionado, presionar el stick = atrás, −/+ = recientes, Capture/Home = inicio. "
                + "Necesita activar el servicio de accesibilidad de joy2con.", 13, weak, false));
        a11yStatus = text("", 13, fg, true);
        a11yStatus.setPadding(0, dp(8), 0, 0);
        m.addView(a11yStatus);
        Button a11y = new Button(this);
        a11y.setText("Abrir ajustes de accesibilidad");
        a11y.setOnClickListener(v -> startActivity(new Intent(Settings.ACTION_ACCESSIBILITY_SETTINGS)));
        m.addView(a11y);
        m.addView(text("Mover el cursor con", 13, fg, false));
        RadioGroup src = new RadioGroup(this);
        String cur = prefs().getString("mouse_source", "auto");
        String[][] opts = {{"auto", "Automático (sensor óptico si responde, si no el stick)"}, {"optical", "Sensor óptico"}, {"stick", "Stick"}};
        for (String[] o : opts) {
            RadioButton rb = new RadioButton(this);
            rb.setText(o[1]);
            rb.setTextColor(fg);
            rb.setId(View.generateViewId());
            src.addView(rb);
            if (o[0].equals(cur)) rb.setChecked(true);
            rb.setOnCheckedChangeListener((v, on) -> {
                if (on) prefs().edit().putString("mouse_source", o[0]).apply();
            });
        }
        m.addView(src);
        TextView sensLabel = text("", 13, fg, false);
        m.addView(sensLabel);
        SeekBar sens = new SeekBar(this);
        sens.setMax(100);
        float sv0 = prefs().getFloat("mouse_sens", 1.0f);
        sens.setProgress((int) Math.round(Math.log(sv0 / 0.2) / Math.log(40) * 100)); // 0.2 .. 8, logarithmic
        sensLabel.setText(String.format("Sensibilidad: %.2f", sv0));
        sens.setOnSeekBarChangeListener(new SeekBar.OnSeekBarChangeListener() {
            @Override public void onProgressChanged(SeekBar b, int p, boolean user) {
                float v = (float) (0.2 * Math.pow(40, p / 100.0));
                sensLabel.setText(String.format("Sensibilidad: %.2f", v));
                if (user) prefs().edit().putFloat("mouse_sens", v).apply();
            }
            @Override public void onStartTrackingTouch(SeekBar b) {}
            @Override public void onStopTrackingTouch(SeekBar b) {}
        });
        m.addView(sens);

        // Optional pairing address.
        LinearLayout p = card("Reconexión automática (opcional)");
        p.addView(text("Si escribes la dirección Bluetooth de este teléfono (Ajustes > Acerca del teléfono > "
                + "Información de estado > Dirección Bluetooth), los Joy-Con se emparejan con él y vuelven a "
                + "conectarse con cualquier botón. Si no, hay que usar SYNC cada vez. Ojo: emparejarlos aquí "
                + "hace que en la PC tengan que volver a usar SYNC.", 13, weak, false));
        EditText mac = new EditText(this);
        mac.setHint("AA:BB:CC:DD:EE:FF");
        mac.setTextColor(fg);
        mac.setHintTextColor(weak);
        mac.setSingleLine(true);
        mac.setText(prefs().getString("host_mac", ""));
        mac.addTextChangedListener(new TextWatcher() {
            @Override public void beforeTextChanged(CharSequence s, int a, int b, int c) {}
            @Override public void onTextChanged(CharSequence s, int a, int b, int c) {}
            @Override public void afterTextChanged(Editable e) { prefs().edit().putString("host_mac", e.toString().trim()).apply(); }
        });
        p.addView(mac);

        LinearLayout l = card("Registro");
        logView = text("", 12, weak, false);
        logView.setTypeface(Typeface.MONOSPACE);
        l.addView(logView);
    }

    // ------------------------------------------------------------- refresh

    private final Runnable tick = new Runnable() {
        @Override
        public void run() {
            refresh();
            ui.postDelayed(this, 33);
        }
    };

    private void setStatus(int color, String title, String detail) {
        ((GradientDrawable) statusDot.getBackground()).setColor(color);
        statusTitle.setText(title);
        statusDetail.setText(detail);
    }

    private void refresh() {
        JoyConService svc = JoyConService.instance;
        MouseService ms = MouseService.instance;
        a11yStatus.setText(ms != null ? "Servicio de accesibilidad: activado ✓" : "Servicio de accesibilidad: desactivado");
        if (ms != null) ms.updateVisibility();
        if (svc == null) {
            setStatus(0xFFE5484D, "El servicio no está corriendo.", "Concede los permisos de Bluetooth y presiona \"Buscar y conectar\".");
            return;
        }
        long now = SystemClock.uptimeMillis();
        boolean l = svc.links.containsKey("left"), r = svc.links.containsKey("right");
        List<String> missing = svc.missingSides();
        if (l && r) {
            setStatus(0xFF30A46C, "Los dos Joy-Con están conectados.", "");
        } else if (svc.connectingSide != null) {
            setStatus(0xFF3E7BFA, "Conectando el " + JoyConService.sideName(svc.connectingSide) + "…", "Mantenlo cerca y no lo apagues.");
        } else if (now - svc.lastErrorT < 20000 && svc.failures > 0) {
            setStatus(0xFFF5A524, "No se pudo conectar (" + svc.failures + (svc.failures == 1 ? " intento fallido)." : " intentos fallidos)."),
                    svc.lastError + ". Lo vuelve a intentar mientras siga en modo SYNC.");
        } else if (svc.scanning && now - svc.requestedAt < 30000) {
            setStatus(0xFF3E7BFA, "Buscando Joy-Con…", "Mantén SYNC 3 segundos en cada mitad hasta que la luz parpadee rápido.");
        } else if (missing.size() == 1) {
            setStatus(0xFF3E7BFA, "Falta el " + JoyConService.sideName(missing.get(0)) + ".", "Mantén SYNC 3 segundos en él.");
        } else {
            setStatus(0xFFF5A524, svc.scanning ? "No veo ningún Joy-Con." : "Sin buscar.",
                    "Mantén SYNC 3 segundos en cada mitad y presiona \"Buscar y conectar\".");
        }

        for (Side s : sides) {
            JoyConLink link = svc.links.get(s.name);
            if (link == null) {
                s.title.setText("No conectado");
                s.info.setText("");
                s.buttons.setText("");
                s.optical.setText("");
                s.stick.set(0, 0, false);
                continue;
            }
            Ns2.State st = link.state;
            s.title.setText("Conectado" + (link.paired ? " y emparejado" : ""));
            int pct = Ns2.batteryPercent(st.batteryMv);
            s.info.setText(String.format("%s · %.0f reportes/s · pausa máx. %.0f ms",
                    st.batteryMv > 0 ? "batería " + pct + "%" : "batería no reportada", link.rateHz, link.worstGapMs));
            List<String> p = st.pressed();
            s.buttons.setText(p.isEmpty() ? "Botones: —" : "Botones: " + String.join(" ", p));
            s.optical.setText(String.format("sensor óptico %5d %5d", st.mouse[0], st.mouse[1]));
            s.stick.set(st.sx, st.sy, st.is("left".equals(s.name) ? "LSTICK" : "RSTICK"));
        }
        StringBuilder sb = new StringBuilder();
        List<String> lines = svc.logLines();
        for (int i = 0; i < Math.min(40, lines.size()); i++) sb.append(lines.get(i)).append('\n');
        logView.setText(sb.toString());
    }

    /** Stick position: a ring and a dot. */
    final class StickView extends View {
        private double sx, sy;
        private boolean click;
        private final Paint ring = new Paint(Paint.ANTI_ALIAS_FLAG), dotP = new Paint(Paint.ANTI_ALIAS_FLAG);

        StickView(Context c) {
            super(c);
            ring.setStyle(Paint.Style.STROKE);
            ring.setStrokeWidth(dp(2));
            ring.setColor(weak);
        }

        void set(double x, double y, boolean pressed) {
            if (x != sx || y != sy || pressed != click) {
                sx = x; sy = y; click = pressed;
                invalidate();
            }
        }

        @Override
        protected void onDraw(Canvas c) {
            float w = getWidth(), h = getHeight(), r = Math.min(w, h) / 2 - dp(4);
            c.drawCircle(w / 2, h / 2, r, ring);
            dotP.setColor(click ? 0xFFFF8C5F : (dark ? Color.WHITE : 0xFF1B1B1F));
            float px = (float) (w / 2 + Ns2.clamp(sx, -1, 1) * (r - dp(10)));
            float py = (float) (h / 2 - Ns2.clamp(sy, -1, 1) * (r - dp(10)));
            c.drawCircle(px, py, dp(10), dotP);
        }
    }
}
