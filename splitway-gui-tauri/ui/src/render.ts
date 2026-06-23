// Pure VM → presentation helpers for the Variant B design. No local state, no
// command wiring, no event handlers — these turn a view-model into DOM nodes and
// presentation facts and nothing else. The interactive sections (toggle, interface
// select, add/delete, check) live in app.ts, which composes these.
//
// THE TRUTH CONTRACT, structural here: DOM is built with createElement /
// createElementNS + textContent — NEVER innerHTML with interpolated daemon
// strings — so a domain name or daemon message can never inject markup. (A
// `grep -rn innerHTML ui/src` returning nothing is the greppable invariant; keep
// it that way.) These helpers derive everything from the pushed view-model; they
// hold no authoritative state.

import type { DriftVerdict, Health, StatusInfo, VerifyView, ViewModel } from "./bindings/view-model";
import type { HostPlatform } from "./api";

const SVG_NS = "http://www.w3.org/2000/svg";

/** Tiny element helper: tag + optional class/text + children. */
export function el(
  tag: string,
  opts: { class?: string; text?: string } = {},
  children: Node[] = [],
): HTMLElement {
  const node = document.createElement(tag);
  if (opts.class) node.className = opts.class;
  if (opts.text !== undefined) node.textContent = opts.text;
  for (const child of children) node.appendChild(child);
  return node;
}

/** SVG element helper (createElementNS so static icons never need innerHTML). */
export function svgEl(tag: string, attrs: Record<string, string>, children: SVGElement[] = []): SVGElement {
  const node = document.createElementNS(SVG_NS, tag);
  for (const [key, value] of Object.entries(attrs)) node.setAttribute(key, value);
  for (const child of children) node.appendChild(child);
  return node;
}

/** The bare brand mark, inline in the topbar: the same logo *concept* as the
 *  distribution asset (assets/icon/splitway-icon-mark.svg) — a split path, one
 *  branch routed through the accent (VPN), one to slate (direct) — but with its
 *  own geometry, hand-tuned for this 40-viewBox / 25px inline render (the mockup's
 *  mark) rather than a scaled-down copy of the 512-viewBox asset. Built with
 *  createElementNS, not innerHTML. */
export function brandMark(): SVGElement {
  return svgEl("svg", { class: "logo", viewBox: "0 0 40 40", width: "25", height: "25", fill: "none", "aria-label": "Splitway" }, [
    svgEl("path", {
      d: "M10 20 H18 C23 20 25.5 17 29 11",
      stroke: "var(--accent)",
      "stroke-width": "3.5",
      "stroke-linecap": "round",
      "stroke-linejoin": "round",
    }),
    svgEl("path", {
      d: "M18 20 C23 20 25.5 23 29 29",
      stroke: "var(--slate)",
      "stroke-width": "2.4",
      "stroke-linecap": "round",
      "stroke-linejoin": "round",
    }),
    svgEl("circle", { cx: "10", cy: "20", r: "3.1", fill: "var(--accent)" }),
    svgEl("circle", { cx: "30", cy: "10", r: "3.1", fill: "var(--accent)" }),
    svgEl("circle", { cx: "30", cy: "30", r: "2.3", fill: "var(--slate)" }),
  ]);
}

/** Blocker glyphs (unplug / lock / warning-triangle), built as SVG nodes. */
export function blockerIcon(variant: BlockerVariant): SVGElement {
  switch (variant) {
    case "disconnected":
    case "error":
      return svgEl("svg", { viewBox: "0 0 24 24", fill: "none", stroke: "currentColor", "stroke-width": "2" }, [
        svgEl("circle", { cx: "12", cy: "12", r: "9" }),
        svgEl("path", { d: "M8 8l8 8", "stroke-linecap": "round" }),
      ]);
    case "no-permission":
      return svgEl("svg", { viewBox: "0 0 24 24", fill: "none", stroke: "currentColor", "stroke-width": "2" }, [
        svgEl("rect", { x: "5", y: "11", width: "14", height: "9", rx: "2" }),
        svgEl("path", { d: "M8 11V8a4 4 0 0 1 8 0v3" }),
      ]);
    case "frozen":
    case "version":
      return svgEl(
        "svg",
        { viewBox: "0 0 24 24", fill: "none", stroke: "currentColor", "stroke-width": "2", "stroke-linejoin": "round", "stroke-linecap": "round" },
        [svgEl("path", { d: "M12 4 22 20H2z" }), svgEl("path", { d: "M12 10v4M12 17h.01" })],
      );
  }
}

