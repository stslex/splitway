# Phase 3a — OpenVPN via NetworkManager

Implement sub-phase 3a from `ROADMAP.md` (Phase 3 — Backend breadth). Read `CLAUDE.md` first and use the `platform-dns` skill (note the OpenVPN-via-NM subtlety section). Scope: make OpenVPN connections managed by NetworkManager work end-to-end, reusing the existing Linux detector and resolvectl backend.

Parallel-safe with Phase 3b (macOS): 3a touches only Linux detector/backend + tests, 3b touches only macOS files. Run them in separate worktrees.

## Branch

Branch `phase-3a-openvpn-nm` from up-to-date `dev`.

## Investigate first (write findings into the PR description)

The Linux detector already parses `IP4.DNS[n]`/`IP6.DNS[n]` generically and takes the interface name from config, so a `tun0` OpenVPN device may largely work already. Before writing code, determine on a real OpenVPN-over-NM connection:

1. Does the `tun*` device's `StateChanged` D-Bus signal fire on connect/disconnect, or does the authoritative up/down live only on the VPN active-connection object (`VPN.Connection.VpnStateChanged`)? Verify with `nmcli connection show --active`, `nmcli -f all device show tun0`, and `busctl introspect`
2. Does `nmcli device show tun0` expose DNS in the same `IP4.DNS[n]` fields? Capture sample output and add it as a parser test fixture — **redact it first: replace real resolver IPs and internal domains with RFC 5737 placeholders (`192.0.2.x`) and `example.com`/`corp.example.com`; never commit real captured values**

The result decides whether 3a is "confirm + test + document" or "add a second D-Bus signal source".

**Evidence source when no OpenVPN-over-NM connection is available:** an openconnect/GlobalProtect connection managed by NM is an acceptable substitute for Q1 and Q2. NM routes every VPN plugin through the same generic `org.freedesktop.NetworkManager.VPN.Connection` interface, and normalizes pushed DNS from any plugin into the device's `IP4Config.Nameservers` (`IP4.DNS[n]` in nmcli) — so the D-Bus state-machine authority and the DNS field layout transfer directly to OpenVPN. Document the substitution and its limits in the PR: the openvpn-NM "use this connection only for resources on its network" routing-domain toggle and the no-pushed-DNS edge case are NOT exercised by openconnect. Treat the literal "OpenVPN-over-NM live toggle" as verified-by-equivalence here and deferred to a later standalone check, to be run when a real OpenVPN connection is available (the user will bring one up separately; sub-phase 3c also stands up a real OpenVPN). Record this as an open follow-up in the PR rather than a blocking criterion. **Redact every captured `nmcli`/`busctl` snippet (resolver IPs, internal domains, hostnames, MACs) to placeholder values before adding it to a fixture or the PR — the field layout is what matters, not the live data.**

## Tasks

### If device `StateChanged` is sufficient

- Add OpenVPN `nmcli device show` output as a new fixture in the detector parser tests (proves DNS parsing covers the OpenVPN field layout), with all addresses/domains redacted to placeholders
- Confirm the watch picks up the tun device appearing/disappearing; add a state-mapping test if OpenVPN surfaces any state value not already covered
- No new runtime code beyond what the investigation shows is missing

### If the VPN active-connection signal is required

- Extend the NM watch to also subscribe to `org.freedesktop.NetworkManager.VPN.Connection.VpnStateChanged` for the relevant active connection, mapping its states through a **pure function** alongside the existing device-state mapping (mirror the `state.rs` pattern; unit-test it)
- Merge both signal sources into the one `mpsc::Sender<VpnEvent>` with the existing `Deduper` so a device-state + vpn-state pair for the same transition does not emit two events
- Keep the D-Bus plumbing thin; only the mapping/dedup logic is unit-tested

### Config / docs

- If interface naming guidance differs for OpenVPN (e.g. `tun0` vs a NM connection name), document it in the README config section
- README: note OpenVPN (NetworkManager-managed) as a supported VPN alongside GlobalProtect

## Out of scope

- OpenVPN run standalone / via systemd (no NM) — that is sub-phase 3c, separate PR
- macOS (3b)
- Any change to the DNS backend (resolvectl already handles per-link DNS regardless of VPN type)

## Done criteria

- fmt, clippy, tests green on CI (ubuntu + macos compile)
- An OpenVPN connection managed by NM auto-applies rules on connect and reverts on disconnect through the running daemon — manual verification log in the PR
- New parser/state tests cover the OpenVPN field/state layout
- GlobalProtect behavior unchanged (no regression)

## Finish

PR into `dev` titled `Phase 3a: OpenVPN via NetworkManager`. Description: investigation findings (which signal(s) are authoritative, with redacted `nmcli`/`busctl` evidence — placeholder values only), what changed, manual verification log, done-criteria checklist.
