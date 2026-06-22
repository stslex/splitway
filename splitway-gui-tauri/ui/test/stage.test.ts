// stageFor maps the view-model to the window stage. Locks the blocker mapping —
// in particular that the frozen (ConfigInvalid) blocker points at the daemon's
// ACTUAL active config path, not a hardcoded default. Pure logic; runs under node
// via ui/test.sh.

import assert from "node:assert/strict";

import { stageFor } from "../src/render";
import type { Health, RoutingState, ViewModel } from "../src/bindings/view-model";

let passed = 0;
function test(name: string, fn: () => void): void {
  fn();
  passed += 1;
  console.log(`  ok  ${name}`);
}

function baseVm(): ViewModel {
  return {
    connection: { health: "Connected", message: null },
    connected: true,
    working: false,
    status: {
      enabled: true,
      interface: "tun0",
      vpn_up: true,
      applied: null,
      routing_state: "Applied",
      detected_dns: [],
      detector_health: "Active",
      domains: [],
    },
    interfaces: [],
    config_loaded: true,
    config: {
      vpn_name: "tun0",
      vpn_backend: "network-manager",
      openvpn_management: "",
      openvpn_management_password_file: null,
    },
    config_path: "/etc/splitway/config.json",
    verify: { state: "Unknown" },
    message: null,
  };
}

function withHealth(health: Health): ViewModel {
  const v = baseVm();
  v.connection = { health, message: health === "VersionMismatch" ? "update splitway" : null };
  if (health !== "Connected") v.status = null;
  return v;
}

function frozen(configPath: string): ViewModel {
  const v = baseVm();
  v.config_path = configPath;
  (v.status as NonNullable<ViewModel["status"]>).routing_state = "ConfigInvalid" as RoutingState;
  return v;
}

test("frozen blocker points at the daemon's actual config path", () => {
  const stage = stageFor(frozen("/home/user/.config/splitway/config.json"));
  assert.equal(stage.kind, "blocker");
  if (stage.kind !== "blocker") return;
  assert.equal(stage.blocker.variant, "frozen");
  assert.equal(stage.blocker.command, "/home/user/.config/splitway/config.json");
});

test("frozen blocker falls back to the default path when unknown", () => {
  const stage = stageFor(frozen(""));
  assert.equal(stage.kind === "blocker" && stage.blocker.command, "/var/lib/splitway/config.json");
});

test("connection health maps to the right blocker / main", () => {
  const cases: [Health, string][] = [
    ["NotRunning", "disconnected"],
    ["PermissionDenied", "no-permission"],
    ["VersionMismatch", "version"],
    ["TransientError", "error"],
  ];
  for (const [health, variant] of cases) {
    const stage = stageFor(withHealth(health));
    assert.equal(stage.kind, "blocker", `${health} should be a blocker`);
    if (stage.kind === "blocker") assert.equal(stage.blocker.variant, variant);
  }
  assert.equal(stageFor(withHealth("Connected")).kind, "main");
  assert.equal(stageFor(withHealth("Unknown")).kind, "connecting");
});

console.log(`stage: ${passed} passed`);
