// Renders /api/state every 5 s. Text goes in with textContent only, never innerHTML: journal
// details are untrusted (news text from P4 on).
"use strict";

const POLL_MS = 5000;
// A reading older than this is greyed out: Alpaca or the console is stuck.
const STALE_SECS = 20;

// The token arrives in the URL fragment (never sent to the server or in a Referer). Keep it for
// reloads of this tab only, and take it off the address bar.
const token = (() => {
  const fromHash = location.hash.slice(1);
  try {
    if (fromHash) sessionStorage.setItem("bmv-token", fromHash);
    return fromHash || sessionStorage.getItem("bmv-token") || "";
  } catch (_) {
    return fromHash;
  } finally {
    if (fromHash) history.replaceState(null, "", location.pathname);
  }
})();

const $ = (id) => document.getElementById(id);
const el = (tag, text, cls) => {
  const e = document.createElement(tag);
  if (text !== undefined) e.textContent = text;
  if (cls) e.className = cls;
  return e;
};

const ASPECT = {
  "TRADING": "go",
  "PAUSED (unresolved)": "caution",
  "HALTED": "danger",
  "STOPPED (kill switch)": "danger",
  "NO SESSION TODAY": "off",
  "ENGINE NOT RUNNING": "caution",
  "ENGINE DOWN, POSITIONS OPEN": "danger",
  "UNKNOWN (journal unreadable)": "danger",
};
const WORDS = {
  "TRADING": "Trading",
  "PAUSED (unresolved)": "Paused",
  "HALTED": "Halted",
  "STOPPED (kill switch)": "Stopped",
  "NO SESSION TODAY": "No session today",
  "ENGINE NOT RUNNING": "Engine not running",
  "ENGINE DOWN, POSITIONS OPEN": "Engine down, positions open",
  "UNKNOWN (journal unreadable)": "Unknown: journal unreadable",
};

let lastState = null;

function setSignal(state, aspect, meta) {
  $("state").textContent = state;
  $("meta").textContent = meta;
  $("signal").dataset.aspect = aspect;
  $("state-word").textContent = state;
  $("state-pill").dataset.aspect = aspect;
  if (state !== lastState) {
    const lamp = document.querySelector(".lamp");
    lamp.classList.remove("changed");
    void lamp.offsetWidth; // restart the animation
    if (lastState !== null) lamp.classList.add("changed");
    lastState = state;
  }
}

function rows(tbody, items, cells, emptyText, cols) {
  tbody.replaceChildren();
  if (!items) {
    const tr = el("tr", undefined, "empty");
    const td = el("td", "Couldn't read this from Alpaca (see above)");
    td.colSpan = cols;
    tr.append(td);
    tbody.append(tr);
    return;
  }
  if (items.length === 0) {
    const tr = el("tr", undefined, "empty");
    const td = el("td", emptyText);
    td.colSpan = cols;
    tr.append(td);
    tbody.append(tr);
    return;
  }
  for (const item of items) {
    const tr = el("tr");
    for (const [text, cls] of cells(item)) tr.append(el("td", String(text), cls));
    tbody.append(tr);
  }
}

function money(text, id) {
  const e = $(id);
  // "$100000.00" -> "$100,000.00"
  e.textContent = text ? text.replace(/\d+(?=\.)/, (d) => d.replace(/\B(?=(\d{3})+$)/g, ",")) : "–";
  e.classList.remove("pos", "neg");
  if (id === "pnl" && text && text !== "$0.00") e.classList.add(text.startsWith("-") ? "neg" : "pos");
}

function banner(text, danger) {
  $("banners").append(el("p", text, danger ? "banner danger" : "banner"));
}

// "$1,234.50" or "-$12.00" -> number; NaN when absent.
const dollars = (text) => (text ? Number(text.replace(/[$,]/g, "")) : NaN);

function pnlPercent(account) {
  const e = $("pnl-pct");
  const pnl = dollars(account?.today_pnl);
  const start = dollars(account?.equity) - pnl;
  e.className = "badge";
  if (!Number.isFinite(pnl) || !(start > 0)) {
    e.textContent = "";
    return;
  }
  const pct = (pnl / start) * 100;
  e.textContent = `${pct > 0 ? "▲" : pct < 0 ? "▼" : ""}${Math.abs(pct).toFixed(2)}%`;
  if (pct !== 0) e.classList.add(pct > 0 ? "up" : "down");
}

// Nothing from an old reading stays on screen once a reading fails.
function clearView() {
  $("banners").replaceChildren();
  for (const id of ["equity", "pnl", "bp"]) money(undefined, id);
  pnlPercent(undefined);
  $("counts").textContent = "";
  for (const id of ["positions", "orders", "trials", "journal"]) $(id).replaceChildren();
  document.body.classList.remove("stale");
}

