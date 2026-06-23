// chipFor must not claim "in sync with system DNS" before a live read-back has
// actually succeeded and matched: the checked in-sync chip is allowed ONLY on
// verify Available + InSync. Unknown (no read-back yet) and Unavailable (read-back
// failed) must render an honest non-checked "routing active" chip. Pure logic;
// runs under node via ui/test.sh.

import assert from "node:assert/strict";

import { chipFor } from "../src/render";
import type { DriftVerdict, LinkDnsState, VerifyView } from "../src/bindings/view-model";

let passed = 0;
function test(name: string, fn: () => void): void {
  fn();
  passed += 1;
  console.log(`  ok  ${name}`);
}

const LIVE: LinkDnsState = {
  servers: ["192.0.2.53"],
  routing_domains: ["corp.example.com"],
  default_route: false,
};
const available = (drift: DriftVerdict): VerifyView => ({ state: "Available", live: LIVE, drift });

test("in-sync chip (✓) ONLY on Available + InSync", () => {
  const chip = chipFor("healthy", available("InSync"));
  assert.equal(chip?.check, true);
  assert.match(chip?.text ?? "", /in sync/i);
});

test("Unknown verify → routing active, NOT an in-sync claim", () => {
  const chip = chipFor("healthy", { state: "Unknown" });
  assert.equal(chip?.check, false);
  assert.doesNotMatch(chip?.text ?? "", /in sync/i);
});

test("Unavailable verify → routing active, NOT an in-sync claim", () => {
  const chip = chipFor("healthy", { state: "Unavailable", message: "verify unavailable" });
  assert.equal(chip?.check, false);
  assert.doesNotMatch(chip?.text ?? "", /in sync/i);
});

test("drifted verify → amber, not checked", () => {
  const chip = chipFor(
    "healthy",
    available({
      Drifted: { missing_servers: [], unrouted_domains: ["corp.example.com"], default_route_leak: false },
    }),
  );
  assert.equal(chip?.tone, "warn");
  assert.equal(chip?.check, false);
});

test("non-healthy modes keep their chips (or none)", () => {
  assert.equal(chipFor("off", { state: "Unknown" }), null);
  assert.equal(chipFor("empty", { state: "Unknown" }), null);
  assert.equal(chipFor("waiting", { state: "Unknown" })?.tone, "warn");
  assert.equal(chipFor("apply-failed", { state: "Unknown" })?.tone, "bad");
});

console.log(`chip: ${passed} passed`);