// --- presentation derivation (pure) -----------------------------------------

export type BlockerVariant = "disconnected" | "no-permission" | "frozen" | "version" | "error";

/** An in-app remediation button a blocker can offer (macOS self-install): the
 *  one-click action that resolves the degraded state, instead of a copy-paste
 *  terminal command. `key` selects which privileged command runs. Presentation
 *  only — it is computed frontend-side from health + platform and is NOT part of
 *  the serialized view-model, so it never touches the bindings contract. */
export interface BlockerAction {
  key: "install";
  label: string;
}

/** A full-window blocker: a degraded state the user must resolve before the main
 *  UI is meaningful. Carries its own copy + (a fix command and/or an action). */
export interface BlockerView {
  variant: BlockerVariant;
  /** "neutral" (informational, e.g. daemon down) vs "warn" (needs attention). */
  tone: "neutral" | "warn";
  title: string;
  /** Body paragraph; `codeWord` (if set) is rendered as an inline code span
   *  spliced where `{}` appears in `body`. */
  body: string;
  codeWord?: string;
  /** A copy-paste fix command shown in a code block. */
  command?: string;
  /** An in-app remediation button (macOS), shown instead of / beside `command`. */
  action?: BlockerAction;
}

/** The main-screen mode, mapped from the daemon's routing belief. Drives the
 *  hero status line, the chip, dimming, and the interface-block DNS readout. */
export type MainMode = "healthy" | "off" | "waiting" | "dns-missing" | "empty" | "apply-failed";

/** What the whole window should show this frame. `connecting` before the first
 *  reply; `blocker` for a full-window degraded state; `main` for the live UI. */
export type Stage =
  | { kind: "connecting" }
  | { kind: "blocker"; blocker: BlockerView }
  | { kind: "main"; mode: MainMode; status: StatusInfo };

/** The topbar connection indicator. */
export interface ConnIndicator {
  text: string;
  level: "ok" | "warn" | "off";
}

/** The malformed-config blocker. The fix command points at the daemon's ACTUAL
 *  active config path (from the snapshot), not a hardcoded default — a hand-started
 *  or `--config` daemon may use `~/.config/splitway/config.json` or a custom path,
 *  and the blocker returns before the footer, so this is the only path shown in the
 *  frozen state. Falls back to the NixOS-service default only when the path is
 *  unknown (empty). */
function frozenBlocker(configPath: string): BlockerView {
  return {
    variant: "frozen",
    tone: "warn",
    title: "Configuration can't be loaded",
    body: "The config file is malformed, so Splitway kept the last working setup. Fix the file to make changes again.",
    command: configPath || "/var/lib/splitway/config.json",
  };
}

/** The `NotRunning` blocker. On macOS the daemon isn't a thing the user starts by
 *  hand — the app installs it — so offer the one-click Install action (escalates
 *  via the native password prompt) instead of a `systemctl` line that doesn't
 *  apply. On Linux/other, keep the copy-paste status command. */
function notRunningBlocker(platform: HostPlatform): BlockerView {
  if (platform === "macos") {
    return {
      variant: "disconnected",
      tone: "neutral",
      title: "Splitway isn't running yet",
      body: "Install the background service to start routing. You'll be asked for your password once — no Terminal needed.",
      action: { key: "install", label: "Install & start the Splitway service" },
    };
  }
  return {
    variant: "disconnected",
    tone: "neutral",
    title: "Can't reach Splitway",
    body: "The background service isn't responding. Make sure it's running, then it'll reconnect on its own.",
    command: "systemctl status splitway",
  };
}

/** The `PermissionDenied` blocker. On macOS the installer already added the user
 *  to the `splitway` group, but macOS only applies new group access at login —
 *  so the remaining step is a sign-out, NOT a `usermod` (which is the wrong OS and
 *  already done). On Linux, keep the add-to-group command + re-login guidance. */
