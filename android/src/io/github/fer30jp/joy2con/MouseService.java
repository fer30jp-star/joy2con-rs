// SPDX-License-Identifier: GPL-3.0-or-later
package io.github.fer30jp.joy2con;

import android.accessibilityservice.AccessibilityService;
import android.accessibilityservice.GestureDescription;
import android.content.Context;
import android.graphics.Canvas;
import android.graphics.Color;
import android.graphics.Paint;
import android.graphics.Path;
import android.graphics.PixelFormat;
import android.graphics.Rect;
import android.os.Handler;
import android.os.Looper;
import android.view.Gravity;
import android.view.View;
import android.view.WindowManager;
import android.view.accessibility.AccessibilityEvent;

/**
 * Mouse mode on Android: an on-screen pointer moved by a Joy-Con, with taps
 * and swipes injected through the accessibility gesture API (no root needed).
 *
 * Movement comes from the optical sensor when it reports motion, otherwise
 * from the stick. Shoulder (L/R) = tap, trigger (ZL/ZR) = long press,
 * stick press = back. Scroll: the stick while the optical sensor drives the
 * pointer, or the D-pad up/down (left) / X and B (right) otherwise.
 */
public class MouseService extends AccessibilityService {
    static volatile MouseService instance;

    private final Handler main = new Handler(Looper.getMainLooper());
    private WindowManager wm;
    private Pointer pointer;
    private WindowManager.LayoutParams lp;
    private float x = -1, y = -1;
    private int sw, sh;

    // per side: previous optical sample, button state, last report time
    private final int[][] prevOptical = new int[2][];
    private final boolean[][] held = new boolean[2][8];
    private final long[] lastT = new long[2];
    private final long[] lastOpticalMove = new long[2];
    private final double[] scrollAcc = new double[2];

    @Override
    protected void onServiceConnected() {
        instance = this;
        wm = getSystemService(WindowManager.class);
        Rect b = wm.getCurrentWindowMetrics().getBounds();
        sw = b.width();
        sh = b.height();
        x = sw / 2f;
        y = sh / 2f;
        pointer = new Pointer(this);
        int size = (int) (28 * getResources().getDisplayMetrics().density);
        lp = new WindowManager.LayoutParams(size, size,
                WindowManager.LayoutParams.TYPE_ACCESSIBILITY_OVERLAY,
                WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE | WindowManager.LayoutParams.FLAG_NOT_TOUCHABLE
                        | WindowManager.LayoutParams.FLAG_LAYOUT_IN_SCREEN | WindowManager.LayoutParams.FLAG_LAYOUT_NO_LIMITS,
                PixelFormat.TRANSLUCENT);
        lp.gravity = Gravity.TOP | Gravity.START;
        lp.x = (int) x;
        lp.y = (int) y;
        pointer.setVisibility(View.GONE);
        wm.addView(pointer, lp);
    }

    @Override public void onAccessibilityEvent(AccessibilityEvent event) {}
    @Override public void onInterrupt() {}

    @Override
    public boolean onUnbind(android.content.Intent intent) {
        instance = null;
        if (pointer != null) {
            try { wm.removeView(pointer); } catch (Exception ignored) {}
        }
        return super.onUnbind(intent);
    }

    private static int idx(String side) { return "left".equals(side) ? 0 : 1; }

    private static int delta16(int cur, int prev) {
        int d = (cur - prev) & 0xFFFF;
        return d > 0x7FFF ? d - 0x10000 : d;
    }

