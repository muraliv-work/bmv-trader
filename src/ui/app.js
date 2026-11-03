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

// Nothing from an old reading stays on screen once a reading fails.
function clearView() {
  $("banners").replaceChildren();
  for (const id of ["equity", "pnl", "bp"]) money(undefined, id);
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
  const c = s.summary;
  $("counts").textContent =
    `Today: ${c.candidates} candidates, ${c.rejections} rejected, ${c.orders} orders, ${c.fills} fills, ${c.exits} exits`;

  rows($("positions"), s.positions, (p) => [[p.symbol], [p.qty, "n"], [p.avg_entry_price, "n"]], "None", 3);
  rows($("orders"), s.orders,
    (o) => [[`${o.symbol} ${o.client_order_id}`], [o.side], [o.status], [`${o.filled_qty}/${o.qty}`, "n"]], "None", 4);
  rows($("trials"), s.trials, (t) => [
    [t.strategy], [t.period], [t.trades, "n"],
    [`${t.expectancy_r.toFixed(3)}R`, "n"], [`${t.ci_low.toFixed(3)}R`, "n"], [t.profit_factor.toFixed(2), "n"],
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

poll();
setInterval(poll, POLL_MS);
