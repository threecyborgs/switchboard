#!/usr/bin/env python3
"""End-to-end check over real tailcat: a hub and a peer, each in its own HOME (so its own tailcat keys), on this
machine. Exercises setup -> invite -> join -> mail both ways -> lock fencing -> hub restart -> doctor.

  python3 scripts/e2e_tailcat.py [--bin target/release/switchboard] [--keep]

Prints one line per step and PASS/FAIL. Starts its own daemons in their own process groups and stops only those.
"""
import argparse
import json
import os
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


class Machine:
    def __init__(self, name, home, binary):
        self.name, self.home, self.bin = name, Path(home), binary
        self.home.mkdir(parents=True, exist_ok=True)
        self.daemon = None
        self.env = {**os.environ, "HOME": str(self.home), "SWITCHBOARD_HOME": str(self.home / ".switchboard")}
        for k in ("SWITCHBOARD_AS", "AGENT_MAIL_AS", "SWITCHBOARD_URL", "SWITCHBOARD_TOKEN"):
            self.env.pop(k, None)

    def run(self, *args, timeout=120, check=True):
        p = subprocess.run([self.bin, *args], env=self.env, capture_output=True, text=True, timeout=timeout)
        out = p.stdout + p.stderr
        if check and p.returncode != 0:
            raise SystemExit(f"FAIL [{self.name}] switchboard {' '.join(args)} -> {p.returncode}\n{out}")
        return p.returncode, out

    def start(self):
        log = open(self.home / "daemon.log", "a")
        self.daemon = subprocess.Popen([self.bin, "daemon"], env=self.env, stdout=log, stderr=log,
                                       start_new_session=True)

    def stop(self):
        if self.daemon and self.daemon.poll() is None:
            os.killpg(self.daemon.pid, signal.SIGTERM)
            try:
                self.daemon.wait(10)
            except subprocess.TimeoutExpired:
                os.killpg(self.daemon.pid, signal.SIGKILL)
        self.daemon = None

    def wait_whoami(self, secs=90):
        deadline = time.time() + secs
        while time.time() < deadline:
            code, out = self.run("whoami", check=False, timeout=30)
            if code == 0:
                return out.strip()
            time.sleep(1)
        raise SystemExit(f"FAIL [{self.name}] hub never answered: {out}\n--- daemon log ---\n"
                         + (self.home / "daemon.log").read_text()[-3000:])


def step(msg):
    print(f"  ok  {msg}", flush=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default=str(ROOT / "target/release/switchboard"))
    ap.add_argument("--keep", action="store_true", help="leave the daemons running (for poking around)")
    ap.add_argument("--port", type=int, default=15490)
    a = ap.parse_args()
    base = Path(tempfile.mkdtemp(prefix="switchboard-e2e-"))
    hub = Machine("sean-hub", base / "sean", a.bin)
    peer = Machine("tony-peer", base / "tony", a.bin)
    t0 = time.time()
    print(f"switchboard e2e over tailcat, sandbox {base}")
    try:
        hub.run("setup", "--name", "sean", "--port", str(a.port), "--no-service", "--no-agents")
        hub.start()
        step(f"hub set up and running: {hub.wait_whoami()}")
        _, out = hub.run("invite", "tony", "--ttl", "10m", timeout=60)
        code = next(w for w in out.split() if w.startswith("sb1."))
        step(f"invite made ({len(code)} chars)")
        t_join = time.time()
        _, out = peer.run("join", code, "--local-port", str(a.port + 1), "--no-service", "--no-agents", timeout=180)
        step(f"peer enrolled through the one-time listener in {time.time() - t_join:.1f}s")
        peer.start()
        step(f"peer reaches the hub over tailcat: {peer.wait_whoami()}")
        code2, out = peer.run("join", code, "--local-port", str(a.port + 2), "--no-service", "--no-agents",
                              check=False, timeout=120)
        assert code2 != 0, "a used invite must not work twice"
        step("the invite can't be used twice")

        _, out = hub.run("inbox")
        assert "tony joined" in out, out
        step("the hub owner got a 'tony joined' notice")
        peer.run("--as", "tony/worldgen", "send", "sean", "biome seams on face 3", "--subject", "seams")
        _, out = hub.run("--as", "sean/coord", "take")
        mid = int(out.split()[0].lstrip("#"))
        assert "biome seams" in out, out
        hub.run("--as", "sean/coord", "done", str(mid), "--note", "fixed on sean/seams")
        _, out = peer.run("--as", "tony/worldgen", "inbox")
        assert "fixed on sean/seams" in out, out
        step("peer -> hub task, taken under a lease, done with a reply that came back over the tunnel")

        _, out = hub.run("--as", "sean", "lock", "take", "feat/a", "--ttl", "3s")
        fence = int(out.split("fence ")[1].split(".")[0])
        code3, _ = peer.run("--as", "tony/merge", "lock", "take", "tony/b", check=False)
        assert code3 == 2
        time.sleep(4)
        _, out = peer.run("--as", "tony/merge", "lock", "take", "tony/b")
        assert hub.run("lock", "check", str(fence), check=False)[0] == 3
        step("lock lease lapsed, peer took it with a newer fence, the stale fence is refused")
        _, out = peer.run("--as", "tony/merge", "inbox")
        assert "EXPIRED" in out, out
        peer.run("--as", "tony/merge", "lock", "release", "--note", "pushed")

        # hub restart: the peer's forward and its saved cursor ride through it
        hub.stop()
        code4, _ = peer.run("whoami", check=False, timeout=60)
        hub.start()
        hub.wait_whoami()
        peer.wait_whoami()
        hub.run("send", "tony", "after the restart")
        _, out = peer.run("inbox")
        assert "after the restart" in out, out
        step(f"hub restarted (peer saw it down: exit {code4}); the peer reconnected with no action")

        _, out = peer.run("doctor", check=False)
        assert "hub reachable" in out and "token valid" in out, out
        _, out2 = hub.run("doctor", check=False)
        assert "1 enrolled peer(s): tony" in out2, out2
        step("doctor is green on both sides (apart from the service, which this test runs by hand)")
        print(f"PASS in {time.time() - t0:.0f}s")
    finally:
        if not a.keep:
            peer.stop()
            hub.stop()


if __name__ == "__main__":
    main()
