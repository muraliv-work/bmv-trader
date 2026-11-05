// Renders the Quant Desk page (DECISION S47): /api/desk/snapshot and /api/keys, read-only. Approving
// stays in Terminal with `desk sign` and Touch ID. Text goes in with textContent only (a test checks).
"use strict";

const POLL_MS = 30000;
// The server reads the desk every 2 minutes; a reading older than this means it's stuck or down.
const STALE_SECS = 360;
// No attempt at all for this long means the console's reader has stopped, not the desk.
const STOPPED_SECS = 300;
const ageSecs = (t) => (t ? (Date.now() - new Date(t)) / 1000 : Infinity);

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
  if (text !== undefined && text !== null) e.textContent = text;
  if (cls) e.className = cls;
  return e;
};
const row = (cells) => {
  const tr = el("tr");
  for (const c of cells) tr.append(c instanceof Node ? c : el("td", c));
  return tr;
};
const td = (child, cls) => {
  const t = el("td", undefined, cls);
  t.append(child);
  return t;
};
const emptyRow = (cols, text) => {
  const tr = el("tr", undefined, "empty");
  const t = el("td", text);
  t.colSpan = cols;
  tr.append(t);
  return tr;
};
const emptyItem = (text) => el("li", text, "empty-item");

// Overview and Research live on "/": remember which one to open there.
for (const a of document.querySelectorAll("a[data-tab]")) {
  a.addEventListener("click", () => {
    try {
      sessionStorage.setItem("bmv-tab", a.dataset.tab);
    } catch (_) {}
  });
}

const ET = { timeZone: "America/New_York", hour12: false };
const clock = () => {
  $("clock").textContent = `${new Date().toLocaleTimeString("en-US", ET)} ET`;
};
clock();
setInterval(clock, 1000);
const when = (t) => (t ? `${new Date(t).toLocaleString("en-US", { ...ET, dateStyle: "medium", timeStyle: "short" })} ET` : "never");

// The lifecycle in order (crates/desk/src/lifecycle.rs), then the two off-path states.
const STAGES = [
  ["idea", "Idea"],
  ["hypothesis_approved", "Hypothesis approved"],
  ["spec_frozen", "Spec frozen"],
  ["implemented", "Implemented"],
  ["validated", "Validated"],
  ["stress_tested", "Stress tested"],
  ["limits_set", "Limits set"],
  ["compliance_cleared", "Compliance cleared"],
  ["paper_trading", "Paper trading"],
  ["paper_reviewed", "Paper reviewed"],
  ["live_approved", "Live approved"],
  ["live", "Live"],
  ["live_full_size", "Live, full size"],
  ["halted", "Halted"],
  ["retired", "Retired"],
];
const RANK = Object.fromEntries(STAGES.map(([k], i) => [k, i]));
const LABEL = Object.fromEntries(STAGES);
// Colour carries meaning only; the badge's words always say it too.
const TONE = (s) => (s === "halted" ? "down" : s.startsWith("live") ? "live" : RANK[s] >= RANK.validated ? "up" : "");

for (const [k, l] of STAGES) {
  const o = el("option", l);
  o.value = k;
  $("stage").append(o);
}

function setSignal(state, aspect, meta) {
  $("state").textContent = state;
  $("meta").textContent = meta;
  $("signal").dataset.aspect = aspect;
  $("state-word").textContent = state;
  $("state-pill").dataset.aspect = aspect;
}

function badge(text, tone) {
  return el("span", text, `badge${tone ? ` ${tone}` : ""}`);
}

// The criteria column: what the desk's own check said, never a number typed by an agent.
function criteria(ev) {
  if (!ev || ev.error) return badge(ev?.error ? "Can't read" : "No evidence", "");
  if (!ev.evidence) return badge("No evidence", "");
  const e = ev.evidence;
  if (!e.checkable) return badge("Not checkable", "warn");
  return e.all_criteria_pass ? badge("All pass", "up") : badge("Not all pass", "down");
}

let snap = null;
let sortKey = "id";
let sortUp = true;

function renderStrategies() {
  const body = $("strategies");
  const list = (snap?.data?.strategies ?? []).filter((s) => !$("stage").value || s.state === $("stage").value);
  const key = (s) => (sortKey === "rank" ? RANK[s.state] ?? 99 : s[sortKey]);
  list.sort((a, b) => (key(a) < key(b) ? -1 : key(a) > key(b) ? 1 : 0) * (sortUp ? 1 : -1));
  for (const b of document.querySelectorAll("button.sort")) {
    b.closest("th").setAttribute("aria-sort", b.dataset.key === sortKey ? (sortUp ? "ascending" : "descending") : "none");
  }
  if (!list.length) {
    body.replaceChildren(emptyRow(5, snap?.data ? "No strategies at this stage yet. The Researcher files new ideas as Idea." : "No reading yet."));
    return;
  }
  body.replaceChildren(
    ...list.map((s) => {
      const open = el("button", s.name, "link");
      open.type = "button";
      open.addEventListener("click", () => detail(s));
      return row([
        el("td", String(s.id), "n"),
        td(open),
        td(badge(LABEL[s.state] ?? s.state, TONE(s.state))),
        el("td", String(s.version), "n"),
        td(criteria(snap.data.evidence?.[s.id])),
      ]);
    }),
  );
}