function permissionDeniedBlocker(platform: HostPlatform): BlockerView {
  if (platform === "macos") {
    return {
      variant: "no-permission",
      tone: "warn",
      title: "Almost done — sign out to finish",
      body: "Splitway is installed and your account was added to the {} group. macOS only applies new group access at login, so sign out and back in (or restart) to start controlling routing.",
      codeWord: "splitway",
    };
  }
  return {
    variant: "no-permission",
    tone: "warn",
    title: "No permission to make changes",
    body: "Your user isn't in the {} group. Add it, then sign out and back in.",
    codeWord: "splitway",
    command: "sudo usermod -aG splitway $USER",
  };
}

/** Map a non-connected (or frozen) condition to its full-window blocker, or null
 *  when the main UI should render. ConfigInvalid is a blocker even while the link
 *  is healthy (the daemon froze on the last-good config). `platform` selects the
 *  macOS self-install affordances over the Linux copy-paste commands. */
function blockerFor(vm: ViewModel, platform: HostPlatform): BlockerView | null {
  const health: Health = vm.connection.health;
  if (health === "NotRunning") return notRunningBlocker(platform);
  if (health === "TransientError") {
    return {
      variant: "error",
      tone: "neutral",
      title: "Can't reach Splitway",
      body: "The background service isn't responding. Make sure it's running, then it'll reconnect on its own.",
      command: platform === "macos" ? undefined : "systemctl status splitway",
    };
  }
  if (health === "PermissionDenied") return permissionDeniedBlocker(platform);
  if (health === "VersionMismatch") {
    return {
      variant: "version",
      tone: "warn",
      title: "Update needed",
      // The daemon's own version-skew guidance is authoritative; surface it.
      body: vm.connection.message ?? "The app and the background service speak different versions. Update Splitway so they match.",
    };
  }
  // Connected, but the on-disk config is frozen-invalid: a blocker too. Show the
  // daemon's actual active config path so the fix points at the right file.
  if (vm.status?.routing_state === "ConfigInvalid") return frozenBlocker(vm.config_path);
  return null;
}

/** Reduce the VM to the stage the window renders. `platform` only affects blocker
 *  copy/affordances (the main stage is platform-agnostic). */
export function stageFor(vm: ViewModel, platform: HostPlatform): Stage {
  if (vm.connection.health === "Unknown") return { kind: "connecting" };
  const blocker = blockerFor(vm, platform);
  if (blocker) return { kind: "blocker", blocker };
  // Connected with a trustworthy status (dropped to null only on a non-status
  // reply, which `blockerFor` already routed to a blocker via health).
  if (!vm.status) return { kind: "connecting" };
  return { kind: "main", mode: mainMode(vm.status), status: vm.status };
}

/** The main mode from enabled + routing_state (ConfigInvalid handled upstream).
 *  Mirrors the daemon's `routing_state()` precedence: a failed apply/revert
 *  (`ApplyFailed` — stale split-DNS rules may still be installed) outranks the
 *  clean `Disabled` state, so a *disable whose revert failed* shows out-of-sync +
 *  the Resync action rather than a reassuring "off". Checking `!enabled` first
 *  would mask that. */
export function mainMode(status: StatusInfo): MainMode {
  if (status.routing_state === "ApplyFailed") return "apply-failed";
  if (!status.enabled) return "off";
  switch (status.routing_state) {
    case "NoDomains":
      return "empty";
    case "VpnDown":
      return "waiting";
    case "NoDnsFromVpn":
      return "dns-missing";
    case "Applied":
      return "healthy";
    // Disabled is covered by !enabled above; ApplyFailed/ConfigInvalid are handled
    // before this switch (the latter is a full-window blocker upstream).
    default:
      return "healthy";
  }
}

/** The topbar connection indicator for the live (non-blocker) stages. */
export function connIndicator(mode: MainMode): ConnIndicator {
  switch (mode) {
    case "waiting":
      return { text: "VPN not connected", level: "warn" };
    case "apply-failed":
      return { text: "Out of sync", level: "warn" };
    default:
      return { text: "Connected", level: "ok" };
  }
}

function plural(n: number): string {
  return `${n} domain${n === 1 ? "" : "s"}`;
}

/** The hero status line as nodes (so the interface / count get their own styled
 *  spans), keyed off the mode. Plain text via textContent — never innerHTML. */
