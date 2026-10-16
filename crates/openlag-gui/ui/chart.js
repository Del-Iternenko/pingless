// Live canvas charts without dependencies.
//
// Stability tricks:
// - values are smoothed with an exponential moving average before drawing;
// - the line scrolls continuously between one-second samples (`phase`), so it glides
//   instead of jumping once per second;
// - the Y scale grows at once but shrinks slowly, so one spike does not make the
//   whole chart jump twice.
"use strict";

const Chart = (() => {
  const scale = new WeakMap(); // canvas -> displayed max

  function setup(canvas) {
    const dpr = window.devicePixelRatio || 1;
    const w = canvas.clientWidth;
    const h = canvas.clientHeight;
    if (canvas.width !== Math.round(w * dpr) || canvas.height !== Math.round(h * dpr)) {
      canvas.width = Math.round(w * dpr);
      canvas.height = Math.round(h * dpr);
    }
    const ctx = canvas.getContext("2d");
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, w, h);
    return { ctx, w, h };
  }

  // Exponential moving average; gaps (null) restart the average.
  function smooth(values, alpha = 0.35) {
    let s = null;
    return values.map((v) => {
      if (v == null) return (s = null);
      s = s == null ? v : s + alpha * (v - s);
      return s;
    });
  }

  function niceStep(max) {
    const raw = max / 4;
    const p = Math.pow(10, Math.floor(Math.log10(Math.max(raw, 1e-9))));
    for (const m of [1, 2, 2.5, 5, 10]) if (m * p >= raw) return m * p;
    return 10 * p;
  }

  // Draws a smooth curve through points using midpoint quadratic segments.
  function curve(ctx, pts) {
    ctx.moveTo(pts[0][0], pts[0][1]);
    for (let i = 1; i < pts.length - 1; i++) {
      const mx = (pts[i][0] + pts[i + 1][0]) / 2;
      const my = (pts[i][1] + pts[i + 1][1]) / 2;
      ctx.quadraticCurveTo(pts[i][0], pts[i][1], mx, my);
    }
    const last = pts[pts.length - 1];
    ctx.lineTo(last[0], last[1]);
  }

  // series: [{ values: (number|null)[], color, dashed? }]
  // phase: 0..1, time since the newest sample in units of one sample interval.
  function line(canvas, series, { points = 120, minMax = 10, unit = "", fill = true, phase = 0, smoothing = true } = {}) {
    const { ctx, w, h } = setup(canvas);
    const pad = { l: 40, r: 10, t: 10, b: 18 };
    const cw = w - pad.l - pad.r;
    const ch = h - pad.t - pad.b;
    const prepared = series.map((s) => ({ ...s, values: (smoothing ? smooth(s.values) : s.values).slice(-(points + 1)) }));

    let dataMax = minMax;
    for (const s of prepared) for (const v of s.values) if (v != null && v > dataMax) dataMax = v;
    const target = dataMax * 1.15;
    let max = scale.get(canvas) ?? target;
    max = target > max ? target : max + (target - max) * 0.04;
    scale.set(canvas, max);

    // Grid with round labels.
    const step = niceStep(max);
    ctx.font = "11px Inter, Segoe UI, sans-serif";
    ctx.lineWidth = 1;
    ctx.textAlign = "right";
    ctx.textBaseline = "middle";
    for (let v = 0; v <= max + 1e-9; v += step) {
      const y = Math.round(pad.t + ch - (ch * v) / max) + 0.5;
      ctx.strokeStyle = "rgba(255,255,255,0.06)";
      ctx.beginPath();
      ctx.moveTo(pad.l, y);
      ctx.lineTo(w - pad.r, y);
      ctx.stroke();
      ctx.fillStyle = "#8b95a7";
      ctx.fillText(String(Math.round(v)) + unit, pad.l - 6, y);
    }
    ctx.textBaseline = "alphabetic";
    ctx.textAlign = "left";
    ctx.fillText(I18N.t(`${points} с назад`), pad.l, h - 3);
    ctx.textAlign = "right";
    ctx.fillText(I18N.t("сейчас"), w - pad.r, h - 3);

    const stepX = cw / Math.max(points - 1, 1);
    const yOf = (v) => pad.t + ch - (ch * Math.min(v, max)) / max;
    ctx.save();
    ctx.beginPath();
    ctx.rect(pad.l, pad.t - 2, cw + 1, ch + 4);
    ctx.clip();
    for (const s of prepared) {
      const n = s.values.length;
      // The newest sample sits at the right edge minus the time elapsed since it came.
      const xOf = (i) => w - pad.r - (n - 1 - i + phase) * stepX;
      const runs = [];
      let run = [];
      s.values.forEach((v, i) => {
        if (v == null) {
          if (run.length) runs.push(run);
          run = [];
        } else run.push([xOf(i), yOf(v)]);
      });
      if (run.length) runs.push(run);

      for (const r of runs) {
        if (r.length < 2) continue;
        if (fill && prepared.length === 1) {
          ctx.beginPath();
          curve(ctx, r);
          ctx.lineTo(r[r.length - 1][0], pad.t + ch);
          ctx.lineTo(r[0][0], pad.t + ch);
          ctx.closePath();
          const g = ctx.createLinearGradient(0, pad.t, 0, pad.t + ch);
          g.addColorStop(0, s.color + "38");
          g.addColorStop(1, s.color + "00");
          ctx.fillStyle = g;
          ctx.fill();
        }
        ctx.beginPath();
        curve(ctx, r);
        ctx.strokeStyle = s.color;
        ctx.lineWidth = 2;
        ctx.lineJoin = "round";
        ctx.lineCap = "round";
        ctx.setLineDash(s.dashed ? [5, 5] : []);
        ctx.stroke();
      }
      ctx.setLineDash([]);
      const last = runs.at(-1)?.at(-1);
      if (last) {
        ctx.fillStyle = s.color;
        ctx.beginPath();
        ctx.arc(last[0], last[1], 3.2, 0, Math.PI * 2);
        ctx.fill();
        ctx.fillStyle = s.color + "40";
        ctx.beginPath();
        ctx.arc(last[0], last[1], 7, 0, Math.PI * 2);
        ctx.fill();
      }
    }
    ctx.restore();
  }

  // Tiny sparkline without axes.
  function spark(canvas, values, color) {
    const { ctx, w, h } = setup(canvas);
    const vals = smooth(values);
    const known = vals.filter((v) => v != null);
    if (known.length < 2) return;
    const max = Math.max(...known) * 1.15;
    const min = Math.min(...known) * 0.85;
    const pts = [];
    vals.forEach((v, i) => {
      if (v == null) return;
      pts.push([(w * i) / Math.max(vals.length - 1, 1), h - 2 - ((h - 4) * (v - min)) / Math.max(max - min, 1)]);
    });
    ctx.strokeStyle = color;
    ctx.lineWidth = 1.6;
    ctx.lineJoin = "round";
    ctx.beginPath();
    curve(ctx, pts);
    ctx.stroke();
  }

  return { line, spark, smooth };
})();
