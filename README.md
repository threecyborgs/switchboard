# switchboard

Durable mail, work queues and locks for AI agents on different machines, carried over
[tailcat](https://github.com/tailscale/tailcat) (WireGuard, NAT traversal, no accounts or central server).

One machine runs the **hub**, which keeps everything in SQLite. Every other machine **joins** with a one-line code and
reaches the hub through a tailcat tunnel. Agents (Claude Code, Codex, scripts) send each other tasks, take them under
leases, reply, broadcast notices and share locks such as "only one of us pushes `main` at a time".

A [Three Cyborgs](https://github.com/threecyborgs) tool. MIT licensed.

## Onboarding: three commands

On the hub machine:

```sh
switchboard setup                 # keys, config, background service, Claude Code + Codex registration
switchboard invite bob           # prints: switchboard join sb1.…
```

On Bob's machine, paste what `invite` printed (or hand it to Bob's coding agent):

```sh
switchboard join sb1.eyJlIjoidGNw…
```

That's all. `join` gets a tailcat client key, spends the single-use invite over a short-lived enrollment tunnel (so
nobody copies node keys around), gets a token scoped to `bob/*`, installs the background service, waits for the hub
to answer, and registers the MCP server and skill with Claude Code and Codex. `switchboard doctor` checks every piece
and prints how to fix whatever isn't right.

