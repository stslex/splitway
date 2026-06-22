// canUndoReadd decides whether a deleted domain can be faithfully restored by
// re-adding it via AddDomain (which normalizes/validates). Offer Undo only for
// values that round-trip unchanged; legacy/odd entries (IP, host:port, uppercase,
// trailing dot, ~marker) must NOT get an Undo that would fail or change the host.
// Pure logic; runs under node via ui/test.sh.

import assert from "node:assert/strict";

import { canUndoReadd } from "../src/domain-undo";

let passed = 0;
function test(name: string, fn: () => void): void {
  fn();
  passed += 1;
  console.log(`  ok  ${name}`);
}

test("canonical lowercase hostnames are undoable", () => {
  for (const d of [
    "corp.example.com",
    "vpn.example.org",
    "a.b.c.example",
    "host-1.example.net",
    "my-host.example.com", // interior hyphen ok
    "xn--caf-dma.example.com", // punycode (xn--…) ok
  ]) {
    assert.equal(canUndoReadd(d), true, d);
  }
});

test("non-round-trippable legacy/odd values are NOT undoable", () => {
  for (const d of [
    "192.0.2.1", // IPv4 literal
    "example.com:443", // host:port
    "Example.COM", // uppercase (would be lowercased)
    "example.com.", // trailing dot (would be stripped)
    "~corp.example.com", // routing-only marker
    "-corp.example", // leading-hyphen label (validate_host rejects)
    "corp-.example", // trailing-hyphen label (validate_host rejects)
    "under_score.example", // underscore
    "has space.example", // whitespace
    "https://example.com", // scheme/path
    "", // empty
    " corp.example.com", // untrimmed
    ".leading", // leading dot
    "double..dot", // empty label
  ]) {
    assert.equal(canUndoReadd(d), false, d);
  }
});

console.log(`domain-undo: ${passed} passed`);
