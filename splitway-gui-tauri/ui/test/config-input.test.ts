// Truth-contract guardrail test (architecture §2): the GUI's only config writer —
// the interface selector — must round-trip the hidden vpn_backend / openvpn_*
// fields unchanged, so hiding them in the Variant B design can never silently
// reset an OpenVPN user's backend or endpoint.
//
// Pure logic (no DOM), so it runs under node after esbuild bundling — see
// ui/test.sh. Asserts with node:assert; a non-zero exit fails the run.

import assert from "node:assert/strict";

import { configInputForInterface } from "../src/config-input";
import type { ConfigFields } from "../src/bindings/view-model";

let passed = 0;
function test(name: string, fn: () => void): void {
  fn();
  passed += 1;
  console.log(`  ok  ${name}`);
}

const openvpnConfig: ConfigFields = {
  vpn_name: "tun0",
  vpn_backend: "openvpn",
  openvpn_management: "127.0.0.1:7505",
  openvpn_management_password_file: "/etc/splitway/mgmt.pass",
};

test("changing the interface preserves an OpenVPN backend + endpoint", () => {
  const sent = configInputForInterface(openvpnConfig, "wg0");
  assert.equal(sent.vpn_name, "wg0", "vpn_name is updated to the selected interface");
  // The hidden fields survive verbatim — the whole point of the guardrail.
  assert.equal(sent.vpn_backend, "openvpn");
  assert.equal(sent.openvpn_management, "127.0.0.1:7505");
  assert.equal(sent.openvpn_management_password_file, "/etc/splitway/mgmt.pass");
});

test("a NetworkManager config round-trips its (empty) openvpn fields too", () => {
  const nm: ConfigFields = {
    vpn_name: "tun0",
    vpn_backend: "network-manager",
    openvpn_management: "",
    openvpn_management_password_file: null,
  };
  const sent = configInputForInterface(nm, "eth0");
  assert.equal(sent.vpn_name, "eth0");
  assert.equal(sent.vpn_backend, "network-manager");
  assert.equal(sent.openvpn_management, "");
  assert.equal(sent.openvpn_management_password_file, null);
});

console.log(`config-input: ${passed} passed`);
