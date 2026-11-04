// Renders /api/services. Read-only: no controls (DECISION S16). Text goes in with textContent only (a test checks).
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

poll();
setInterval(poll, POLL_MS);
