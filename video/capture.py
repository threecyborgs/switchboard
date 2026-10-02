#!/usr/bin/env python3
"""Run the switchboard demo for real and write a storyboard for render.py from the captured output.

Two "machines" live on this Mac, each with its own HOME (own tailcat keys, own config, own launchd service, own
Claude Code / Codex registration); a third is the Linux box over ssh (optional). Every terminal line in the video is
what the command printed, except: the sandbox folder is shown as `~` (what a real install prints), the per-sandbox
service label suffix is dropped, and long invite codes typed into a pane are typed fast.

  python3 video/capture.py --out video/storyboard.json [--linux] [--codex]

Cleans up after itself: removes the sandbox services, stops its own processes, and leaves the folders for inspection.
"""
import argparse
import json
import os
import re
import shlex
import signal
import subprocess
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BIN = str(ROOT / "target/release/switchboard")
LABEL_RE = re.compile(r"com\.threecyborgs\.switchboard\.[0-9a-f]{8}")


class Machine:
    def __init__(self, base, name):
        self.home = Path(base) / name
        self.home.mkdir(parents=True, exist_ok=True)
        bindir = self.home / "bin"
        bindir.mkdir(exist_ok=True)
        link = bindir / "switchboard"
        if not link.exists():
            link.symlink_to(BIN)
        self.env = {**os.environ, "HOME": str(self.home), "SWITCHBOARD_HOME": str(self.home / ".switchboard"),
                    "PATH": f"{bindir}:{os.environ['PATH']}", "NO_COLOR": ""}
        for k in ("SWITCHBOARD_AS", "AGENT_MAIL_AS", "SWITCHBOARD_URL", "SWITCHBOARD_TOKEN"):
            self.env.pop(k, None)

    def clean(self, text):
        text = text.replace(str(self.home / ".switchboard/bin/switchboard"), "~/.switchboard/bin/switchboard")
        text = text.replace(str(self.home), "~")
        text = text.replace(str(Path(self.home).resolve()), "~")
        return LABEL_RE.sub("com.threecyborgs.switchboard", text).rstrip("\n")

    def sh(self, cmd, timeout=180):
        p = subprocess.run(cmd, shell=True, env=self.env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                           text=True, timeout=timeout, cwd=self.home)
        return self.clean(p.stdout), p.returncode

    def raw(self, cmd):
        return subprocess.run(cmd, shell=True, env=self.env, capture_output=True, text=True).stdout.strip()


class Board:
    def __init__(self):
        self.scenes = []

    def title(self, title, subtitle, duration=4):
        self.scenes.append({"type": "title", "title": title, "subtitle": subtitle, "duration": duration})

    def card(self, title, bullets, duration):
        self.scenes.append({"type": "card", "title": title, "bullets": bullets, "duration": duration})

    def term(self, layout, panes, caption=None, font_size=None):
        s = {"type": "terminal", "layout": layout, "panes": [{"id": i, "title": t} for i, t in panes], "steps": []}
        if caption:
            s["caption"] = caption
        if font_size:
            s["font_size"] = font_size
        self.scenes.append(s)
        return s["steps"]


def cmd(steps, m, pane, shown, real=None, cps=30, pause=1.2, timeout=180, running=False):
    """Run `real` (default: `shown`) on machine m and add a typed command with its real output."""
    out, code = m.sh(real or shown, timeout)
    step = {"pane": pane, "type": "cmd", "text": shown, "output": out, "cps": cps, "pause_after": pause}
    if running:
        step["running"] = True
    steps.append(step)
    print(f"[{pane}] $ {shown}\n{out}\n", flush=True)
    return out, code


def caption(steps, text):
    steps.append({"type": "caption", "text": text})