for (const b of document.querySelectorAll("button.sort")) {
  b.addEventListener("click", () => {
    sortUp = sortKey === b.dataset.key ? !sortUp : true;
    sortKey = b.dataset.key;
    renderStrategies();
  });
}
$("stage").addEventListener("change", renderStrategies);

function field(dl, name, value) {
  dl.append(el("dt", name), el("dd", value ?? "none"));
}

// The drawer: one strategy's evidence as the desk computed it, with its hashes.
function detail(s) {
  const ev = snap.data.evidence?.[s.id] ?? {};
  $("detail-title").textContent = `#${s.id} ${s.name}`;
  const body = $("detail-body");
  const dl = el("dl", undefined, "facts");
  field(dl, "Stage", LABEL[s.state] ?? s.state);
  field(dl, "Version", String(s.version));
  const parts = [dl];
  if (ev.error) {
    parts.push(el("p", `The desk didn't return its evidence: ${ev.error}`, "note"));
  } else {
    parts.push(el("h3", "Pre-registered criteria"));
    parts.push(el("pre", ev.criteria ?? "None set yet. The Strategist sets them before the spec is frozen."));
    const e = ev.evidence;
    parts.push(el("h3", "Evidence"));
    if (!e) {
      parts.push(el("p", "No evidence for this version yet. The engine posts it with bmv-trader desk-evidence.", "note"));
    } else {
      const ed = el("dl", undefined, "facts");
      field(ed, "Recorded", when(e.recorded));
      field(ed, "Results hash", e.results_hash);
      field(ed, "Inputs hash", e.inputs_hash);
      field(ed, "Criteria", e.checkable ? (e.all_criteria_pass ? "all pass" : "not all pass") : "not checkable");
      parts.push(ed);
      const checks = e.criteria_checks ?? [];
      if (checks.length) {
        const t = el("table");
        const head = el("thead");
        head.append(row(["Metric", "Rule", "Value", "Result"].map((h) => el("th", h))));
        const tb = el("tbody");
        for (const c of checks) {
          const res = c.passed === true ? badge("Pass", "up") : c.passed === false ? badge("Fail", "down") : badge("Missing", "warn");
          tb.append(row([c.metric, c.rule, c.value === null ? "missing" : String(c.value), td(res)]));
        }
        t.append(head, tb);
        const wrap = el("div", undefined, "scroll");
        wrap.append(t);
        parts.push(wrap);
      }
      const m = e.body?.metrics ?? {};
      if (Object.keys(m).length) {
        parts.push(el("h3", "All metrics"));
        const md = el("dl", undefined, "facts");
        for (const [k, v] of Object.entries(m)) field(md, k, String(v));
        parts.push(md);
      }
    }
    parts.push(el("h3", "Sign-offs on this version"));
    const signed = ev.signatures_this_version ?? [];
    const ul = el("ul", undefined, "night");
    for (const g of signed) ul.append(el("li", `${g.by}: ${LABEL[g.to] ?? g.to}`));
    if (!signed.length) ul.append(emptyItem("None yet."));
    parts.push(ul);
  }
  body.replaceChildren(...parts);
  $("detail").showModal();
}
$("detail-close").addEventListener("click", () => $("detail").close());

function renderRequests() {
  const list = snap?.data?.requests ?? [];
  $("requests-count").textContent = list.length ? String(list.length) : "";
  if (!list.length) {
    $("requests").replaceChildren(emptyItem("Nothing is waiting for your approval."));
    return;
  }
  $("requests").replaceChildren(
    ...list.map((r) => {
      const li = el("li", undefined, "pack");
      const head = el("div", undefined, "pack-head");
      head.append(el("strong", `Request ${r.id}`), el("span", `from ${r.requested_by}, expires ${when(r.expires)}`, "note"));
      // the exact text desk sign shows before Touch ID
      li.append(head, el("pre", r.text), el("p", `In Terminal: desk sign ${r.id}`, "note"));
      return li;
    }),
  );
}

function renderHalts() {
  const halted = (snap?.data?.strategies ?? []).filter((s) => s.state === "halted");
  $("halts-count").textContent = halted.length ? String(halted.length) : "";
  $("halts").replaceChildren(...(halted.length ? halted.map((s) => el("li", `#${s.id} ${s.name}`)) : [emptyItem("No strategy is halted.")]));
}

function renderAudit() {
  const a = snap?.data?.audit;
  const p = $("audit");
  if (!a) {
    p.textContent = "No reading yet.";
    p.className = "audit";
  } else if (a.error) {
    p.textContent = `Not verified: ${a.error}. Don't approve anything until the break is explained; the desk log names the entry.`;
    p.className = "audit bad";
  } else {
    p.textContent = `Intact: ${a.entries} entries, checked ${when(snap.as_of)}.`;
    p.className = "audit good";
  }
}

