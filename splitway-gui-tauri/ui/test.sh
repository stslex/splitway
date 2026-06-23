#!/usr/bin/env sh
# Run the frontend's pure-logic unit tests WITHOUT npm/jsdom: esbuild bundles each
# test (TypeScript) to a temp ESM file, node runs it. Pure logic only (no DOM) —
# the DOM-building code is exercised in the live e2e. Provided by the Nix dev
# shell (nodejs + esbuild). Run from `nix develop`:  sh splitway-gui-tauri/ui/test.sh
set -eu

cd "$(dirname "$0")"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

status=0
for test_file in test/*.test.ts; do
  out="$tmp/$(basename "$test_file" .ts).mjs"
  echo "==> $test_file"
  esbuild "$test_file" --bundle --format=esm --platform=node --packages=external --outfile="$out"
  if ! node "$out"; then
    status=1
  fi
done

if [ "$status" -eq 0 ]; then
  echo "==> frontend unit tests passed"
else
  echo "==> frontend unit tests FAILED" >&2
fi
exit "$status"
