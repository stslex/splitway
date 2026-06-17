# Phase 3c — OpenVPN standalone (no NetworkManager)

Implement sub-phase 3c from `ROADMAP.md` (Phase 3 — Backend breadth). Read `CLAUDE.md` first and use the `platform-dns` skill (systemd-resolved section and the OpenVPN-via-NM subtlety for contrast). Scope: make a **standalone** OpenVPN connection — one started directly by `openvpn`/`openvpn-client@.service`, **not** imported into NetworkManager — auto-apply and auto-revert split-DNS through the running daemon.

This is the "more work" sub-phase the roadmap flagged: unlike 3a, there is no NetworkManager normalizing the pushed DNS onto the link. A new `VpnDetector` is needed; the `DnsBackend` (resolvectl on Linux, `/etc/resolver` on macOS) is unchanged and VPN-agnostic. Parallel-safe with nothing currently open; touches only the Linux detector tree, the detector-selection seam, config, and docs.

## Branch

Branch `phase-3c-openvpn-standalone` from up-to-date `dev`.

## The core problem (read before designing)

With NM (3a), NM applies pushed DNS to the `tun*` link, so `LinuxDetector::detect` reads it back from `nmcli device show` (`IP4.DNS[n]`). **Standalone OpenVPN does not do this** unless an `--up` script or a plugin (`update-systemd-resolved`, or 2.6+ `--dns`) handles the pushed options — and we cannot assume the user configured one. So the pushed DNS is not on any link for us to read back; **Splitway must learn it from OpenVPN itself**. The roadmap names the two sources: the OpenVPN **management interface (preferred)** and **pushed-DNS parsing**. In practice these combine: the management interface is the event/up-down source *and* the channel through which pushed DNS is observed (`log on` surfaces the `PUSH_REPLY` line carrying `dhcp-option DNS ...`).

The DNS *application* is unchanged: once the detector emits `VpnEvent::Up(VpnInfo { interface_name, dns_servers })`, the existing resolvectl backend sets per-link DNS + routing domains on the `tun*` device exactly as it does for NM/GlobalProtect. Confirm this end-to-end; do not touch the backend.

## Investigate first (write findings + transcripts into the PR description)

Stand up a real standalone OpenVPN (the user will provide one) and capture, from the management interface, enough to decide the design. Parsing must be reconstructable from these captures so tests need no live socket (`CLAUDE.md`: "parsing must be testable without live system commands").