function render(s) {
  const word = WORDS[s.state] ?? s.state;
  const session = s.summary.session ? `session ${s.summary.session}` : "no session yet";
  setSignal(word, ASPECT[s.state] ?? "off", `${s.mode === "LIVE" ? "LIVE" : "Paper"}, ${session}, updated ${s.time_et} ET`);

  $("banners").replaceChildren();
  const age = Math.round(Date.now() / 1000 - s.now);
  const stale = age > STALE_SECS;
  document.body.classList.toggle("stale", stale);
  if (stale) {
    setSignal("Stale reading", "off", `Last reading ${s.time_et} ET, ${age} s ago. Alpaca or the console is slow; nothing below is current.`);
    banner(`Showing the reading from ${s.time_et} ET. It isn't current.`, true);
  }
  if (s.mode === "LIVE") banner("Live account: real money.", true);
  if (s.stop) banner(`STOP is engaged (${s.stop}). To clear it, remove data/STOP in the terminal.`, true);
  if (s.summary.halted) banner(`Halt reason: ${s.summary.halted}`, true);
  if (s.state === "ENGINE NOT RUNNING") banner("No heartbeat from the engine in the last minute. If you started `run`, check its terminal.");
  if (s.state === "ENGINE DOWN, POSITIONS OPEN") banner("Positions are open but the engine isn't running. Alpaca still holds their stops and targets. Use Close everything to exit them now.", true);
  if (s.summary.unresolved.length) banner(`Waiting on: ${s.summary.unresolved.join(", ")}. No new entries until it resolves.`);
  if (s.summary.unreadable) banner(`${s.summary.unreadable} journal lines couldn't be read.`, true);
  for (const [what, err] of Object.entries(s.errors)) banner(`Couldn't read ${what}: ${err}`, true);

  money(s.account?.equity, "equity");
  money(s.account?.today_pnl, "pnl");
  money(s.account?.buying_power, "bp");
  pnlPercent(s.account);
  const mode = $("mode-pill");
  mode.textContent = s.mode === "LIVE" ? "LIVE" : "PAPER";
  mode.classList.toggle("live", s.mode === "LIVE");
  if (s.stop) {
    $("state-word").textContent = "STOPPED";
    $("state-pill").dataset.aspect = "danger";
  }
  const c = s.summary;
  $("counts").textContent =
    `Today: ${c.candidates} candidates, ${c.rejections} rejected, ${c.orders} orders, ${c.fills} fills, ${c.exits} exits`;

  rows($("positions"), s.positions, (p) => [[p.symbol], [p.qty, "n"], [p.avg_entry_price, "n"]], "None", 3);
  rows($("orders"), s.orders,
    (o) => [[`${o.symbol} ${o.client_order_id}`], [o.side], [o.status], [`${o.filled_qty}/${o.qty}`, "n"]], "None", 4);
  rows($("trials"), s.trials, (t) => [
    [t.strategy], [t.period], [t.trades, "n"],
    [`${t.expectancy_r.toFixed(3)}R`, "n"], [t.ci_low === null ? "–" : `${t.ci_low.toFixed(3)}R`, "n"],
    [t.profit_factor === null ? "–" : t.profit_factor.toFixed(2), "n"],
  ], "No trials yet", 6);

  const j = $("journal");
  j.replaceChildren();
  for (const l of [...s.summary.recent].reverse()) {
    const li = el("li");
    const t = el("time", new Date(l.ts * 1000).toLocaleTimeString("en-US", { timeZone: "America/New_York", hour12: false }));
    li.append(t, el("span", l.kind, "kind"), el("span", [l.id, l.detail].filter(Boolean).join("  "), "detail"));
    j.append(li);
  }
  if (!j.children.length) j.append(el("li", "Nothing logged today."));
}

async function api(path, opts = {}, timeoutMs = 4000) {
  const res = await fetch(path, {
    ...opts,
    signal: AbortSignal.timeout(timeoutMs),
    headers: { Authorization: `Bearer ${token}`, ...(opts.headers || {}) },
    cache: "no-store",
  });
  const body = await res.json().catch(() => ({ message: `HTTP ${res.status}` }));
  return { ok: res.ok, status: res.status, body };
}

let polling = false;

async function poll() {
  if (polling) return; // one reading at a time
  polling = true;
  try {
    const r = await api("/api/state");
    if (r.status === 401) {
      clearView();
      setSignal("No access", "off", "Open the URL the ui command printed in the terminal.");
    } else if (!r.ok) {
      clearView();
      setSignal("No reading", "off", r.body.message || `HTTP ${r.status}`);
    } else {
      render(r.body);
    }
  } catch (e) {
    // Never keep showing old numbers as if they were current.
    clearView();
    const why = e instanceof TypeError || e.name === "TimeoutError"
      ? "Can't reach the console. Is `ui` still running?"
      : `This reading couldn't be shown (${e.message}).`;
    setSignal("No reading", "off", why);
  } finally {
    polling = false;
  }
}

