// Hand-written TypeScript mirror of `splitway_gui_core::ViewModelSnapshot` and
// the shared wire types it embeds. This is the read-only view-model the daemon →
// gui-core → Tauri pipeline serializes and the frontend renders.
//
// Keep this in lockstep with the Rust side. Two guards back that up (see
// docs/design/tauri-read-only.md):
//   - a Rust test asserts a sample VM serializes to the committed
//     `view-model.sample.json` (locks the exact JSON shape, incl. enum reprs);
//   - `test/render.test.ts` checks that fixture satisfies this mirror's top-level
//     shape at compile time and renders without crashing.
//
// Serde representation notes (must match):
//   - unit-only enums (Health, RoutingState) serialize as bare strings;
//   - enums with data are externally tagged: DetectorHealth.Error -> {"Error": s},
//     DriftVerdict.Drifted -> {"Drifted": {...}};
//   - VpnBackend is kebab-case ("network-manager" | "openvpn");
//   - VerifyView is internally tagged on "state" (a Tauri-side type we control).

export type Health =
  | "Unknown"
  | "Connected"
  | "NotRunning"
  | "PermissionDenied"
  | "VersionMismatch"
  | "TransientError";

export type RoutingState =
  | "Disabled"
  | "NoDomains"
  | "VpnDown"
  | "NoDnsFromVpn"
  | "Applied"
  | "ApplyFailed"
  | "ConfigInvalid";

export type DetectorHealth = "Active" | "Inactive" | { Error: string };

export type VpnBackend = "network-manager" | "openvpn";

export type MessageKind = "Info" | "Error";

export interface ConnectionState {
  health: Health;
  message: string | null;
}

export interface AppliedInfo {
  interface: string;
  domains: string[];
  dns_servers: string[];
}

export interface StatusInfo {
  enabled: boolean;
  interface: string;
  vpn_up: boolean;
  applied: AppliedInfo | null;
  routing_state: RoutingState;
  /** DNS server(s) the configured interface is currently *detected* to expose,
   *  independent of whether routing is applied (empty when none / interface down /
   *  no DNS pushed). Drives the read-only "Using <ip> · detected from <iface>"
   *  readout and the DNS-not-detected state. */
  detected_dns: string[];
  detector_health: DetectorHealth;
  domains: string[];
}

export interface InterfaceInfo {
  name: string;
  up: boolean;
  vpn_like: boolean;
}

export interface ConfigFields {
  vpn_name: string;
  vpn_backend: VpnBackend;
  openvpn_management: string;
  openvpn_management_password_file: string | null;
}

export interface LinkDnsState {
  servers: string[];
  routing_domains: string[];
}

// DriftVerdict: externally-tagged Rust enum.
export type DriftVerdict =
  | "NotApplicable"
  | "InSync"
  | { Drifted: { missing_servers: string[]; unrouted_domains: string[] } };

// VerifyView: internally tagged on "state" (Tauri-side type).
export type VerifyView =
  | { state: "Unknown" }
  | { state: "Available"; live: LinkDnsState; drift: DriftVerdict }
  | { state: "Unavailable"; message: string };

export interface MessageView {
  kind: MessageKind;
  text: string;
}

export interface ViewModel {
  connection: ConnectionState;
  connected: boolean;
  working: boolean;
  status: StatusInfo | null;
  interfaces: InterfaceInfo[];
  config_loaded: boolean;
  config: ConfigFields | null;
  config_path: string;
  verify: VerifyView;
  message: MessageView | null;
}

// --- Command-path types (Phase 7c) ------------------------------------------
//
// These are NOT part of the polled view-model — they are the return type of the
// one-shot `check_domain` command. A parameterized query result is not ambient
// config truth, so it is never folded into `ViewModel` (see
// docs/design/tauri-mutations.md). Mirrors `splitway_gui_core::CheckOutcome` and
// the `splitway_shared::ipc::DomainCheckInfo` it embeds; the gui-core
// `check_outcome_serializes_internally_tagged_on_state` test locks the Rust shape.

// The live-resolution result for one host (best-effort attribution).
export interface ResolutionInfo {
  addresses: string[];
  via_interface: string | null;
  via_dns: string | null;
}

// The daemon's route-check: coverage + best-effort live resolution. Reports
// which resolver answered, NOT reachability (Splitway governs DNS, not IP routing).
export interface DomainCheckInfo {
  host: string;
  covered: boolean;
  matched_domain: string | null;
  vpn_interface: string;
  resolution: ResolutionInfo | null;
  enabled: boolean;
  vpn_up: boolean;
  routing_state: RoutingState;
}

// CheckOutcome: internally tagged on "state" (a gui-core type we control), like
// VerifyView. `Checked` carries the daemon result; `Error` carries a reason.
export type CheckOutcome =
  | { state: "Checked"; result: DomainCheckInfo }
  | { state: "Error"; message: string };