export function statusLineNodes(mode: MainMode, status: StatusInfo): Node[] {
  const iface = status.interface || "this interface";
  const n = status.domains.length;
  switch (mode) {
    case "healthy":
      return [
        document.createTextNode("Routing "),
        el("span", { class: "n", text: plural(n) }),
        document.createTextNode(" through "),
        el("span", { class: "if", text: iface }),
      ];
    case "off":
      return [
        document.createTextNode("Routing paused — all traffic goes "),
        el("span", { class: "if", text: "direct" }),
      ];
    case "waiting":
      return [
        document.createTextNode("Enabled — "),
        el("span", { class: "n", text: "waiting" }),
        document.createTextNode(" for the VPN to connect"),
      ];
    case "dns-missing":
      return [
        document.createTextNode("Waiting — no DNS on "),
        el("span", { class: "if", text: iface }),
      ];
    case "empty":
      return [
        el("span", { class: "n", text: "No domains yet" }),
        document.createTextNode(" — add one to start routing"),
      ];
    case "apply-failed":
      return [
        document.createTextNode("Routing "),
        el("span", { class: "if", text: iface }),
        document.createTextNode(" — rules are out of sync"),
      ];
  }
}

export interface ChipView {
  tone: "ok" | "warn" | "bad";
  /** Leading ✓ glyph only on the healthy in-sync chip. */
  check: boolean;
  text: string;
}

/** The hero status chip, or null when no chip is shown (off / empty). Healthy-mode
 *  copy reflects the live verify state honestly: the checked "in sync" chip is
 *  claimed ONLY on a successful read-back that matched (`Available` + `InSync`).
 *  Before the first read-back lands (`Unknown`) or after one fails (`Unavailable`)
 *  the chip says routing is active without asserting a sync that was never
 *  verified — see the P2 review note. */
export function chipFor(mode: MainMode, verify: VerifyView): ChipView | null {
  switch (mode) {
    case "off":
    case "empty":
      return null;
    case "waiting":
      return { tone: "warn", check: false, text: "Waiting for the VPN" };
    case "dns-missing":
      return { tone: "warn", check: false, text: "No DNS — routing paused" };
    case "apply-failed":
      return { tone: "bad", check: false, text: "Rules out of sync" };
    case "healthy":
      if (verify.state === "Available") {
        const drift = verify.drift;
        if (typeof drift === "object" && "Drifted" in drift) {
          // A catch-all leak is the headline: the live link is the DNS default
          // route, so every unmatched name resolves through the VPN, not direct.
          if (drift.Drifted.default_route_leak) {
            return { tone: "warn", check: false, text: "Everything else leaks through the VPN" };
          }
          return { tone: "warn", check: false, text: "Some domains have drifted" };
        }
        if (drift === "InSync") {
          return { tone: "ok", check: true, text: "In sync with system DNS" };
        }
        // NotApplicable: nothing was believed-installed to compare against, so
        // routing is active but there is no sync to claim.
        return { tone: "ok", check: false, text: "Routing active" };
      }
      if (verify.state === "Unavailable") {
        return { tone: "ok", check: false, text: "Routing active · live DNS check unavailable" };
      }
      // Unknown: the first connected poll has not read DNS back yet.
      return { tone: "ok", check: false, text: "Routing active · checking system DNS…" };
  }
}

/** The drift verdict carried by the snapshot, or null when verify is not Available. */
export function driftOf(vm: ViewModel): DriftVerdict | null {
  return vm.verify.state === "Available" ? vm.verify.drift : null;
}

/** Whether `domain` has drifted (live state does not route it), per the verdict. */
export function domainDrifted(drift: DriftVerdict | null, domain: string): boolean {
  if (!drift || typeof drift !== "object" || !("Drifted" in drift)) return false;
  return drift.Drifted.unrouted_domains.includes(domain);
}

/** Whether the live link is a catch-all (the DNS default route) while the
 *  configured split is narrow — i.e. every name NOT in the configured domains
 *  leaks through the VPN instead of going direct. Drives the "Everything else"
 *  row, which would otherwise falsely claim "direct". */
export function defaultRouteLeak(drift: DriftVerdict | null): boolean {
  if (!drift || typeof drift !== "object" || !("Drifted" in drift)) return false;
  return drift.Drifted.default_route_leak;
}
