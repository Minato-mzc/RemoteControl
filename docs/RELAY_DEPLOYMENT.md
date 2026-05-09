# Cross-network relay deployment

Run `remotecontrol-relay` on a public-IP VPS so your phone can reach the
PC even when they're on different networks (mobile data, hotel Wi-Fi, etc).

The relay is a thin broker — it just forwards bytes between PC and phone
WebSockets. End-to-end auth still happens between PC and phone (HMAC
over the QR pairing key); the relay sees only opaque traffic.

## What you need

- A VPS with a public IP. Bandwidth determines streaming quality —
  any `1 vCPU / 512 MB / 5 Mbps` plan handles a single user reasonably
  well at 720p / 5 Mbps. For 1080p / 30 Mbps cross-network, you need
  the VPS uplink to support it.
- A domain name pointed at the VPS (e.g. `relay.yourdomain.com`).
  Phones on mobile carriers often reject self-signed TLS, so you want
  Let's Encrypt.
- `caddy` (auto Let's Encrypt) or `nginx + certbot` on the VPS.

## Step 1 — Build & ship the relay binary

On a Linux build host (or use cross-compile from Windows / WSL):

```bash
cd /path/to/RemoteControl
cargo build --release -p remotecontrol-relay
# produces target/release/remotecontrol-relay
scp target/release/remotecontrol-relay user@vps:/usr/local/bin/
```

The binary is a single static-ish ELF (~7 MB) with no runtime
dependencies beyond glibc.

## Step 2 — systemd unit

```ini
# /etc/systemd/system/remotecontrol-relay.service
[Unit]
Description=RemoteControl cross-network relay
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/remotecontrol-relay --port 7891 --host 127.0.0.1
Restart=on-failure
User=relay
Group=relay
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true

[Install]
WantedBy=multi-user.target
```

```bash
sudo useradd --system --no-create-home relay
sudo systemctl daemon-reload
sudo systemctl enable --now remotecontrol-relay
```

The relay listens on `127.0.0.1:7891` only — caddy fronts it. Adjust
`--host 0.0.0.0` if you'd rather expose it directly (then handle TLS
yourself).

## Step 3 — TLS termination with caddy

```caddyfile
# /etc/caddy/Caddyfile
relay.yourdomain.com {
    reverse_proxy 127.0.0.1:7891
    # The relay's WebSocket upgrade works through caddy's default
    # proxy config — no special directives needed.
}
```

```bash
sudo systemctl reload caddy
```

Caddy fetches a Let's Encrypt cert automatically on first request to
`https://relay.yourdomain.com`. Verify with `curl https://relay.yourdomain.com/healthz`
— should return `ok`.

## Step 4 — Provision your PC

On the Windows host, one-shot:

```powershell
remotecontrol-server.exe --relay-register https://relay.yourdomain.com
```

Output:
```
Relay provisioning complete.
  base_url   = https://relay.yourdomain.com
  host_id    = <UUID>
  host_token = (saved to %LOCALAPPDATA%\RemoteControl\relay.toml; do not share)
```

The `host_token` is your secret with the relay — anyone who has it can
impersonate your PC to the relay. Keep it on the PC. The QR payload
embeds only `host_id`, never `host_token`.

## Step 5 — Run the PC server in relay mode

```powershell
remotecontrol-server.exe --relay         # LAN listener + relay client
# or:
remotecontrol-server.exe --relay-only    # relay only, skip LAN listener
```

The console will log the relay QR payload — currently it goes to the
log, not the HTML page. You can rebuild the QR card client-side from:

```
rcrelay://relay.yourdomain.com/?host=<HOST_ID>&v=6&c=<CODE>&k=<KEY>
```

Use any QR generator (e.g. `qrencode 'rcrelay://...'`) to display it.
Native HTML rendering of the relay QR is on the polish list.

## Step 6 — Scan from the phone

The phone app accepts both `rc://` (LAN) and `rcrelay://` (relay) QR
schemes. Scan whichever matches the network the phone is currently on:

| Phone network | Scan |
|---------------|------|
| Same Wi-Fi as PC | `rc://192.168.x.x/...` (LAN QR) |
| Different Wi-Fi or mobile data | `rcrelay://relay.yourdomain.com/...` (relay QR) |

Trusted reconnect works the same in both modes — after the first
successful pairing, the phone stores the path it used and can resume
without scanning.

## Troubleshooting

- **`relay disconnected` on the PC right after dial.** Check
  `/var/log/syslog` on the VPS for a 401 — the saved `host_token`
  doesn't match what the relay has in memory. Solution: re-run
  `--relay-register` (this mints a new `host_id`/token pair, the
  old one becomes orphan and harmless).

- **Phone connects but sees `host offline`.** The PC isn't running
  in `--relay` mode, or the relay crashed and the PC needs a few
  seconds to reconnect.

- **Stream stutters cross-network but not on LAN.** Your VPS uplink
  (or the user's downlink) is the bottleneck. `iftop` on the VPS
  during a session shows the bandwidth — if it pegs at the plan
  limit, lower the bitrate target on the phone side.

## Trust model summary

```
QR (public)           rcrelay://relay.example/?host=PUBLIC&c=...&k=...
                                                    │
                                                    ▼
                            ┌─────────────────────────────┐
                            │  RELAY                      │
                            │  knows: host_id, token_hash │
                            │  doesn't decrypt traffic    │
                            └─────────────────────────────┘
                                 ▲                     ▲
host_token (secret)    ┌─────────┘                     └─────────┐
                       │                                          │
                  ┌─────────┐                              ┌─────────┐
                  │   PC    │ ◄────── M1 HMAC ────────►   │  PHONE  │
                  └─────────┘     (end-to-end auth)        └─────────┘
```