    /** Called on the Joy-Con's GATT thread for every report of a half in mouse mode. */
    void feed(String side, Ns2.State s) {
        int i = idx(side);
        float sens = JoyConService.instance != null ? JoyConService.instance.prefs().getFloat("mouse_sens", 1.0f) : 1.0f;
        String source = JoyConService.instance != null ? JoyConService.instance.prefs().getString("mouse_source", "auto") : "auto";
        double dt = lastT[i] > 0 ? Math.min(0.05, (s.t - lastT[i]) / 1000.0) : 0;
        lastT[i] = s.t;

        // Movement.
        float dx = 0, dy = 0;
        boolean optical = false;
        if (prevOptical[i] != null && !"stick".equals(source)) {
            int ox = delta16(s.mouse[0], prevOptical[i][0]);
            int oy = delta16(s.mouse[1], prevOptical[i][1]);
            if (Math.abs(ox) < 400 && Math.abs(oy) < 400 && (ox != 0 || oy != 0)) {
                dx = ox * sens;
                dy = oy * sens;
                lastOpticalMove[i] = s.t;
            }
        }
        prevOptical[i] = new int[] {s.mouse[0], s.mouse[1]};
        optical = "optical".equals(source) || ("auto".equals(source) && s.t - lastOpticalMove[i] < 3000 && lastOpticalMove[i] > 0);
        if (!optical) {
            // Stick: speed grows with deflection past a dead zone.
            double mag = Math.hypot(s.sx, s.sy);
            if (mag > 0.15) {
                double f = Math.pow(Math.min(1, (mag - 0.15) / 0.85), 1.7);
                double speed = f * 1800 * sens; // px per second at full tilt
                dx = (float) (s.sx / mag * speed * dt);
                dy = (float) (-s.sy / mag * speed * dt);
            }
        }
        if (dx != 0 || dy != 0) move(dx, dy);

        // Buttons.
        boolean left = "left".equals(side);
        edge(i, 0, s.is(left ? "L" : "R"), this::tap);
        edge(i, 1, s.is(left ? "ZL" : "ZR"), this::longPress);
        edge(i, 2, s.is(left ? "LSTICK" : "RSTICK"), () -> performGlobalAction(GLOBAL_ACTION_BACK));
        edge(i, 3, s.is(left ? "MINUS" : "PLUS"), () -> performGlobalAction(GLOBAL_ACTION_RECENTS));
        edge(i, 4, s.is(left ? "CAPTURE" : "HOME"), () -> performGlobalAction(GLOBAL_ACTION_HOME));

        // Scroll.
        double scroll = 0;
        if (optical) {
            if (Math.abs(s.sy) > 0.25) scroll = s.sy;
        } else {
            boolean up = s.is(left ? "UP" : "X"), down = s.is(left ? "DOWN" : "B");
            scroll = up ? 1 : down ? -1 : 0;
        }
        scrollAcc[i] += scroll * dt * 6; // swipes per second at full tilt
        if (Math.abs(scrollAcc[i]) >= 1) {
            swipe(scrollAcc[i] > 0);
            scrollAcc[i] = 0;
        }
    }

    private void edge(int i, int b, boolean now, Runnable onPress) {
        if (now && !held[i][b]) main.post(onPress);
        held[i][b] = now;
    }

    void release(String side) {
        int i = idx(side);
        prevOptical[i] = null;
        lastT[i] = 0;
        held[i] = new boolean[8];
        main.post(this::updateVisibility);
    }

    void updateVisibility() {
        JoyConService svc = JoyConService.instance;
        boolean any = svc != null && ((svc.isMouse("left") && svc.links.containsKey("left"))
                || (svc.isMouse("right") && svc.links.containsKey("right")));
        if (pointer != null) pointer.setVisibility(any ? View.VISIBLE : View.GONE);
    }

    private void move(float dx, float dy) {
        main.post(() -> {
            Rect b = wm.getCurrentWindowMetrics().getBounds();
            sw = b.width();
            sh = b.height();
            x = Math.max(0, Math.min(sw - 1, x + dx));
            y = Math.max(0, Math.min(sh - 1, y + dy));
            lp.x = (int) x;
            lp.y = (int) y;
            if (pointer.getVisibility() != View.VISIBLE) pointer.setVisibility(View.VISIBLE);
            try { wm.updateViewLayout(pointer, lp); } catch (Exception ignored) {}
        });
    }

    private void gesture(Path p, long duration) {
        GestureDescription g = new GestureDescription.Builder()
                .addStroke(new GestureDescription.StrokeDescription(p, 0, duration))
                .build();
        dispatchGesture(g, null, null);
    }

    private void tap() {
        Path p = new Path();
        p.moveTo(x, y);
        gesture(p, 40);
    }

    private void longPress() {
        Path p = new Path();
        p.moveTo(x, y);
        gesture(p, 700);
    }

    private void swipe(boolean up) {
        float d = sh * 0.18f;
        Path p = new Path();
        // content moves with the finger: scrolling up means swiping down
        float y0 = Math.max(1, Math.min(sh - 2, up ? y - d / 2 : y + d / 2));
        float y1 = Math.max(1, Math.min(sh - 2, up ? y + d / 2 : y - d / 2));
        p.moveTo(x, y0);
        p.lineTo(x, y1);
        gesture(p, 120);
    }

    /** The pointer drawn on top of everything. */
    static final class Pointer extends View {
        private final Paint fill = new Paint(Paint.ANTI_ALIAS_FLAG), edge = new Paint(Paint.ANTI_ALIAS_FLAG);

        Pointer(Context c) {
            super(c);
            fill.setColor(Color.WHITE);
            edge.setColor(Color.BLACK);
            edge.setStyle(Paint.Style.STROKE);
            edge.setStrokeWidth(2 * c.getResources().getDisplayMetrics().density);
        }

        @Override
        protected void onDraw(Canvas c) {
            float w = getWidth(), h = getHeight();
            Path p = new Path();
            p.moveTo(2, 2);
            p.lineTo(2, h * 0.8f);
            p.lineTo(w * 0.28f, h * 0.6f);
            p.lineTo(w * 0.5f, h - 2);
            p.lineTo(w * 0.64f, h * 0.92f);
            p.lineTo(w * 0.44f, h * 0.54f);
            p.lineTo(w * 0.75f, h * 0.54f);
            p.close();
            c.drawPath(p, fill);
            c.drawPath(p, edge);
        }
    }
}
