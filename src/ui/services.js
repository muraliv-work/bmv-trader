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
}

const NAMES = { desk: "Desk (trial 10 analysis)", notes: "Notes (local digests)", llm: "LLM key rotation" };

// Only jobs that are installed, loaded and not running now can be chosen.
function renderJobs(components) {
  const select = document.querySelector("#run select");
  const keep = select.value;
  const jobs = components.map((c) => c.run).filter(Boolean);
  select.replaceChildren(el("option", jobs.length ? "Choose" : "None can start now"));
  select.firstChild.value = "";
  for (const j of jobs) {
    const o = el("option", NAMES[j] ?? j);
    o.value = j;
    select.append(o);
  }
  select.value = jobs.includes(keep) ? keep : "";
  checkRun();
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
    tr.append(status, el("td", c.name), el("td", c.detail), fix);
    tbody.append(tr);
  }
  renderJobs(r.components);
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
const runButton = form.querySelector("button");
function checkRun() {
  runButton.disabled = !(form.job.value && form.confirm.value === "RUN");
}
form.addEventListener("input", checkRun);
form.addEventListener("change", checkRun);
form.addEventListener("submit", async (ev) => {
  ev.preventDefault();
  if (runButton.disabled) return;
  runButton.disabled = true;
  const result = $("run-result");
  result.className = "result";
  result.textContent = "Starting…";
  try {
    const res = await fetch("/api/run", {
      method: "POST",
      signal: AbortSignal.timeout(10000),
      headers: { Authorization: `Bearer ${token}`, "Content-Type": "application/json" },
      body: JSON.stringify({ job: form.job.value, confirm: form.confirm.value }),
      cache: "no-store",
    });
    const body = await res.json().catch(() => ({ message: `HTTP ${res.status}` }));
    result.textContent = body.message;
    if (!res.ok) result.classList.add("bad");
  } catch (_) {
    result.textContent = "Couldn't reach the console; check its terminal.";
    result.classList.add("bad");
  }
  form.confirm.value = "";
  checkRun();
  setTimeout(poll, 2000);
});

poll();
setInterval(poll, POLL_MS);
