# OpenLag

Open-source game traffic optimizer for Windows. OpenLag takes the UDP traffic of the
games you choose and sends it through your own relay servers, over several routes at
once. Whichever copy of a packet arrives first wins, so packet loss and latency spikes
on one route stop reaching the game.

Nothing is hosted for you: you run the relays (any cheap VPS works), and the client
talks only to them.

```
 game.exe ──UDP──► OpenLag client ──┬── route A (bridge 1) ──┐
   ▲                (WinDivert)     ├── route B (bridge 2) ──┼──► exit relay ──► game server
   └──────── answers injected ◄─────┴── route C (2nd NIC) ───┘
```

## Status

Early, working core:

- per-process UDP capture with WinDivert (`cs2.exe`, `dota2.exe`, ...)
- encrypted tunnel (ChaCha20-Poly1305, pre-shared key) to an **exit** relay, optionally
  through **bridge** relays
- multipath: every packet goes over the N best routes, duplicates are dropped on both ends
- continuous latency / jitter / loss probing, route selection with hysteresis, failover
  when a route dies
- falls back to a direct connection when no relay answers, instead of cutting the game off
- terminal dashboard with per-route ping history

Not yet: TCP tunnelling (game TCP goes direct), GUI, Linux/macOS capture, routing
rules by server IP range.

## Quick start

### 1. Relays

```sh
# On a VPS close to the game servers (the "exit"):
OPENLAG_PSK='long random secret' openlag-relay exit --listen 0.0.0.0:7000 --listen 0.0.0.0:47000

# Optional: bridges on other providers, each forwarding to the exit:
openlag-relay bridge --listen 0.0.0.0:7001 --exit <exit-ip>:7000
```

Bridges never decrypt traffic and only forward to their one exit. The exit refuses to
send traffic to private or loopback addresses unless started with `--allow-private`.

### 2. Client

1. Download WinDivert 2.2 from <https://github.com/basil00/WinDivert/releases> and put
   `WinDivert.dll` and `WinDivert64.sys` (from the `x64` folder) next to `openlag.exe`.
   Some antivirus products flag WinDivert as a "hack tool" because it can intercept
   traffic; you may need to allow it.
2. Copy `config.example.toml` to `config.toml` and fill in your relays and games.
3. From an elevated terminal:

```sh
openlag check            # can this machine capture traffic?
openlag probe            # measure routes only, no admin needed
openlag run              # tunnel the configured games
```

Start OpenLag **before** the game. UDP sockets that were already open stay direct,
because moving a live match to another IP usually disconnects it
(`tunnel_existing = true` changes that).

## Building

```sh
cargo build --release
cargo test --workspace
```

Crates:

| crate | what |
|---|---|
| `openlag-proto` | wire format, encryption, duplicate-detection window |
| `openlag-relay` | `exit` and `bridge` relay roles |
| `openlag-core` | client engine: routes, probing, selection, multipath |
| `openlag-capture` | Windows packet capture and injection (WinDivert) |
| `openlag-cli` | `openlag` command-line client |

See [docs/design.md](docs/design.md) for how it works and which edge cases it handles.

## License

MIT or Apache-2.0, at your option. WinDivert is a separate project under LGPL-3.0 /
GPL-2.0 and is loaded at runtime, not linked.