1. **Event source.** With `state on`, capture the real-time `>STATE:<time>,CONNECTED,SUCCESS,<vpn_ip>,...` line and the `RECONNECTING` / `EXITING` lines on disconnect/restart. Confirm these are sufficient to drive up/down.
2. **Pushed DNS.** With `log on`, capture the `PUSH: Received control message: 'PUSH_REPLY,...,dhcp-option DNS <ip>,dhcp-option DOMAIN <d>,...'` line. Confirm IPv4 **and** IPv6 (`dhcp-option DNS` may carry a v6 address) appear here for your server. Save it as a parser test fixture.
3. **Attach-after-connect recovery.** Determine whether `log on all` (or equivalent) **replays** the historical `PUSH_REPLY` so a watcher that connects to the management socket *after* OpenVPN is already up can still recover the pushed DNS. This decides whether startup detection works when the daemon starts after the VPN. Capture the transcript. If replay is not reliable, document the fallback (e.g. re-read on the next event, or `detect()` keying off another source) and isolate it.
4. **Device name.** Decide how `VpnInfo.interface_name` is populated for the resolvectl backend: from config `vpn_name`, or discovered from the log (`TUN/TAP device tunX opened`) and cross-checked against config. The backend must target the actual `tun*` link.
5. **Socket transport & auth.** Confirm whether to support TCP (`management 127.0.0.1 7505`) and/or a unix socket (`management /run/openvpn/mgmt.sock unix`), and management password (`management-client-auth` / password file). Note perms/security (below).
6. **Close the 3a deferred checks** while a real OpenVPN is up: the OpenVPN-over-NM "use this connection only for resources on its network" routing-domain toggle, and the no-pushed-DNS edge case (see 3a prompt's deferred follow-up). Record results in this PR.

## Detector design

Add a standalone-OpenVPN detector under `splitway-daemon/src/detector/linux/openvpn/` (mirror the existing thin-plumbing / pure-logic split — see `detector/linux/dbus.rs` vs `state.rs`, and the macOS `watch.rs` vs `state.rs`):

- **`parser.rs` (pure, unit-tested).** Parse a management `>STATE` line to a state token; parse a `PUSH_REPLY` line to `Vec<String>` of DNS servers (`dhcp-option DNS <ip>`, v4 + v6; ignore non-DNS options). No I/O. Cover with fixtures from the investigation.
- **`state.rs` (pure, unit-tested).** Map OpenVPN state tokens to a transition: `CONNECTED` → `Up`; `EXITING` → `Down`; `RECONNECTING` → `Down`; intermediate/unknown (`CONNECTING`, `WAIT`, `AUTH`, `GET_CONFIG`, `ASSIGN_IP`, `ADD_ROUTES`, `RESOLVE`, ...) → ignored. Reuse the `Deduper`/`Transition` pattern from `detector/linux/state.rs` (lift it to a shared spot if cleaner than duplicating; do not regress the NM tests).
- **`mgmt.rs` (thin, not unit-tested).** Connect to the management socket (`tokio::net::TcpStream` / `UnixStream`), authenticate if a password is configured, send `state on` + `log on`, and read lines with `BufReader::lines()`. Feed parsed transitions + DNS into the same `tokio::sync::mpsc::Sender<VpnEvent>` contract the other detectors use; honor `tx.closed()` to stop (mirror `watch_loop`'s `tokio::select!`). Keep client commands **read-only** (`state`, `log`); never send `signal`/`hold`.
- **`OpenVpnDetector` implementing `VpnDetector`.** `watch()` spawns the `mgmt` task on the ambient tokio runtime (mirror `LinuxDetector::watch`). `detect()` does a one-shot connect → `state` (+ replayed `log` if Q3 supports it) → `VpnInfo`.

`VpnInfo` already carries exactly what we need (`interface_name`, `dns_servers: Vec<String>`) — no trait/struct changes expected. Confirm.

## Detector selection (design decision — justify in the PR)

`create_vpn_detector()` in `detector/mod.rs` currently picks **per-OS** (`cfg!`) and takes no arguments; `daemon/mod.rs` calls it then `detector.watch(&config.vpn_name)`. OpenVPN-standalone is not a different OS — it's a second Linux detector, so a selector is needed.

- **Recommended:** add a config field (e.g. `vpn_backend`: `"network-manager"` | `"openvpn"`, **defaulting to `network-manager`** so every existing config keeps its behavior). Change `create_vpn_detector(&config)` to dispatch on it for Linux; pass the config through from `daemon/mod.rs`. This is explicit, testable, and leaves macOS/Windows untouched (they ignore the field for now).
- **Rejected unless investigation forces it:** auto-detection (probe NM, fall back to mgmt socket). Hidden behavior, fragile ordering — avoid as the primary path.

## Config / docs

- Extend `LocalConfig` (`splitway-shared/src/config/mod.rs`) with the selection field plus the management connection: address (TCP `host:port` or unix path) and optional password-file path. **Every new field must use `#[serde(default)]`** and be covered by a back-compat test (mirror `enabled_defaults_to_true_when_absent` — a pre-3c config must still parse and behave as NM).
- `README.md`: add standalone OpenVPN to "Current state" and a Config subsection — the required `openvpn.conf` `management ...` line, that Splitway only *reads* state/log, the device-name guidance, and a **security note**: bind the management interface to localhost/unix-socket with tight perms; never expose it over TCP to other hosts (it is a control channel). Note no new daemon deployment artifact is needed — OpenVPN runs as its own service.

## Failure modes (must be handled + tested where pure)

- **Socket loss ≠ VPN down.** A dropped/erroring management socket while the VPN is still up must **not** emit `Down` (that would revert rules). Reconnect with backoff and re-sample; only OpenVPN state (`EXITING`/`RECONNECTING`) means down. Mirror the macOS rule "transient read error → keep last state."
- **No pushed DNS.** If `PUSH_REPLY` carries no `dhcp-option DNS`, define and test the behavior: emit `Up` with empty `dns_servers` and have the backend no-op + log (don't call `resolvectl dns <iface>` with zero servers), rather than applying a broken rule. This is the 3a-deferred no-pushed-DNS edge.
- **Management not enabled / wrong password.** Clear error; daemon stays up with IPC available and auto-apply off (mirror `daemon/mod.rs`'s existing "watch failed" handling).
- **Attach-after-connect race.** Sample current state only after `state on`/`log on` are armed so a transition mid-setup isn't lost; dedup the initial sample against the first streamed event (mirror the macOS "sample after arming" + shared `Deduper`).

## Out of scope

- Any `DnsBackend` change — resolvectl already applies per-link DNS regardless of VPN type; `/etc/resolver` likewise.
- OpenVPN imported into NetworkManager — that is 3a (done).
- macOS standalone OpenVPN — the management-socket detector is OS-independent in principle, but keep 3c Linux-only; design the config field/selector so macOS can adopt it later without rework, and note that as a follow-up.
- Windows, GUI (Phase 4), proxy/route targets, routing-domain semantics tuning.

## Done criteria

- fmt, clippy, tests green on CI (ubuntu + macos compile).
- On a real standalone OpenVPN (no NM) on Linux: connecting auto-applies rules and the configured domains resolve through the VPN; disconnecting reverts — through the running daemon. Manual log with `resolvectl status <tun>` before/after in the PR.
- `>STATE` parsing, `PUSH_REPLY` DNS parsing, state→transition mapping, and dedup unit-tested as pure functions with real-capture fixtures.
- Verified: a management-socket drop while the VPN stays up does **not** revert rules; reconnect resumes watching after an OpenVPN restart.
- No-pushed-DNS behavior implemented and verified.
- Pre-3c configs still load and still select the NM detector (back-compat test).
- NM (3a) and macOS (3b) detectors unchanged — no regression in their tests or behavior.
- 3a-deferred OpenVPN-over-NM live checks recorded in the PR.

## Finish

PR into `dev` titled `Phase 3c: OpenVPN standalone`. Description: investigation transcripts (management `state`/`log` captures, replay behavior), the event-source + detector-selection decisions with rationale, the socket-loss-vs-VPN-down and no-pushed-DNS handling, the manual verification log from real hardware, the done-criteria checklist, and the now-closed 3a follow-ups.
