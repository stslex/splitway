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
