// Renders /api/services, plus Run-now for the launchd jobs that place no orders (DECISION S24).
// Text goes in with textContent only (a test checks).
"use strict";

const POLL_MS = 10000;
// The server checks every 30 s; a report older than this means the console is stuck.
const STALE_SECS = 90;

// Same token as the console tab (sessionStorage), or one in the URL fragment.
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

const DOT = { ok: "go", warn: "caution", down: "danger", unknown: "off" };
const WORD = { ok: "Up", warn: "Check", down: "Down", unknown: "Unknown" };

function setSignal(state, aspect, meta) {
  $("state").textContent = state;
  $("meta").textContent = meta;
  $("signal").dataset.aspect = aspect;
  $("state-word").textContent = state;
  $("state-pill").dataset.aspect = aspect;
}

// Overview and Research live on "/": remember which one to open there.
for (const a of document.querySelectorAll("a[data-tab]")) {
  a.addEventListener("click", () => {
    try {
      sessionStorage.setItem("bmv-tab", a.dataset.tab);
    } catch (_) {}
  });
}

const clock = () => {
  $("clock").textContent = `${new Date().toLocaleTimeString("en-US", { timeZone: "America/New_York", hour12: false })} ET`;
};
clock();
setInterval(clock, 1000);

const NAMES = {
  desk: "desk (trial 10 analysis)",
  notes: "notes (local digests)",
  llm: "LLM key rotation",
  ollama: "Ollama",
  omniroute: "OmniRoute",
};
const VERB = { start: "Start", run: "Run", restart: "Restart" };
const WORDS = { start: "START", run: "RUN", restart: "RESTART" };
const WARN = {
  "restart desk": "Stops tonight's analysis mid-signal. It resumes on the next run, but that signal's Claude spend is lost.",
  "run desk": "Calls Claude (about $0.10–0.80 a night, within its budget) and does nothing before 20 minutes after the close.",
  "restart ollama": "Notes, lab ideas and trial 12 lose the model for about 10 s. A running trial 12 stops after 3 failures and resumes when re-run.",
  "restart omniroute": "Notes and lab ideas fail for about 10 s. Nothing trades.",
};

// The one confirm bar: a row button fills it in; the server checks the word again.
let pending = null;
function ask(target, action) {
  pending = { target, action };
  const bar = $("run");
  $("confirm-title").textContent = `${VERB[action]} ${NAMES[target] ?? target}?`;
  $("confirm-warning").textContent = WARN[`${action} ${target}`] ?? "Nothing here places an order.";
  $("confirm-label").textContent = `Type ${WORDS[action]} to confirm`;
  bar.confirm.value = "";
  bar.hidden = false;
  checkRun();
  bar.confirm.focus();
}

function actionsCell(c) {
  const td = el("td", undefined, "n actions");
  for (const a of c.actions ?? []) {
    const b = el("button", VERB[a] ?? a, a === "restart" ? "small quiet" : "small");
    b.type = "button";
    b.addEventListener("click", () => ask(c.target, a));
    td.append(b);
  }
  return td;
}

function render(r) {
  const tbody = $("components");
  tbody.replaceChildren();
  const count = { ok: 0, warn: 0, down: 0, unknown: 0 };
  for (const c of r.components) {
    count[c.status] = (count[c.status] ?? 0) + 1;
    const tr = el("tr");
    const status = el("td");
    status.append(el("span", undefined, `dot ${DOT[c.status] ?? "off"}`), el("span", WORD[c.status] ?? c.status));
    const fix = el("td");
    if (c.fix) fix.append(el("code", c.fix));
    tr.append(status, el("td", c.name), el("td", c.detail), fix, actionsCell(c));
    tbody.append(tr);
  }
  const when = new Date(r.now * 1000).toLocaleTimeString("en-US", { timeZone: "America/New_York", hour12: false });
  const age = Math.round(Date.now() / 1000 - r.now);
  const meta = `${count.ok} up, ${count.warn} to check, ${count.down} down. Checked ${when} ET`;
  if (age > STALE_SECS) setSignal("Stale check", "off", `${meta}, ${age} s ago; nothing here is current.`);
  else if (count.down) setSignal("Something is down", "danger", meta);
  else if (count.warn || count.unknown) setSignal("Needs a look", "caution", meta);
  else setSignal("All up", "go", meta);
}

let polling = false;

async function poll() {
  if (polling) return;
  polling = true;
  try {
    const res = await fetch("/api/services", {
      signal: AbortSignal.timeout(4000),
      headers: { Authorization: `Bearer ${token}` },
      cache: "no-store",
    });
    const body = await res.json().catch(() => ({ message: `HTTP ${res.status}` }));
    if (res.status === 401) {
      $("components").replaceChildren();
      setSignal("No access", "off", "Open the URL the ui command printed, then follow the Services link.");
    } else if (!res.ok) {
      setSignal("No check yet", "off", body.message || `HTTP ${res.status}`);
    } else {
      render(body);
    }
  } catch (_) {
    $("components").replaceChildren();
    setSignal("No reading", "off", "Can't reach the console. Is `ui` still running?");
  } finally {
    polling = false;
  }
}

const form = $("run");
const runButton = form.querySelector('button[type="submit"]');
function checkRun() {
  runButton.disabled = !(pending && form.confirm.value === WORDS[pending.action]);
}
form.addEventListener("input", checkRun);
$("confirm-cancel").addEventListener("click", () => {
  pending = null;
  form.hidden = true;
});
form.addEventListener("submit", async (ev) => {
  ev.preventDefault();
  if (runButton.disabled || !pending) return;
  runButton.disabled = true;
  const result = $("run-result");
  result.className = "result";
  result.textContent = `${VERB[pending.action]} ${NAMES[pending.target] ?? pending.target}: sent…`;
  try {
    const res = await fetch("/api/run", {
      method: "POST",
      signal: AbortSignal.timeout(90000),
      headers: { Authorization: `Bearer ${token}`, "Content-Type": "application/json" },
      body: JSON.stringify({ target: pending.target, action: pending.action, confirm: form.confirm.value }),
      cache: "no-store",
    });
    const body = await res.json().catch(() => ({ message: `HTTP ${res.status}` }));
    result.textContent = body.message;
    if (!res.ok) result.classList.add("bad");
  } catch (_) {
    result.textContent = "Couldn't reach the console; check its terminal.";
    result.classList.add("bad");
  }
  pending = null;
  form.confirm.value = "";
  form.hidden = true;
  setTimeout(poll, 2000);
});

poll();
setInterval(poll, POLL_MS);