def wait(steps, s):
    steps.append({"type": "wait", "seconds": s})


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default=str(ROOT / "video/storyboard.json"))
    ap.add_argument("--base", default=None)
    ap.add_argument("--linux", action="store_true", help="also join from the Linux box over ssh")
    ap.add_argument("--codex", action="store_true", help="have a real Codex agent work the queue over MCP")
    a = ap.parse_args()
    base = Path(a.base or f"/private/tmp/sbvideo-{int(time.time())}")
    sean, tony = Machine(base, "sean"), Machine(base, "tony")
    procs = []
    b = Board()
    try:
        b.title("switchboard", "Durable mail, work queues and locks for AI agents on different machines", 4)
        b.card("Agents on two machines need to talk", [
            "Sean's agents and Tony's agents hand each other work: reviews, tests, merges",
            "The old hub (Python agent-mail) mostly worked, but:",
            "anyone past the tunnel could act as anyone; a crashed agent stranded its task forever",
            "the merge lock never expired; peers stayed disconnected after a hub restart",
            "switchboard: one Rust binary, leases, scoped tokens, fenced locks, one-command onboarding",
        ], 13)
        b.scenes.append({
            "type": "diagram", "title": "How it fits together", "duration": 9,
            "caption": "One hub keeps the mail in SQLite. Everyone else joins over tailcat (WireGuard, no accounts).",
            "nodes": [
                {"id": "hub", "label": "Sean's machine\nhub · SQLite · leases · locks", "x": 0.5, "y": 0.26},
                {"id": "tony", "label": "Tony's machine\npeer", "x": 0.2, "y": 0.52, "color": "#bc8cff"},
                {"id": "box", "label": "Linux box\npeer", "x": 0.8, "y": 0.52, "color": "#bc8cff"},
                {"id": "agents", "label": "Claude Code · Codex\nMCP tools + CLI", "x": 0.5, "y": 0.72,
                 "color": "#3fb950"},
            ],
            "edges": [
                {"from": "tony", "to": "hub", "label": "tailcat", "both": True},
                {"from": "box", "to": "hub", "label": "tailcat", "both": True},
                {"from": "agents", "to": "tony", "dashed": True},
                {"from": "agents", "to": "box", "dashed": True},
            ],
        })

        # ---- setup ----------------------------------------------------------------------------------------------
        st = b.term("single", [("hub", "Sean's machine: the hub")],
                    "One command on the hub machine: tailcat keys, config, a background service, and agent registration.")
        cmd(st, sean, "hub", "switchboard setup --name sean", pause=3.5)
        caption(st, "It registered the MCP server and a skill with Claude Code and Codex, so agents can use it at once.")
        wait(st, 2.5)

        # ---- invite + join --------------------------------------------------------------------------------------
        st = b.term("split2", [("hub", "Sean's machine: hub"), ("tony", "Tony's machine")],
                    "Invite someone: a single-use code that expires in 24 hours.")
        out, _ = cmd(st, sean, "hub", "switchboard invite tony", pause=2.5)
        code = re.search(r"sb1\.[A-Za-z0-9_-]+", out).group(0)
        caption(st, "Tony pastes one line. No node keys to copy, no allow lists to edit, no restarts.")
        cmd(st, tony, "tony", f"switchboard join {code}", cps=260, pause=3.5, timeout=240)
        caption(st, "The join used a short-lived enrollment tunnel, got a token scoped to tony/*, and installed the service.")
        wait(st, 2.5)

        st = b.term("split2", [("hub", "Sean's machine: hub"), ("tony", "Tony's machine")],
                    "doctor checks every piece on either side, and says how to fix anything that isn't right.")
        cmd(st, tony, "tony", "switchboard doctor", pause=1.0)
        cmd(st, sean, "hub", "switchboard doctor", pause=3.5)

        # ---- mail: send, watch, take, done ----------------------------------------------------------------------
        st = b.term("split3", [("hub", "Sean's agent  (sean/coord)"), ("watch", "Tony's watcher  (tony/merge)"),
                               ("tony", "Tony's agent  (tony/merge)")],
                    "Mailboxes are person/topic. An agent can watch its queue and get each message the moment it lands.")
        watch = subprocess.Popen([BIN, "--as", "tony/merge", "watch", "--consumer", "video"], env=tony.env,
                                 stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, start_new_session=True)
        procs.append(watch)
        lines = []
        threading.Thread(target=lambda: [lines.append(l.rstrip("\n")) for l in watch.stdout], daemon=True).start()
        time.sleep(2)
        st.append({"pane": "watch", "type": "cmd", "text": "switchboard watch", "output": "", "running": True,
                   "pause_after": 0.6})
        out, _ = cmd(st, sean, "hub",
                     'switchboard --as sean/coord send tony/merge "PR 41 (gunner seats) is gated: please merge it" '
                     '--subject "merge PR 41"', cps=55, pause=0.4)
        deadline = time.time() + 15
        while time.time() < deadline and len(lines) < 2:
            time.sleep(0.2)
        st.append({"pane": "watch", "type": "output", "text": tony.clean("\n".join(lines)), "pause_after": 1.5})
        st.append({"type": "highlight", "pane": "watch", "line_contains": "merge PR 41", "seconds": 1.2})
        caption(st, "take claims it under a 15-minute lease, so no other agent can start the same job.")
        out, _ = cmd(st, tony, "tony", "switchboard --as tony/merge take", pause=1.6)
        mid = re.search(r"#(\d+)", out).group(1)
        caption(st, "done with a note closes it, and the note goes back to the sender as a reply.")
        cmd(st, tony, "tony", f'switchboard --as tony/merge done {mid} --note "merged as 6ce7623, main is green"',
            cps=55, pause=1.2)
        cmd(st, sean, "hub", "switchboard --as sean/coord inbox", pause=1.5)
        cmd(st, sean, "hub", f"switchboard show {mid}", pause=3.0)

        # ---- a real agent over MCP ------------------------------------------------------------------------------
        if a.codex:
            st = b.term("split2", [("hub", "Sean's agent"), ("tony", "Tony's machine: a Codex agent over MCP")],
                        "Coding agents get the queue as native MCP tools: take, done, send, wait_for_mail, lock_take…")
            cmd(st, sean, "hub", 'switchboard --as sean/coord send tony/codex "In Rust, does holding a '
                                 'std::sync::MutexGuard across .await make the future !Send? One sentence." --subject "rust question"',
                cps=70, pause=0.8)
            sbin = str(tony.home / ".switchboard/bin/switchboard")
            prompt = ("You are Tony's agent. Using the switchboard MCP tools as tony/codex: take the oldest message "
                      "from your queue, answer it, and mark it done with your answer as the note. Then say what you "
                      "did in one line.")
            real = (f"codex exec --skip-git-repo-check -c 'mcp_servers.switchboard.command=\"{sbin}\"' "
                    f"-c 'mcp_servers.switchboard.args=[\"mcp\"]' -c 'mcp_servers.switchboard.env="
                    f"{{SWITCHBOARD_HOME=\"{tony.home}/.switchboard\",SWITCHBOARD_AS=\"tony/codex\"}}' "
                    f"{shlex.quote(prompt)}")
            env = {**tony.env, "HOME": os.environ["HOME"]}  # Codex's own login lives in the real home
            p = subprocess.run(real, shell=True, env=env, capture_output=True, text=True, timeout=300, cwd=tony.home)
            raw = (p.stdout + p.stderr).splitlines()
            keep = [l for l in raw if l.startswith("mcp: switchboard/")]
            # The agent's last message: the lines after the last "codex" marker, up to "tokens used".
            idx = [i for i, l in enumerate(raw) if l.strip() == "codex"]
            final = []
            if idx:
                for l in raw[idx[-1] + 1:]:
                    if l.startswith("tokens used"):
                        break
                    if l.strip():
                        final.append("codex: " + l.strip())
            shown_out = "\n".join(keep + ([""] + final if final else []))
            st.append({"pane": "tony", "type": "cmd", "text": 'codex exec "Work your switchboard queue as tony/codex…"',
                       "output": tony.clean(shown_out), "cps": 45, "pause_after": 2.0})
            print("[codex]\n" + shown_out, flush=True)
            caption(st, "The agent's answer came back to Sean as a threaded reply.")
            cmd(st, sean, "hub", "switchboard --as sean/coord inbox", pause=3.0)

        # ---- crash safety ---------------------------------------------------------------------------------------
        st = b.term("split2", [("hub", "Sean's machine"), ("tony", "Tony's machine")],
                    "If an agent crashes mid-task, its lease runs out and the task goes back in the queue.")
        cmd(st, sean, "hub", 'switchboard send tony "rebuild the Windows package and run the smoke test"', cps=60,
            pause=0.6)
        cmd(st, tony, "tony", "switchboard --as tony/a take --lease 5s", pause=1.0)
        caption(st, "tony/a dies here without finishing. Five seconds later its lease runs out…")
        time.sleep(6.5)
        wait(st, 2.0)
        cmd(st, tony, "tony", "switchboard --as tony/b take", pause=1.2)
        st.append({"type": "highlight", "pane": "tony", "line_contains": "leased by tony/b", "seconds": 1.4})
        caption(st, "…so tony/b picks it up. After five lapses a message goes to the dead-letter list, and the sender is told.")
        wait(st, 3.0)

        # ---- fenced locks ---------------------------------------------------------------------------------------
        st = b.term("split2", [("hub", "Sean's merge lane"), ("tony", "Tony's merge lane")],
                    "Locks such as \"one lane pushes main at a time\" expire, and every grant gets a higher fence number.")
        out, _ = cmd(st, sean, "hub", "switchboard lock take feat/gunner-seats --ttl 15s", cps=45, pause=0.8)
        fence = re.search(r"fence (\d+)", out).group(1)
        # Tony waits for it; Sean's lane "stalls" and never renews.
        out, _ = cmd(st, tony, "tony", "switchboard --as tony/merge lock take tony/boats --wait", cps=45, pause=1.2,
                     timeout=120)
        caption(st, "Sean's lane stalled and never renewed, so its lease ran out and Tony's waiting lane got the lock.")
        wait(st, 1.5)
        caption(st, "Before pushing, Sean's lane checks its fence. It's stale, so the push is refused.")
        cmd(st, sean, "hub", f"switchboard lock check {fence} && git push origin HEAD:main", pause=3.0)
        cmd(st, tony, "tony", "switchboard --as tony/merge lock release --note \"pushed 1a2b3c4\"", cps=45,
            pause=2.5)

        # ---- the hub dies ---------------------------------------------------------------------------------------
        st = b.term("split2", [("hub", "Sean's machine: hub"), ("tony", "Tony's machine")],
                    "Kill the hub outright. The service manager restarts it, and the peer repairs its own tunnel.")
        lbl = [l for l in sean.raw(f"ls {sean.home}/Library/LaunchAgents").split() if l.endswith(".plist")][0][:-6]
        pid = sean.raw(f"launchctl print gui/$(id -u)/{lbl} | awk '/pid =/{{print $3}}'")
        assert pid.isdigit(), f"no hub pid for {lbl}: {pid!r}"
        cmd(st, sean, "hub", f"kill -9 {pid}    # the hub daemon", real=f"kill -9 {pid}", pause=0.5)
        t0 = time.time()
        cmd(st, sean, "hub", 'switchboard send tony "are you still there?"', pause=0.8)
        out, _ = cmd(st, tony, "tony", "time (until switchboard inbox 2>/dev/null | grep -q 'still there'; do sleep 1; "
                                       "done); switchboard inbox | tail -2", pause=1.0, timeout=180)
        recovered = time.time() - t0
        time.sleep(6)  # let the peer's next probe log that the hub is back
        cmd(st, tony, "tony", "grep -E 'unreachable|reachable' ~/.switchboard/switchboard.log | tail -3", cps=70,
            real=f"grep -E 'unreachable|reachable' {tony.home}/.switchboard/switchboard.log | tail -3", pause=1.5)
        caption(st, f"Measured: Tony's machine had the new message {recovered:.0f} s after the hub was killed, "
                    "with nobody touching anything.")
        wait(st, 3.5)
        print(f"peer recovered {recovered:.1f}s after kill -9", flush=True)

        # ---- the Linux box ----------------------------------------------------------------------------------------
        if a.linux:
            st = b.term("split2", [("hub", "Sean's machine: hub"), ("linux", "Linux box (Arch, RTX 3090)")],
                        "A second real machine, the Linux box: same invite, same one-line join (a throwaway sandbox there, so no service).")
            out, _ = cmd(st, sean, "hub", "switchboard invite linux", pause=1.5)
            code = re.search(r"sb1\.[A-Za-z0-9_-]+", out).group(0)
            lhome = f"/tmp/sbvideo-linux-{int(time.time())}"
            lenv = (f"export HOME={lhome} PATH=/home/sean/go/bin:/home/sean/switchboard-src/target/release:$PATH; "
                    f"mkdir -p {lhome}; cd {lhome}; ")
            def lsh(c, timeout=180):
                p = subprocess.run(["ssh", "linux", lenv + c], capture_output=True, text=True, timeout=timeout)
                return (p.stdout + p.stderr).replace(lhome, "~").replace(
                    "/home/sean/switchboard-src/target/release/", "").rstrip("\n")
            out = lsh(f"switchboard join {code} --no-service --no-agents")
            st.append({"pane": "linux", "type": "cmd", "text": f"switchboard join {code} --no-service",
                       "output": out, "cps": 260, "pause_after": 1.5})
            lsh("setsid nohup switchboard daemon > daemon.log 2>&1 < /dev/null & echo $! > daemon.pid; "
                "for i in $(seq 1 40); do switchboard whoami >/dev/null 2>&1 && break; sleep 1; done")
            out = lsh("switchboard --as linux/gpu send sean 'GPU parity check passed on the 3090 (seed 42)'")
            st.append({"pane": "linux", "type": "cmd", "text":
                       "switchboard --as linux/gpu send sean 'GPU parity check passed on the 3090 (seed 42)'",
                       "output": out, "cps": 60, "pause_after": 1.0})
            cmd(st, sean, "hub", "switchboard inbox | tail -4", pause=3.0)
            lsh("kill $(cat daemon.pid); sleep 2; for f in .switchboard/tailcat-*.pid; do kill $(cat $f) 2>/dev/null; "
                "done; sleep 2; for f in .switchboard/tailcat-*.pid; do kill -9 $(cat $f) 2>/dev/null; done; true")

        b.card("What you get", [
            "Scoped tokens: tony may act as tony/* only; force-release and invites are admin-only",
            "Leases, requeue and dead letters: a crashed agent never strands work",
            "Expiring locks with fencing: a stale holder can't push",
            "Server-side watch cursors, idempotent sends, broadcasts with per-agent acks",
            "launchd/systemd service, disk-space guard, daily backups, self-healing tunnels, a doctor command",
            "MCP tools and a skill for Claude Code and Codex, registered by setup/join",
        ], 13)
        b.card("Get started", [
            "`cargo install --git https://github.com/threecyborgs/switchboard`",
            "hub: `switchboard setup`, then `switchboard invite tony`",
            "everyone else: `switchboard join sb1.…`",
            "anywhere: `switchboard doctor`",
        ], 9)
        b.title("switchboard", "Three Cyborgs · github.com/threecyborgs/switchboard", 4)
    finally:
        for p in procs:
            try:
                os.killpg(p.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
        for m in (sean, tony):
            m.sh("switchboard service uninstall")
    Path(a.out).write_text(json.dumps({"width": 1920, "height": 1080, "fps": 30, "scenes": b.scenes}, indent=1))
    print(f"storyboard written to {a.out}; sandbox {base}")


if __name__ == "__main__":
    main()
