---
name: platform-dns
description: Reference for the platform DNS/VPN interfaces Splitway builds on — systemd-resolved (resolvectl), NetworkManager D-Bus API and nmcli output, macOS resolver. Use when implementing or reviewing VpnDetector / DnsBackend code, NM event watching, or platform backends.
---

# Platform DNS/VPN reference

Curated facts for Splitway's platform layer. When something here disagrees with the live system, trust the system: verify D-Bus details with `busctl introspect`, resolvectl behavior with `resolvectl --help` / `man resolvectl`.

## systemd-resolved / resolvectl (Linux DnsBackend)

- `resolvectl dns <iface> <server...>` — set per-link DNS servers
- `resolvectl domain <iface> <domain...>` — set per-link domains. A bare `example.com` is both a *search* domain and a *routing* domain; `~example.com` is routing-only (queries for `*.example.com` go to this link's DNS, but the domain is not appended to bare hostnames); `~.` routes *all* queries to this link
- `resolvectl revert <iface>` — drop per-link overrides, back to whatever the link's manager (NM/DHCP) configured. Also clears the per-link default-route override (below), so it is the only revert step needed
- `resolvectl status <iface>` — current per-link DNS state. The `Default Route: yes/no` line and the `Protocols: ±DefaultRoute` token report the catch-all flag below
- `resolvectl default-route <iface> <bool>` — get/set whether the link is the DNS **default route** (catch-all). **Critical for split-DNS on a full-tunnel VPN:** systemd-resolved sends every query that matches *no* link's routing domain to all links flagged as the default route, and it auto-sets this flag `yes` on the link carrying the default *IP* route (a full-tunnel VPN). So setting per-link DNS + routing domains is **not enough** — the link is still the catch-all and *all* DNS leaks to the VPN resolver. Splitway's apply runs `resolvectl default-route <iface> false` so the link resolves only its routing domains. See [`docs/design/linux-default-route-catch-all.md`](../../../docs/design/linux-default-route-catch-all.md)
- Needs privileges (root or polkit) for mutating calls
- **Implemented:** Splitway applies config domains routing-only (`~domain`) — routes `*.domain` without polluting the search list — and reads back the default-route flag into `LinkDnsState.default_route` so `compare_drift` flags a catch-all leak (a link that re-becomes the default route, e.g. after an NM reconnect re-asserts the flag)

## NetworkManager D-Bus (Linux VpnDetector)

Bus: system bus, name `org.freedesktop.NetworkManager`.

- Manager object `/org/freedesktop/NetworkManager`, interface `org.freedesktop.NetworkManager`:
  - `GetDeviceByIpIface(s) -> o` — device object path by interface name (errors if absent)
  - Signals `DeviceAdded(o)` / `DeviceRemoved(o)` — needed to catch tun devices created on VPN connect
- Device objects implement `org.freedesktop.NetworkManager.Device`:
  - Signal `StateChanged(u new_state, u old_state, u reason)`
  - Property `Interface` (s) — match against the configured interface name
- `NMDeviceState` (the values that matter): `10` unmanaged, `20` unavailable, `30` disconnected, `40–90` activation stages, `100` **activated**, `110` deactivating, `120` failed
- Practical mapping: `100` → VPN up; `30`/`120` (and device removal) → VPN down; ignore intermediate stages; deduplicate repeats — NM can re-emit states
- GlobalProtect/OpenVPN typically appear as `tun*` devices; WireGuard as `wireguard` type. The device may not exist until the VPN client starts — watch must survive that
- zbus: generate typed proxies from introspection (`zbus-xmlgen` or `busctl introspect --xml-interface`) instead of hand-writing signatures
- **OpenVPN-via-NM subtlety:** an OpenVPN connection imported into NM is modelled as a *VPN active connection*, not only a device. NM still creates a `tun*` device whose `StateChanged` fires, but the authoritative VPN up/down may live on the active-connection object (`org.freedesktop.NetworkManager.VPN.Connection`, `VpnStateChanged(u state, u reason)`, state `5` = activated, `7`/`8` = disconnected). Verify with `nmcli connection show --active` + `busctl introspect` whether device `StateChanged` alone is sufficient for the OpenVPN case before adding a second signal source. `nmcli device show tun0` for DNS works regardless

## nmcli (current detect path)

`nmcli device show <iface>` prints `KEY: value` lines; DNS lives in `IP4.DNS[n]` / `IP6.DNS[n]` entries. Parsing is implemented and unit-tested in `splitway-daemon/src/.../parser.rs` — extend tests there if the format handling changes.

## macOS (Phase 3 — verify on hardware before relying on this)

- Split DNS: write `/etc/resolver/<domain>` files; keys per `man 5 resolver`: `nameserver <ip>` (repeatable), optional `search_order`, `port`. Remove the file to revert. Needs root
- Inspect resolver state: `scutil --dns`; VPN interfaces show up as `utun*`
- After changing resolver files flush caches: `dscacheutil -flushcache` and `killall -HUP mDNSResponder`
- No systemd/NM here: VPN detection and event source need a separate mechanism. Preferred event source is **SCDynamicStore** (SystemConfiguration framework) — subscribe to keys like `State:/Network/Interface/utun[0-9]+/IPv4` and `State:/Network/Global/DNS` and get a callback on change; the `system-configuration` crate (0.7, paired with `core-foundation` 0.9 — not 0.10) exposes `SCDynamicStore` + a run-loop source. Read current DNS/interface state with `scutil --dns` / `scutil --nwi` or via the same store. Polling `scutil` on a timer is the fallback — mark it as a crutch if used. The run-loop blocks, so it lives on its own `std` thread feeding the same `mpsc::Sender<VpnEvent>` the Linux detector uses (via `blocking_send`); `SCDynamicStore` is not `Send`, so build it on that thread. The callback is a bare `fn` pointer — carry state through its `&mut T` context