for (const form of document.querySelectorAll(".control")) {
  const input = form.querySelector("input");
  const button = form.querySelector("button");
  input.addEventListener("input", () => {
    button.disabled = input.value !== form.dataset.word;
  });
  form.addEventListener("submit", async (ev) => {
    ev.preventDefault();
    if (input.value !== form.dataset.word) return;
    button.disabled = true;
    const result = $("result");
    result.className = "result";
    result.textContent = form.dataset.action === "flatten" ? "Closing everything…" : "Stopping…";
    try {
      // flatten checks the account up to 10 times, so give it time
      const r = await api(`/api/${form.dataset.action}`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ confirm: input.value }),
      }, 120000);
      result.textContent = r.body.message;
      if (!r.ok) result.classList.add("bad");
    } catch (_) {
      result.textContent = "Couldn't reach the console; check the terminal or Alpaca to see what happened.";
      result.classList.add("bad");
    }
    input.value = "";
    setTimeout(poll, 1000); // the server is fetching a fresh reading
  });
}

// Trial 10 and the equity chart change once a night; the server reads them every 60 s.
const SLOW_POLL_MS = 60000;
const RATING = { Buy: "up", Overweight: "up", Underweight: "down", Sell: "down" };

function stat(dl, label, value, fill) {
  const d = el("div");
  d.append(el("dt", label), el("dd", value));
  if (fill !== undefined) {
    const m = el("div", undefined, "meter");
    const bar = el("span");
    bar.style.width = `${Math.min(100, Math.max(0, fill * 100)).toFixed(1)}%`; // CSSOM, allowed by the CSP
    m.append(bar);
    d.append(m);
  }
  dl.append(d);
}

function renderDesk(p) {
  const dl = $("desk");
  dl.replaceChildren();
  stat(dl, "Rated signals", `${p.rated} of ${p.target}`, p.rated / p.target);
  stat(dl, "Spend this month", `$${p.spent_month.toFixed(2)} of $${p.cap_month.toFixed(0)}`, p.spent_month / p.cap_month);
  stat(dl, "Trades", `${p.open} open, ${p.done} finished`);
  stat(dl, p.verdict === "not due" ? "Verdict due by" : "Verdict",
    p.verdict === "ready" ? "Ready to judge" : p.verdict === "waiting" ? "Trades still open" : p.due_by ?? "–");
  const extra = [];
  if (p.failed) extra.push(`${p.failed} failed`);
  if (p.unanalysed) extra.push(`${p.unanalysed} not analysed`);
  if (p.queued) extra.push(`${p.queued} queued`);
  if (p.missed.length) extra.push(`missed nights: ${p.missed.join(", ")}`);
  $("desk-note").textContent = [
    "Counts only. Results by rating stay hidden until the verdict.",
    extra.length ? `Also: ${extra.join("; ")}.` : "",
  ].join(" ");

  const ul = $("night");
  ul.replaceChildren();
  if (!p.night) {
    $("night-date").textContent = "No night analysed yet.";
    return;
  }
  const [date, rows] = p.night;
  $("night-date").textContent = `Signals from the ${date} close`;
  for (const [symbol, rating] of rows) {
    const li = el("li");
    li.append(el("span", symbol), el("span", rating, `badge ${RATING[rating] ?? ""}`));
    ul.append(li);
  }
}

let chart = null;
let series = null;
let hist = { equity: [], spy: [] };
let rangeDays = 366;

function drawChart() {
  if (!window.LightweightCharts) {
    $("chart-note").textContent = "The chart library didn't load.";
    return;
  }
  if (!chart) {
    const LC = window.LightweightCharts;
    chart = LC.createChart($("chart"), {
      autoSize: true,
      layout: { background: { type: "solid", color: "#1a2027" }, textColor: "#a3aed0", fontFamily: "system-ui, sans-serif" },
      grid: { vertLines: { color: "#232c37" }, horzLines: { color: "#232c37" } },
      rightPriceScale: { borderColor: "#26303b" },
      timeScale: { borderColor: "#26303b" },
    });
    series = {
      spy: chart.addSeries(LC.LineSeries, { color: "#a3aed0", lineWidth: 2 }),
      equity: chart.addSeries(LC.LineSeries, { color: "#76da44", lineWidth: 2 }),
    };
  }
  const from = new Date(Date.now() - rangeDays * 864e5).toISOString().slice(0, 10);
  const cut = (xs) => xs.filter((x) => x.time >= from);
  series.equity.setData(cut(hist.equity));
  series.spy.setData(cut(hist.spy));
  chart.timeScale().fitContent();
  $("chart-note").textContent = hist.equity.length
    ? "Both lines start at 100 on the first day they share."
    : "No account history yet.";
}

