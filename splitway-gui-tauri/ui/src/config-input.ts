// The interface selector is the GUI's ONLY config writer (the backend / OpenVPN
// settings screen is gone in the Variant B design). Hiding those fields is safe
// ONLY if every write round-trips them unchanged: a `set_config` is a full update
// of {vpn_name, vpn_backend, openvpn_management, openvpn_management_password_file}
// (the daemon stores what it is sent), so omitting or defaulting a hidden field
// would silently reset an OpenVPN user's backend/endpoint.
//
// This pure read-modify-write builds the payload from the daemon's CURRENT config
// (the last view-model's `config`) and changes ONLY `vpn_name`, so the hidden
// fields are preserved by construction. Unit-tested in test/config-input.test.ts.

import type { ConfigFields } from "./bindings/view-model";
import type { ConfigInput } from "./api";

/** Build a `set_config` payload that selects `vpnName` while preserving every
 *  hidden config field from the daemon's current `config` verbatim. */
export function configInputForInterface(config: ConfigFields, vpnName: string): ConfigInput {
  return {
    vpn_name: vpnName,
    vpn_backend: config.vpn_backend,
    openvpn_management: config.openvpn_management,
    openvpn_management_password_file: config.openvpn_management_password_file,
  };
}