Installing: `cargo install --git https://github.com/threecyborgs/switchboard` (tailcat comes from `brew install
tailcat`; `setup` and `join` install it with Homebrew when it's missing).

## Using it

You act as a mailbox `person/topic` (`alice/netcode`, `bob/merge`): `--as`, `$SWITCHBOARD_AS`, or the machine's name.

```sh
switchboard send bob "check the installer on Windows" --subject "installer check"   # any bob/* agent may take it
switchboard send bob/merge "PR 41 is gated"                                   # only bob/merge
switchboard send 'bob/*' "freeze: no pushes for 10 min"                       # a notice to every bob/* mailbox
switchboard inbox                       # open + leased messages and unread notices (--all adds finished ones)
switchboard take                        # claim the oldest open message under a 15-minute lease
switchboard renew 12                    # still working: extend the lease
switchboard done 12 --note "works, branch bob/installer-ok"   # the note goes back to the sender as a reply
switchboard release 12                  # can't do it: back in the queue
switchboard show 12                     # the whole thread
switchboard watch                       # one line per new message; run it under a Monitor
switchboard dead / retry 12             # messages whose lease lapsed 5 times
```

Locks have leases and **fencing tokens**:

```sh
switchboard lock take feat/x --wait     # -> "yours, fence 7"
switchboard lock check 7 && git push origin feat/x:main   # refuses if your lease lapsed and someone else took it
switchboard lock release --note "pushed 1a2b3c4"
```

Agents with MCP get the same as tools: `send`, `inbox`, `take`, `renew`, `done`, `release`, `show`, `sent`,
`wait_for_mail`, `lock_take`, `lock_check`, `lock_renew`, `lock_release`, `lock_status`, `whoami`.

## Design: what can go wrong, and what switchboard does about it

| Failure | What switchboard does |
|---|---|
| Any process that reaches the hub claims to be any agent, or force-releases a lock | Every call carries a hub-issued bearer token scoped to a prefix (`bob` may act as `bob` and `bob/*` only); `--force`, invites and token management are admin-only |
| An agent dies holding a task, and the task is stranded | Leases: a lapsed task goes back to the queue. After 5 lapses it goes to a dead-letter list and the sender is told |
| A lock holder goes silent and blocks everyone, or wakes up late and acts anyway | Locks expire unless renewed. Every grant gets a higher fence number, and `lock check FENCE` refuses a stale holder right before it acts |
| Automated notices get replied to, or picked up as work | Hub notices are marked no-reply: `done --note` on one records the note and sends nothing, and `take` never hands them out |
| A watcher restarts and misses what arrived while it was down | Server-side cursors per consumer, at-least-once: a restarted watcher gets what it missed |
| A send is retried after a timeout and arrives twice | Every send carries an idempotency key; a retry returns the first message |
| "For any of Bob's agents" and "for all of Bob's agents" get confused | `bob` = work for any one `bob/*` agent; `bob/*` = a notice everyone gets and acks separately |
| The hub dies, fills its disk, or corrupts its store | launchd / systemd / logon-task service, WAL SQLite behind one writer, refuses new mail below a free-disk floor instead of failing mid-write, daily `VACUUM INTO` backups (7 kept), `/healthz`, `doctor` |
| A peer's `tailcat forward` stays dead after the hub restarts (tailcat does not recover by itself) | The peer daemon probes the hub through the tunnel every 5 s and restarts the forward after two misses (measured: a peer had new mail 20–23 s after the hub was `kill -9`ed) |
| Adding a machine means copying node keys, editing allow lists and restarting | `invite` / `join`: single-use, expiring codes; the hub learns the peer's key from the tunnel itself and restarts its listener with the new allow list |

## How it fits together

```
 Alice's machine (hub)                                  Bob's machine (peer)
 ┌──────────────────────────────┐                      ┌──────────────────────────────┐
 │ switchboard daemon           │   tailcat (WireGuard)│ switchboard daemon           │
 │  ├ HTTP API 127.0.0.1:5490   │◄────────────────────►│  └ tailcat forward 5490      │
 │  ├ SQLite (WAL) + backups    │   --allow=<enrolled> │       (+ health probe)       │
 │  ├ reaper: leases, locks, TTL│                      │                              │
 │  ├ tailcat serve (main)      │   enrollment tunnel  │ switchboard join sb1.…       │
 │  └ tailcat serve (enroll,    │◄ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ │  (once, with the invite)     │
 │     only while invites open) │                      │                              │
 └──────────────▲───────────────┘                      └──────────────▲───────────────┘
   Claude Code / Codex (MCP: `switchboard mcp`), CLI       Claude Code / Codex, CLI
```

- **Hub**: `switchboard daemon` with `role = "hub"`. The API listens on loopback only; tailcat carries it to enrolled
  peers (`--allow` = their node keys). The tailcat keys are saved with `--fixed-region`, so the hub's address never
  changes.
- **Enrollment**: `invite` stores a hashed one-time secret and starts a second tailcat listener, which runs
  `switchboard enroll-stdio` per connection. tailcat hands it the caller's node key (`$TAILCAT_PEER_KEY`). The secret
  buys a token and an allow-list entry, and the main listener restarts with the new list. The enrollment listener
  stops when no invite is open.
- **Peer**: `switchboard daemon` with `role = "peer"` runs and babysits `tailcat forward`.
- Files: `~/.switchboard/` (or `$SWITCHBOARD_HOME`): `config.toml` (0600, holds the token), `switchboard.sqlite`,
  `backups/`, `switchboard.log`, `hub.addr`.

## Commands for the hub owner

```sh
switchboard peers [--remove nodekey:…]          # enrolled machines; removal cuts tunnel access at once
switchboard token list | create PREFIX [--admin] | revoke ID
switchboard import-agent-mail OLD.sqlite      # bring in a legacy agent-mail database (keeps ids; safe to rerun)
switchboard service install | uninstall | status
switchboard agents install | uninstall
```

`config.toml` `[hub]` settings: `port` (5490), `min_free_mb` (512), `backups_keep` (7), `lease_secs` (900),
`max_attempts` (5), and `[hub.lock_notify]` with `main = ["alice", "bob/merge"]` for lock notice lists.

## Testing

```sh
cargo test                           # 14 end-to-end API tests: auth scopes, 8 concurrent takers x 200 messages,
                                     # lease lapse -> requeue -> dead letter, idempotency, TTL, broadcasts, cursor
                                     # survival across a restart, long-poll wakeup, lock expiry + fencing, disk
                                     # guard, single-use invites, agent-mail import, backups, MCP JSON-RPC, CLI
python3 scripts/e2e_tailcat.py       # a real hub and peer over tailcat, each in its own HOME: setup, invite,
                                     # join, mail both ways, lock fencing, hub restart and reconnect, doctor
```

## Wrapping it in your own tooling

The CLI is stable enough to wrap: `exec switchboard "$@"` behind your own script name works, `--json` is available on
the read commands, and the exit codes mean something (`lock take` exits 2 when the lock is held, `lock check` exits 3
for a stale fence). A typical "one lane pushes main" flow:

```sh
switchboard lock take "$BRANCH" --wait          # prints the fence
switchboard lock check "$FENCE" && git push origin "$BRANCH:main"
switchboard lock release --note "pushed $(git rev-parse --short HEAD)"
```

Pick an always-on machine for the hub; peers only need to be up when they work.

## Not done yet

- Windows: the service is a logon scheduled task, and it has not been tested on Windows yet.
- No binary releases or Homebrew tap yet: install with `cargo install --git`.
- The tunnel is tailcat only. iroh would be the natural second transport.

## The demo video

`video/capture.py` runs the whole demo for real and writes `video/storyboard.json`. That covers two sandboxed
machines with launchd services, a Codex agent working the queue over MCP, lock fencing, a `kill -9` of the hub, and
an optional join from a second real machine over ssh (`--remote HOST`). `video/render.py` turns the storyboard into a 1080p mp4 (`video/out/`, not
committed). Every terminal line in the video is captured output. Two things are tidied for display: the sandbox
folder is shown as `~`, and each sandbox's service-label suffix is dropped.
