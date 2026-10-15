// The "Фан" tab: a calculator and a snake game, for while the ping loads.
"use strict";

const Fun = (() => {
  // ---------------- Calculator ----------------
  const KEYS = [
    ["C", "fn"], ["⌫", "fn"], ["%", "op"], ["÷", "op"],
    ["7"], ["8"], ["9"], ["×", "op"],
    ["4"], ["5"], ["6"], ["−", "op"],
    ["1"], ["2"], ["3"], ["+", "op"],
    ["±", "fn"], ["0"], [","], ["=", "eq"],
  ];
  let expr = "";
  let justEvaluated = false;

  // Shunting-yard over + − × ÷ and postfix %; no eval().
  function evaluate(src) {
    const tokens = src.match(/\d+(?:\.\d+)?|[+\-×÷%]/g) || [];
    const out = [];
    const ops = [];
    const prec = { "+": 1, "-": 1, "×": 2, "÷": 2 };
    let expectNumber = true;
    let sign = 1;
    for (const t of tokens) {
      if (/^\d/.test(t)) {
        out.push(sign * Number(t));
        sign = 1;
        expectNumber = false;
      } else if (t === "%") {
        if (!out.length) throw new Error("bad");
        out.push(out.pop() / 100);
      } else if (t === "-" && expectNumber) {
        sign = -sign; // unary minus
      } else {
        while (ops.length && prec[ops[ops.length - 1]] >= prec[t]) out.push(ops.pop());
        ops.push(t);
        expectNumber = true;
      }
    }
    if (expectNumber && tokens.length) throw new Error("incomplete");
    while (ops.length) out.push(ops.pop());
    const st = [];
    for (const t of out) {
      if (typeof t === "number") { st.push(t); continue; }
      const b = st.pop();
      const a = st.pop();
      if (a === undefined || b === undefined) throw new Error("bad");
      st.push(t === "+" ? a + b : t === "-" ? a - b : t === "×" ? a * b : a / b);
    }
    if (st.length !== 1 || !Number.isFinite(st[0])) throw new Error("bad");
    return st[0];
  }

  function fmt(n) {
    const r = Math.round(n * 1e10) / 1e10;
    return String(r).replace(".", ",");
  }

  function render() {
    const shown = expr.replace(/-/g, "−").replace(/\./g, ",");
    document.getElementById("calc-expr").textContent = justEvaluated ? "" : shown;
    let out = "0";
    if (expr) {
      try { out = fmt(evaluate(expr)); } catch { out = justEvaluated ? "Ошибка" : document.getElementById("calc-out").textContent; }
    }
    document.getElementById("calc-out").textContent = out;
  }

  function press(k) {
    const last = expr.slice(-1);
    const isOp = (c) => "+-×÷".includes(c);
    if (justEvaluated && /[0-9,]/.test(k)) expr = "";
    justEvaluated = false;
    if (/^[0-9]$/.test(k)) expr += k;
    else if (k === ",") {
      const tail = expr.split(/[+\-×÷]/).pop();
      if (!tail.includes(".")) expr += tail === "" ? "0." : ".";
    } else if (k === "C") expr = "";
    else if (k === "⌫") expr = expr.slice(0, -1);
    else if (k === "%") { if (/[0-9]/.test(last)) expr += "%"; }
    else if (k === "±") {
      const m = expr.match(/(^|[+\-×÷])(-?)(\d+(?:\.\d*)?%?)$/);
      if (m) expr = expr.slice(0, m.index) + m[1] + (m[2] ? "" : "-") + m[3];
    } else if (k === "=") {
      try { expr = String(Math.round(evaluate(expr) * 1e10) / 1e10); justEvaluated = true; } catch { justEvaluated = true; }
    } else {
      const op = k === "−" ? "-" : k;
      if (expr === "" && op !== "-") return render();
      if (isOp(last)) expr = expr.slice(0, -1);
      expr += op;
    }
    render();
  }

  function initCalc() {
    const box = document.getElementById("calc-keys");
    for (const [k, cls] of KEYS) {
      const b = document.createElement("button");
      if (k === "⌫") {
        b.innerHTML = '<svg class="ic"><use href="#i-backspace"/></svg>';
        b.setAttribute("aria-label", "Стереть");
      } else b.textContent = k;
      if (cls) b.className = cls;
      b.addEventListener("click", () => press(k));
      box.appendChild(b);
    }
    render();
  }

  function calcKey(e) {
    const map = { "*": "×", "/": "÷", "-": "−", "+": "+", Enter: "=", "=": "=", Backspace: "⌫", Escape: "C", Delete: "C", ".": ",", ",": ",", "%": "%" };
    if (/^[0-9]$/.test(e.key)) press(e.key);
    else if (map[e.key]) press(map[e.key]);
    else return false;
    e.preventDefault();
    return true;
  }

  // ---------------- Snake ----------------
  const N = 20;
  let snake, dir, queue, food, score, timer, paused, over;
  let best = 0;
  try { best = Number(localStorage.getItem("snake-best")) || 0; } catch { /* storage unavailable */ }

  function placeFood() {
    const free = [];
    for (let x = 0; x < N; x++) for (let y = 0; y < N; y++) if (!snake.some((s) => s.x === x && s.y === y)) free.push({ x, y });
    food = free[Math.floor(Math.random() * free.length)];
  }

  function draw() {
    const c = document.getElementById("snake");
    const ctx = c.getContext("2d");
    const s = c.width / N;
    ctx.fillStyle = "#0b0f17";
    ctx.fillRect(0, 0, c.width, c.height);
    ctx.fillStyle = "rgba(255,255,255,0.03)";
    for (let x = 0; x < N; x++) for (let y = 0; y < N; y++) if ((x + y) % 2) ctx.fillRect(x * s, y * s, s, s);
    if (food) {
      ctx.fillStyle = "#6366f1";
      ctx.beginPath();
      ctx.arc(food.x * s + s / 2, food.y * s + s / 2, s * 0.38, 0, Math.PI * 2);
      ctx.fill();
    }
    snake.forEach((p, i) => {
      ctx.fillStyle = i === 0 ? "#34d399" : "#10b981";
      ctx.globalAlpha = Math.max(0.35, 1 - i / (snake.length + 6));
      ctx.beginPath();
      ctx.roundRect(p.x * s + 1, p.y * s + 1, s - 2, s - 2, 5);
      ctx.fill();
    });
    ctx.globalAlpha = 1;
    if (paused || over) {
      ctx.fillStyle = "rgba(11,15,23,0.7)";
      ctx.fillRect(0, 0, c.width, c.height);
      ctx.fillStyle = "#e6eaf2";
      ctx.font = "600 24px Inter, Segoe UI, sans-serif";
      ctx.textAlign = "center";
      ctx.fillText(over ? `Конец игры: ${score}` : "Пауза", c.width / 2, c.height / 2);
    }
  }

  function step() {
    if (paused || over) return;
    if (queue.length) dir = queue.shift();
    const head = { x: snake[0].x + dir.x, y: snake[0].y + dir.y };
    const ate = food && head.x === food.x && head.y === food.y;
    const body = ate ? snake : snake.slice(0, -1);
    if (head.x < 0 || head.y < 0 || head.x >= N || head.y >= N || body.some((p) => p.x === head.x && p.y === head.y)) {
      over = true;
      clearInterval(timer);
      if (score > best) {
        best = score;
        try { localStorage.setItem("snake-best", String(best)); } catch { /* ignore */ }
        document.getElementById("snake-best").textContent = best;
      }
      document.getElementById("snake-start").textContent = "Ещё раз";
      return draw();
    }
    snake = [head, ...body];
    if (ate) {
      score++;
      document.getElementById("snake-score").textContent = score;
      placeFood();
      restartTimer();
    }
    draw();
  }

  function restartTimer() {
    clearInterval(timer);
    timer = setInterval(step, Math.max(55, 130 - score * 3));
  }

  function startSnake() {
    snake = [{ x: 8, y: 10 }, { x: 7, y: 10 }, { x: 6, y: 10 }];
    dir = { x: 1, y: 0 };
    queue = [];
    score = 0;
    paused = false;
    over = false;
    document.getElementById("snake-score").textContent = "0";
    document.getElementById("snake-start").textContent = "Заново";
    placeFood();
    restartTimer();
    draw();
    document.getElementById("snake").focus();
  }

  function snakeKey(e) {
    if (!snake || over) return false;
    const dirs = {
      ArrowUp: [0, -1], KeyW: [0, -1], ArrowDown: [0, 1], KeyS: [0, 1],
      ArrowLeft: [-1, 0], KeyA: [-1, 0], ArrowRight: [1, 0], KeyD: [1, 0],
    };
    if (e.code === "Space") {
      paused = !paused;
      draw();
    } else if (dirs[e.code]) {
      const [x, y] = dirs[e.code];
      const last = queue.length ? queue[queue.length - 1] : dir;
      if (last.x !== -x || last.y !== -y) queue.push({ x, y });
      if (queue.length > 3) queue.shift();
    } else return false;
    e.preventDefault();
    return true;
  }

  function init() {
    initCalc();
    document.getElementById("snake-best").textContent = best;
    snake = [{ x: 8, y: 10 }, { x: 7, y: 10 }, { x: 6, y: 10 }];
    food = { x: 14, y: 10 };
    draw();
    document.getElementById("snake-start").addEventListener("click", startSnake);
    document.addEventListener("keydown", (e) => {
      if (!document.getElementById("page-fun").classList.contains("active")) return;
      if (e.target.matches("input, textarea, select")) return;
      if (document.activeElement && document.activeElement.id === "snake") snakeKey(e);
      else calcKey(e);
    });
  }

  // Pause when leaving the tab.
  function hide() {
    if (snake && !over && timer && !paused) {
      paused = true;
      draw();
    }
  }

  return { init, hide };
})();
