// Undo of a domain delete re-adds it via the `AddDomain` command, which the daemon
// NORMALIZES and validates. So undo can only faithfully restore a value that
// `AddDomain` accepts AND reproduces unchanged. A legacy / externally-edited entry
// that `RemoveDomain` can delete but `AddDomain` would reject or normalize
// differently (an IP literal like `192.0.2.1`, a `host:port`, an uppercase or
// trailing-dot host, a `~`-prefixed routing marker, …) cannot be round-tripped —
// offering Undo for it would fail or silently restore a different host.
//
// This is a CONSERVATIVE client-side guard: it returns true only for values that
// are already a canonical lowercase hostname (so re-adding yields exactly the same
// string). It deliberately errs toward false (no Undo offered) for anything
// ambiguous — a missed Undo is safe; a misleading one is not. Pure; unit-tested in
// test/domain-undo.test.ts.

/** Whether deleting `domain` can be undone by re-adding it unchanged. */
export function canUndoReadd(domain: string): boolean {
  if (domain === "" || domain !== domain.trim()) return false;
  if (domain !== domain.toLowerCase()) return false; // AddDomain would lowercase it
  // No scheme/port/path, routing-only marker, underscore, or whitespace.
  if (/[\s/:~_]/.test(domain)) return false;
  if (domain.startsWith(".") || domain.endsWith(".") || domain.includes("..")) return false;
  const labels = domain.split(".");
  if (!labels.every((label) => /^[a-z0-9-]+$/.test(label))) return false;
  // All-numeric labels read as an IPv4 literal, which AddDomain does not round-trip.
  if (labels.every((label) => /^[0-9]+$/.test(label))) return false;
  return true;
}