function renderBudgets() {
  const list = snap?.data?.budgets ?? [];
  if (!list.length) {
    $("budgets").replaceChildren(emptyItem(snap?.data ? "No budgets this month. Set them with scripts/desk-wave1.sh budgets." : "No reading yet."));
    return;
  }
  $("budgets").replaceChildren(
    ...list.map((b) => {
      const limit = Number(b.limit_usd);
      const spent = Number(b.spent_usd);
      const share = limit > 0 ? Math.min(1, spent / limit) : 1;
      const li = el("li");
      const top = el("div", undefined, "budget-head");
      top.append(el("strong", b.agent), el("span", `$${spent.toFixed(4)} of $${limit.toFixed(2)}`));
      const meter = el("div", undefined, `meter${share >= 0.9 ? " high" : ""}`);
      meter.setAttribute("role", "meter");
      meter.setAttribute("aria-label", `${b.agent} budget used`);
      meter.setAttribute("aria-valuemin", "0");
      meter.setAttribute("aria-valuemax", "100");
      meter.setAttribute("aria-valuenow", String(Math.round(share * 100)));
      const fill = el("span");
      fill.style.width = `${share * 100}%`;
      meter.append(fill);
      li.append(top, meter);
      return li;
    }),
  );
}

async function get(path) {
  const r = await fetch(path, { headers: { Authorization: `Bearer ${token}` }, cache: "no-store" });
  const body = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(body.message ?? `HTTP ${r.status}`);
  return body;
}

function warn(text) {
  const problem = $("problem");
  problem.hidden = !text;
  problem.className = "banner danger";
  problem.textContent = text ?? "";
}

async function loadDesk() {
  try {
    snap = await get("/api/desk/snapshot");
  } catch (e) {
    setSignal("Not reading", "danger", e.message);
    warn(`This page can't reach the console (${e.message}). What's below is from an earlier reading; don't act on it.`);
    return;
  }
  const d = snap.data;
  // parts of a reading that worked can still be broken: say so up top, not only in their card
  const broken = [];
  if (d?.audit?.error) broken.push("the audit trail can't be verified");
  const unread = Object.values(d?.evidence ?? {}).filter((e) => e.error).length;
  if (unread) broken.push(`${unread} strateg${unread === 1 ? "y's" : "ies'"} evidence can't be read`);
  if (ageSecs(snap.checked) > STOPPED_SECS) {
    warn(`The console stopped reading the desk at ${when(snap.checked)}. Restart bmv-trader ui; what's below may be old.`);
  } else if (snap.error) {
    warn(d ? `The desk didn't answer at ${when(snap.checked)}: ${snap.error}. Showing its last reading.` : `No reading of the desk yet: ${snap.error}`);
  } else {
    warn(broken.length ? `In the latest reading, ${broken.join(" and ")}.` : null);
  }
  if (!d) setSignal("No reading", "danger", "The desk hasn't been read since this console started");
  else if (snap.error || ageSecs(snap.as_of) > STALE_SECS || ageSecs(snap.checked) > STOPPED_SECS) setSignal("Out of date", "caution", `Last reading ${when(snap.as_of)}`);
  else if (broken.length) setSignal("Desk read, with problems", "caution", `As of ${when(snap.as_of)}; see the warning above`);
  else setSignal("Desk read", "go", `As of ${when(snap.as_of)}; read every 2 minutes`);
  renderRequests();
  renderStrategies();
  renderHalts();
  renderAudit();
  renderBudgets();
}

async function loadKeys() {
  let table;
  try {
    table = await get("/api/keys");
  } catch (e) {
    $("keys-note").textContent = `Can't reach the console (${e.message}); the table below may be old.`;
    return;
  }
  // the last table that worked, with when it was read; a failed read keeps it and says why
  $("keys-note").textContent = table.error
    ? `Not updated at ${when(table.checked)}: ${table.error}.${table.data ? ` Showing the table from ${when(table.as_of)}.` : ""}`
    : `As of ${when(table.as_of)}.`;
  $("keys-note").className = table.error ? "note bad" : "note";
  const rows = table.data;
  if (!rows) {
    $("keys").replaceChildren(emptyRow(5, "No key table yet."));
    return;
  }
  const due = rows.filter((k) => k.needs_you).length;
  $("keys-count").textContent = due ? `${due} need you` : "";
  $("keys-count").className = due ? "badge down" : "badge";
  $("keys").replaceChildren(
    ...rows.map((k) => row([k.name, k.renewed ?? "", k.due ?? "", td(badge(k.state, k.needs_you ? "down" : "")), el("td", k.breaks, "wrap")])),
  );
}

if (!token) {
  setSignal("No token", "danger", "Open the URL bmv-trader ui printed, then come back to Desk");
} else {
  loadDesk();
  loadKeys();
  setInterval(loadDesk, POLL_MS);
  setInterval(loadKeys, POLL_MS);
}
