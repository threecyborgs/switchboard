# switchboard

Durable mail, work queues and locks for AI agents on different machines, carried over
[tailcat](https://github.com/tailscale/tailcat) (WireGuard, NAT traversal, no accounts or central server).

One machine runs the **hub**, which keeps everything in SQLite. Every other machine **joins** with a one-line code and
reaches the hub through a tailcat tunnel. Agents (Claude Code, Codex, scripts) send each other tasks, take them under
leases, reply, broadcast notices and share locks such as "only one of us pushes `main` at a time".

A Three Cyborgs tool. It replaces the Python `agent-mail` hub that CubeVoid's agents used.

## Onboarding: three commands

On the hub machine:

```sh
switchboard setup                 # keys, config, background service, Claude Code + Codex registration
switchboard invite tony           # prints: switchboard join sb1.…
```

On Tony's machine, paste what `invite` printed (or hand it to Tony's coding agent):

```sh
switchboard join sb1.eyJlIjoidGNw…
```

That's all. `join` gets a tailcat client key, spends the single-use invite over a short-lived enrollment tunnel (so
nobody copies node keys around), gets a token scoped to `tony/*`, installs the background service, waits for the hub
to answer, and registers the MCP server and skill with Claude Code and Codex. `switchboard doctor` checks every piece
and prints how to fix whatever isn't right.

Installing: `cargo install --git https://github.com/threecyborgs/switchboard` (tailcat comes from `brew install
tailcat`; `setup` and `join` install it with Homebrew when it's missing).

## Using it

You act as a mailbox `person/topic` (`sean/netcode`, `tony/merge`): `--as`, `$SWITCHBOARD_AS`, or the machine's name.

```sh
switchboard send tony "check boat reload on Windows" --subject "boat reload"   # any tony/* agent may take it
switchboard send tony/merge "PR 41 is gated"                                   # only tony/merge
switchboard send 'tony/*' "freeze: no pushes for 10 min"                       # a notice to every tony/* mailbox
switchboard inbox                       # open + leased messages and unread notices (--all adds finished ones)
switchboard take                        # claim the oldest open message under a 15-minute lease
switchboard renew 12                    # still working: extend the lease
switchboard done 12 --note "works, branch tony/boat-ok"   # the note goes back to the sender as a reply
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

## What changed from agent-mail, and why

| agent-mail (Python) | switchboard |
|---|---|
| The sender is whatever `x-mail-as` says: any process past the tunnel could act as anyone or force-release the lock | Every call carries a hub-issued bearer token scoped to a prefix (`tony` may act as `tony` and `tony/*` only); `--force` and invites are admin-only |
| `take` is forever: an agent that dies holding a message strands it | Leases: lapsed messages go back to the queue; after 5 lapses they go to a dead-letter list and the sender is told |
| The merge lock never expires; a silent holder blocks everyone until someone force-releases | Locks expire unless renewed; every grant gets a higher fence, and `lock check FENCE` refuses a stale holder right before it pushes |
| Lock notices came from a `merge-lock` "mailbox"; replies to them piled up unread (159 of them) | Hub notices are marked no-reply: `done --note` on one records the note and sends nothing; `take` never hands them out as work |
| `watch` remembered its place client-side and skipped everything sent while it was down | Server-side cursors per consumer, at-least-once: a restarted watcher gets what it missed |
| A retried send after a timeout could arrive twice | Every send carries an idempotency key; a retry returns the first message |
| A message to `tony` was a work item and a broadcast at once | `tony` = work for any one `tony/*` agent; `tony/*` = a notice everyone gets and acks separately |
| `nohup` shell loop on a laptop; died with the disk; a SIGBUS once; no backups | launchd / systemd / logon-task service, WAL SQLite behind one writer, refuses new mail below a free-disk floor instead of corrupting, daily `VACUUM INTO` backups (7 kept), `/healthz`, `doctor` |
| Peers' `tailcat forward` stayed dead after the hub restarted | The peer daemon probes the hub through the tunnel every 5 s and restarts the forward after two misses (measured: a `kill -9`ed hub is reachable from a peer again within 30 s) |
| Adding a peer: send your node key to the hub owner, who edits an allow file and restarts | `invite` / `join`: single-use, expiring codes; the hub learns the peer's key from the tunnel itself and restarts its listener with the new allow list |

## How it fits together

```
 Sean's machine (hub)                                  Tony's machine (peer)
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
switchboard import-agent-mail ~/.config/cubevoid/agent-mail.sqlite   # keeps ids; safe to rerun
switchboard service install | uninstall | status
switchboard agents install | uninstall
```

`config.toml` `[hub]` settings: `port` (5490), `min_free_mb` (512), `backups_keep` (7), `lease_secs` (900),
`max_attempts` (5), and `[hub.lock_notify]` with `main = ["sean", "tony/merge"]` for lock notice lists.

## Testing

```sh
cargo test                           # 14 end-to-end API tests: auth scopes, 8 concurrent takers x 200 messages,
                                     # lease lapse -> requeue -> dead letter, idempotency, TTL, broadcasts, cursor
                                     # survival across a restart, long-poll wakeup, lock expiry + fencing, disk
                                     # guard, single-use invites, agent-mail import, backups, MCP JSON-RPC, CLI
python3 scripts/e2e_tailcat.py       # a real hub and peer over tailcat, each in its own HOME: setup, invite,
                                     # join, mail both ways, lock fencing, hub restart and reconnect, doctor
```

## Migrating CubeVoid from agent-mail

1. On the machine that will host the hub (the Linux box is the best fit: it is always on): `switchboard setup --name
   sean`, then `switchboard import-agent-mail <old agent-mail.sqlite>`, and set `[hub.lock_notify] main = ["sean",
   "tony/merge"]`.
2. `switchboard invite tony` and send Tony the line. Each other machine runs `switchboard join …`.
3. Point `tools/agent-mail` at it: `exec switchboard "$@"`. The verbs are the same (`send`, `inbox`, `take`, `done`,
   `show`, `sent`, `watch`, `lock take BRANCH --wait`, `lock release --note`), and `$AGENT_MAIL_AS` is still read.
   Add `lock check FENCE` before the push.
4. Stop the old hub.

## Not done yet

- Windows: the service is a logon scheduled task, and it has not been tested on Windows yet.
- No binary releases or Homebrew tap yet: install with `cargo install --git`.
- The tunnel is tailcat only. iroh would be the natural second transport.
