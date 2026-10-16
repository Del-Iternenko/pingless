# OpenLag

Open-source game traffic optimizer for Windows, a free alternative to paid "game VPNs".
OpenLag captures the traffic of the programs you choose (games, a browser, a terminal,
anything) and sends it through **your** VPN subscriptions or **your own** relay servers.
No TUN adapter, no system-wide VPN: everything else stays direct.

[Русский ниже](#по-русски)

```
 game.exe ──UDP/TCP──► OpenLag (WinDivert) ──┬─► VPN server from your subscription (xray)
   ▲                                         └─► own relays: several routes at once ─► exit ─► game server
   └──────────── answers injected back ◄─────────────────────────────────────────────┘
```

## Features

**Routing**
- Per-process capture with WinDivert: "only these programs" or "everything except these"
- Process groups, each with its own server; auto-connect when a listed program starts
- Kill switch: block instead of leaking when no server answers
- Bypasses the TUN of other VPN clients (Happ, v2rayN...), so traffic never goes through two VPNs
- High CPU priority for the selected programs while optimization is on (optional)

**Servers**
- Subscriptions from Happ, Incy, Marzban, Remnawave; links `vless://` `vmess://` `trojan://`
  `ss://` `wireguard://`, WireGuard `.conf`, Amnezia `vpn://` keys (Xray and WireGuard)
- Import from clipboard, from a file or by `openlag://import?url=...` links
- Real exit country of every server, checked through the server itself (not by its name);
  one click switches a whole country off; filters by word; auto/balancer servers can be turned off
- UDP ping, jitter and loss through every server, the way game packets go; per-server speed test
- Connection tuning: uTLS fingerprint, TLS fragmentation (DPI bypass), mux, DNS-over-HTTPS
- **Own relays**: encrypted tunnel (ChaCha20-Poly1305) to an exit node, optionally through
  bridges; multipath sends every packet over the best N routes and the first copy wins.
  One-click install of a relay on your Linux VPS over SSH

**App**
- Native-feeling window (no browser menus), tray menu, global hotkey (Ctrl+Alt+O)
- Live charts; "without OpenLag → with OpenLag" ping to the very game servers you play on
- Built-in terminal (local shells and SSH) on [xterm.js](https://github.com/xtermjs/xterm.js)
- English and Russian; auto-update from GitHub Releases; settings backup

## Install

Download `OpenLag-Setup-<version>.exe` from [Releases](https://github.com/Del-Iternenko/openlag/releases)
and run it. It installs OpenLag with WinDivert, xray-core and (if missing) WebView2.
OpenLag needs administrator rights to capture traffic. Some antivirus products flag
WinDivert as a "hack tool" because it can intercept traffic.

Start OpenLag **before** the game: UDP sockets that were already open stay direct,
because moving a live match to another IP usually disconnects it.

## Own relays

In the app: add your server on the **Terminal** tab, then **Settings → Own relay servers →
Install relay**. By hand:

```sh
# On a VPS close to the game servers (the "exit"):
OPENLAG_PSK='long random secret' openlag-relay exit --listen 0.0.0.0:7000
# Optional bridges on other providers, each forwarding to the exit:
openlag-relay bridge --listen 0.0.0.0:7001 --exit <exit-ip>:7000
```

Bridges never decrypt traffic. The exit refuses private and loopback destinations unless
started with `--allow-private`. How it works: [docs/design.md](docs/design.md) (in Russian).

## Building

Rust (stable) with the MSVC toolchain.

```sh
cargo build --release
cargo test --workspace
```

Installer, with the app, WinDivert, xray-core, static Linux relays and the WebView2
bootstrapper inside (`vendor\xray\xray.exe` and `vendor\WinDivert-2.2.2-A` must exist;
the Linux relays need `rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl`):

```powershell
.\scripts\build-installer.ps1 -Variant public
```

| crate | what |
|---|---|
| `openlag-proto` | wire format, encryption, duplicate-detection window |
| `openlag-relay` | `exit` and `bridge` relay roles |
| `openlag-core` | client engine: routes, probing, selection, multipath, SOCKS proxies |
| `openlag-capture` | Windows packet capture and injection (WinDivert) |
| `openlag-proxy` | subscriptions, links, xray-core configs, WireGuard |
| `openlag-cli` | `openlag` command-line client |
| `openlag-gui` | desktop app (Tauri) |
| `openlag-setup` | installer and uninstaller |

## License

MIT or Apache-2.0, at your option. Bundled third-party parts keep their own licenses:
WinDivert (LGPL-3.0 / GPL-2.0, loaded at runtime), xray-core (MPL-2.0, separate process),
xterm.js (MIT), flag-icons (MIT).

---

## По-русски

OpenLag — бесплатный open-source оптимизатор игрового трафика для Windows, альтернатива
платным «игровым VPN». Пускает выбранные программы через ваши VPN-подписки (Happ, Incy,
Amnezia, WireGuard...) или через свои relay-серверы, без TUN и без VPN на весь компьютер.

- Перехват по процессам, группы со своим сервером, автоподключение при запуске игры, kill switch
- Реальная страна выхода каждого сервера, отключение стран одним щелчком, фильтры
- Пинг, джиттер и потери через каждый сервер; тест скорости; «без OpenLag → с OpenLag»
- Отпечаток TLS, фрагментация против DPI, mux, DoH
- Свои relay: multipath по нескольким маршрутам, установка на VPS в один клик по SSH
- Встроенный терминал, трей, горячая клавиша, автообновление, русский и английский

Установщик — в [Releases](https://github.com/Del-Iternenko/openlag/releases).