for (const b of document.querySelectorAll(".ranges button")) {
  b.addEventListener("click", () => {
    for (const o of document.querySelectorAll(".ranges button")) {
      o.classList.toggle("is-active", o === b);
      o.setAttribute("aria-pressed", String(o === b));
    }
    rangeDays = Number(b.dataset.days);
    drawChart();
  });
}

const fmt = (x, units) => units === "R" ? `${x >= 0 ? "+" : ""}${x.toFixed(3)}R`
  : units === "monthly IC" ? `IC ${x.toFixed(3)}` : `${x >= 0 ? "+" : ""}${(x * 100).toFixed(2)}%/mo`;

function renderLessons(list) {
  const tb = $("lessons");
  tb.replaceChildren();
  for (const l of list) {
    const tr = el("tr");
    const name = el("td");
    name.append(el("strong", `${l.trial}`), document.createTextNode(` ${l.name}`), el("div", l.period, "years"));
    const result = el("td", undefined, "n");
    if (!l.reproduces) {
      result.append(el("span", "doesn't reproduce", "badge down"));
    } else {
      const a = l.autopsy;
      result.append(el("div", fmt(a.net, l.units), a.net > 0 ? "pos" : "neg"));
      if (a.gross !== null && a.gross !== undefined) result.append(el("div", `before costs ${fmt(a.gross, l.units)}`, "years"));
    }
    const lost = el("td");
    if (l.reproduces) {
      const flags = el("div", undefined, "flags");
      for (const f of l.autopsy.flags) flags.append(el("span", f.split(":")[0], "badge down"));
      if (!l.autopsy.flags.length) flags.append(el("span", "no single cause", "badge"));
      lost.append(flags);
      const years = Object.entries(l.autopsy.by_year).map(([y, g]) => `${y} ${fmt(g.mean, l.units)}`).join("  ");
      lost.append(el("div", years, "years"));
    }
    tr.append(name, result, lost, el("td", l.lesson ?? ""), el("td", `${l.next ?? ""} (trial ${l.acted_on_by ?? "open"})`));
    tb.append(tr);
  }
  if (!list.length) tb.append(el("tr", undefined, "empty"));
}

async function slowPoll() {
  api("/api/lessons").then((r) => {
    if (r.ok) renderLessons(r.body);
    else {
      const tb = $("lessons");
      const tr = el("tr", undefined, "empty");
      const td = el("td", r.body.message);
      td.colSpan = 5;
      tr.append(td);
      tb.replaceChildren(tr);
    }
  }).catch(() => {});
  try {
    const [d, e] = await Promise.all([api("/api/desk"), api("/api/equity")]);
    if (d.ok) renderDesk(d.body);
    else $("desk-note").textContent = `Couldn't read trial 10: ${d.body.message}`;
    if (e.ok) {
      hist = e.body;
      drawChart();
    } else {
      $("chart-note").textContent = `Couldn't read the history: ${e.body.message}`;
    }
  } catch (_) {
    $("desk-note").textContent = "Can't reach the console.";
  }
}

// Overview and Research tabs (ARIA tablist); the last one is remembered for this tab's session.
const tabs = [...document.querySelectorAll('[role="tab"]')];
function showTab(tab, focus) {
  for (const t of tabs) {
    const on = t === tab;
    t.setAttribute("aria-selected", String(on));
    t.tabIndex = on ? 0 : -1;
    $(t.getAttribute("aria-controls")).hidden = !on;
  }
  if (focus) tab.focus();
  try {
    sessionStorage.setItem("bmv-tab", tab.id.replace("tab-", ""));
  } catch (_) {}
}
for (const t of tabs) {
  t.addEventListener("click", () => showTab(t));
  t.addEventListener("keydown", (ev) => {
    const i = tabs.indexOf(t);
    const next = { ArrowRight: i + 1, ArrowLeft: i - 1, Home: 0, End: tabs.length - 1 }[ev.key];
    if (next === undefined) return;
    ev.preventDefault();
    showTab(tabs[(next + tabs.length) % tabs.length], true);
  });
}
try {
  const saved = $(`tab-${sessionStorage.getItem("bmv-tab")}`);
  if (saved) showTab(saved);
} catch (_) {}

const clock = () => {
  $("clock").textContent = `${new Date().toLocaleTimeString("en-US", { timeZone: "America/New_York", hour12: false })} ET`;
};
clock();
setInterval(clock, 1000);

poll();
setInterval(poll, POLL_MS);
slowPoll();
setInterval(slowPoll, SLOW_POLL_MS);
