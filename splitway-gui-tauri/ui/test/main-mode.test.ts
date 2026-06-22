// mainMode must mirror the daemon's routing_state precedence — in particular a
// failed apply/revert (ApplyFailed: stale split-DNS rules may still be installed)
// outranks the clean Disabled state, so a disable-whose-revert-failed shows
// out-of-sync (+ Resync) rather than a reassuring "off". Pure logic; runs under
// node via ui/test.sh (esbuild → node, no jsdom).

import assert from "node:assert/strict";

import { mainMode } from "../src/render";
import type { RoutingState, StatusInfo } from "../src/bindings/view-model";

let passed = 0;
function test(name: string, fn: () => void): void {
  fn();
  passed += 1;
  console.log(`  ok  ${name}`);
}

function status(enabled: boolean, routingState: RoutingState): StatusInfo {
  return {
    enabled,
    interface: "tun0",
    vpn_up: true,
    applied: null,
    routing_state: routingState,
    detected_dns: [],
    detector_health: "Active",
    domains: [],
  };
}

test("ApplyFailed outranks a disabled toggle (stale rules → out-of-sync, not off)", () => {
  assert.equal(mainMode(status(false, "ApplyFailed")), "apply-failed");
  assert.equal(mainMode(status(true, "ApplyFailed")), "apply-failed");
});

test("a clean disabled state is off", () => {
  assert.equal(mainMode(status(false, "Disabled")), "off");
  // Even if routing_state lags as Applied, !enabled (with no ApplyFailed) is off.
  assert.equal(mainMode(status(false, "Applied")), "off");
});

test("enabled states map per routing_state", () => {
  assert.equal(mainMode(status(true, "Applied")), "healthy");
  assert.equal(mainMode(status(true, "NoDomains")), "empty");
  assert.equal(mainMode(status(true, "VpnDown")), "waiting");
  assert.equal(mainMode(status(true, "NoDnsFromVpn")), "dns-missing");
});

console.log(`main-mode: ${passed} passed`);
