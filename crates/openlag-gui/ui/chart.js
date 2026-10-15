// Minimal canvas line chart for live data. No dependencies.
"use strict";

const Chart = (() => {
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

  // Smallest 4 * step >= v where step is 1, 2 or 5 times a power of ten, so every
  // grid label is a whole number.
  function niceMax(v) {
    if (v <= 0) return 4;
    const raw = v / 4;
    const p = Math.pow(10, Math.floor(Math.log10(raw)));
    for (const m of [1, 2, 5, 10]) if (m * p >= raw) return Math.max(4, 4 * m * p);
    return 40 * p;
  }

  // series: [{ values: (number|null)[], color }], `points` = window length.
  function line(canvas, series, { points = 120, minMax = 10, unit = "", fill = true } = {}) {
    const { ctx, w, h } = setup(canvas);
    const pad = { l: 40, r: 8, t: 8, b: 18 };
    const cw = w - pad.l - pad.r;
    const ch = h - pad.t - pad.b;
    let max = minMax;
    for (const s of series) for (const v of s.values) if (v != null && v > max) max = v;
    max = niceMax(max * 1.1);

    ctx.font = "11px Inter, Segoe UI, sans-serif";
    ctx.fillStyle = "#8b95a7";
    ctx.strokeStyle = "rgba(255,255,255,0.06)";
    ctx.lineWidth = 1;
    ctx.textAlign = "right";
    ctx.textBaseline = "middle";
    for (let i = 0; i <= 4; i++) {
      const y = pad.t + ch - (ch * i) / 4;
      ctx.beginPath();
      ctx.moveTo(pad.l, Math.round(y) + 0.5);
      ctx.lineTo(w - pad.r, Math.round(y) + 0.5);
      ctx.stroke();
      ctx.fillText(String(Math.round((max * i) / 4)) + unit, pad.l - 6, y);
    }
    ctx.textAlign = "left";
    ctx.textBaseline = "alphabetic";
    ctx.fillText(`${points} с назад`, pad.l, h - 4);
    ctx.textAlign = "right";
    ctx.fillText("сейчас", w - pad.r, h - 4);

    const x = (i) => pad.l + (cw * i) / Math.max(points - 1, 1);
    const y = (v) => pad.t + ch - (ch * Math.min(v, max)) / max;
    for (const s of series) {
      const vals = s.values.slice(-points);
      const offset = points - vals.length;
      ctx.strokeStyle = s.color;
      ctx.lineWidth = 2;
      ctx.lineJoin = "round";
      let open = false;
      let first = null;
      let last = null;
      ctx.beginPath();
      vals.forEach((v, i) => {
        if (v == null) {
          open = false;
          return;
        }
        const px = x(i + offset);
        const py = y(v);
        if (!open) ctx.moveTo(px, py);
        else ctx.lineTo(px, py);
        open = true;
        if (first == null) first = px;
        last = [px, py];
      });
      ctx.stroke();
      if (fill && first != null && series.length === 1) {
        ctx.lineTo(last[0], pad.t + ch);
        ctx.lineTo(first, pad.t + ch);
        ctx.closePath();
        const g = ctx.createLinearGradient(0, pad.t, 0, pad.t + ch);
        g.addColorStop(0, s.color + "40");
        g.addColorStop(1, s.color + "00");
        ctx.fillStyle = g;
        ctx.fill();
      }
      if (last) {
        ctx.fillStyle = s.color;
        ctx.beginPath();
        ctx.arc(last[0], last[1], 3, 0, Math.PI * 2);
        ctx.fill();
      }
    }
  }

  // Tiny sparkline without axes.
  function spark(canvas, values, color) {
    const { ctx, w, h } = setup(canvas);
    const known = values.filter((v) => v != null);
    if (!known.length) return;
    const max = Math.max(...known) * 1.15 || 1;
    const min = Math.min(...known) * 0.85;
    ctx.strokeStyle = color;
    ctx.lineWidth = 1.5;
    ctx.beginPath();
    let open = false;
    values.forEach((v, i) => {
      if (v == null) {
        open = false;
        return;
      }
      const px = (w * i) / Math.max(values.length - 1, 1);
      const py = h - 2 - ((h - 4) * (v - min)) / Math.max(max - min, 1);
      if (!open) ctx.moveTo(px, py);
      else ctx.lineTo(px, py);
      open = true;
    });
    ctx.stroke();
  }

  return { line, spark };
})();
