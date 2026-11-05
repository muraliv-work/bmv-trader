"""Makes Paperclip match the agents file (DESIGN §4c, S38). Run as you, after clip-agents-setup.sh:

    target/clip/node/bin/node target/clip/app/node_modules/paperclipai/dist/index.js \\
        auth login --api-base http://127.0.0.1:3100        # once: approve in the browser
    python3 -I scripts/clip-agents-sync.py

It creates the company "bmv Quant Desk" if missing, then one Paperclip agent per desk agent on the
`process` adapter, running desk-agent (signers through their one sudo rule), one run at a time,
with Paperclip's budget set to the desk's. Scheduled agents get a routine, created paused: nothing
runs until you unpause it. Reruns update what exists, matched by desk agent id, including each
routine's schedule; they never unpause a routine, and delete nothing.
"""

import json
import os
import subprocess
import sys
from decimal import Decimal

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
NODE = os.path.join(ROOT, "target/clip/node/bin/node")
CLI = os.path.join(ROOT, "target/clip/app/node_modules/paperclipai/dist/index.js")
API = "http://127.0.0.1:3100"
DESK_AGENT = "/usr/local/libexec/bmv-desk/desk-agent"
AGENTS = "/usr/local/etc/bmv-clip/agents.json"
CLIP_USER = "bmvclip"
COMPANY = "bmv Quant Desk"
ROLES = {"head_of_desk": ("Head of Desk", "general"), "researcher": ("Researcher", "researcher"),
         "validator": ("Validator", "qa"), "stress_lead": ("Stress Lead", "qa")}


def cli(*args):
    """One Paperclip CLI call, with its telemetry and update checks off; returns its JSON."""
    env = {"HOME": os.environ["HOME"], "PATH": "/usr/bin:/bin", "PAPERCLIP_TELEMETRY_DISABLED": "1",
           "DO_NOT_TRACK": "1", "PAPERCLIP_UPDATE_CHECK": "0",
           "PAPERCLIP_ANNOUNCEMENTS_ENABLED": "false"}
    r = subprocess.run([NODE, CLI, *args, "--api-base", API, "--json"],
                       capture_output=True, text=True, env=env)
    if r.returncode != 0:
        sys.exit(f"clip-agents-sync: paperclipai {args[0]} {args[1] if len(args) > 1 else ''} failed:\n"
                 f"{r.stderr.strip() or r.stdout.strip()}")
    out = r.stdout.strip()
    return json.loads(out) if out else None


def payload(a):
    name, role = ROLES[a["role"]]
    command = [DESK_AGENT, AGENTS, a["id"], a["keys"]]
    if a["runs_as"] != CLIP_USER:
        # exactly the command its sudo rule allows; -n so a missing rule fails instead of asking
        command = ["/usr/bin/sudo", "-n", "-u", a["runs_as"], *command]
    return {
        "name": f"{name} ({a['id']})",
        "role": role,
        "title": f"{a['id']}: {a['provider']} / {a['model']}",
        "adapterType": "process",
        "adapterConfig": {"command": command[0], "args": command[1:], "cwd": "/",
                          "timeoutSec": 600, "graceSec": 15},
        "runtimeConfig": {"heartbeat": {"enabled": False, "wakeOnDemand": True,
                                        "maxConcurrentRuns": 1}},
        "budgetMonthlyCents": int(Decimal(a["budget_usd"]) * 100),
        "metadata": {"bmvAgent": a["id"]},
    }


def schedule(rid, cron):
    """Gives the routine one schedule trigger on `cron`, New York time: made if missing (also
    after a run that stopped between making the routine and its trigger), updated if changed."""
    triggers = [t for t in (cli("routine", "get", rid) or {}).get("triggers") or []
                if t.get("kind") == "schedule" and not t.get("archived")]
    tz = "America/New_York"
    if not triggers:
        cli("routine", "trigger:create", rid, "--payload-json", json.dumps({
            "kind": "schedule", "cronExpression": cron, "timezone": tz}))
        print(f"    schedule set: '{cron}' New York time")
    elif (triggers[0].get("cronExpression"), triggers[0].get("timezone")) != (cron, tz):
        cli("routine", "trigger:update", triggers[0]["id"], "--payload-json", json.dumps({
            "cronExpression": cron, "timezone": tz}))
        print(f"    schedule changed to '{cron}' New York time")


def main():
    for p in (NODE, CLI):
        if not os.path.exists(p):
            sys.exit(f"clip-agents-sync: {p} is missing; run scripts/clip-build.sh first")
    listed = subprocess.run([DESK_AGENT, "--list", AGENTS], capture_output=True, text=True)
    if listed.returncode != 0:
        sys.exit(f"clip-agents-sync: {listed.stderr.strip()}")
    agents = json.loads(listed.stdout)
    unknown = [a["id"] for a in agents if a["role"] not in ROLES]
    if unknown:
        sys.exit(f"clip-agents-sync: no Paperclip setup for the roles of {', '.join(unknown)}")
    if subprocess.run([NODE, CLI, "auth", "whoami", "--api-base", API],
                      capture_output=True, env={"HOME": os.environ["HOME"], "PATH": "/usr/bin:/bin",
                                                "DO_NOT_TRACK": "1"}).returncode != 0:
        sys.exit("clip-agents-sync: the Paperclip CLI isn't signed in; run the auth login line at the "
                 "top of this script first")

    company = next((c for c in cli("company", "list") or [] if c.get("name") == COMPANY), None)
    if company is None:
        company = cli("company", "create", "--payload-json",
                      json.dumps({"name": COMPANY, "description": "Paper-trading research desk (DESIGN §4c)",
                                  "budgetMonthlyCents": 10000}))
        print(f"made company {COMPANY}")
    cid = company["id"]

    existing = {(x.get("metadata") or {}).get("bmvAgent"): x for x in cli("agent", "list", "-C", cid) or []}
    routines = {r.get("title"): r for r in cli("routine", "list", "-C", cid) or []}
    for a in agents:
        body = payload(a)
        if a["id"] in existing:
            pid = existing[a["id"]]["id"]
            cli("agent", "update", pid, "--payload-json", json.dumps(body))
            print(f"updated {a['id']} ({body['name']})")
        else:
            pid = cli("agent", "create", "-C", cid, "--payload-json", json.dumps(body))["id"]
            print(f"made {a['id']} ({body['name']})")
        if a["schedule"]:
            title = f"{a['id']} scheduled run"
            if title in routines:
                rid = routines[title]["id"]
                print(f"  routine '{title}' exists (status {routines[title].get('status')}, left as is)")
            else:
                rid = cli("routine", "create", "-C", cid, "--payload-json", json.dumps({
                    "title": title, "assigneeAgentId": pid, "status": "paused",
                    "description": "Wakes the agent on its schedule; it runs its agents-file task."}))["id"]
                print(f"  routine '{title}' made, paused")
            schedule(rid, a["schedule"])
    print("Paperclip matches the agents file. Nothing runs until you unpause a routine or wake an agent.")


if __name__ == "__main__":
    main()
