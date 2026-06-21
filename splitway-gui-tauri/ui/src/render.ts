// Pure VM → DOM helpers for the read-only display sections. No local state, no
// event handlers, no commands — these turn a view-model into nodes and nothing
// else. The interactive controls (toggle, add/remove, config editor, check) live
// in app.ts, which composes these for the read-only parts.
//
// DOM is built with createElement + textContent (never innerHTML with interpolated
// daemon strings), so a domain name or error message from the daemon can never
// inject markup.

import type {
  DetectorHealth,
  DriftVerdict,
  Health,
  RoutingState,
  StatusInfo,
  VerifyView,
  ViewModel,
} from "./bindings/view-model";

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

/** A labelled row: "<label>: <value>". */
export function row(label: string, value: string): HTMLElement {
  return el("div", { class: "row" }, [
    el("span", { class: "label", text: label }),
    el("span", { class: "value", text: value }),
  ]);
}

export function section(title: string, body: Node[]): HTMLElement {
  return el("section", {}, [el("h2", { text: title }), ...body]);
}

const HEALTH_LABEL: Record<Health, string> = {
  Unknown: "Connecting…",
  Connected: "Connected",
  NotRunning: "Daemon not running",
  PermissionDenied: "Permission denied",
  VersionMismatch: "Version mismatch",
  TransientError: "Error",
};

export function detectorText(health: DetectorHealth): string {
  if (typeof health === "string") {
    return health === "Active" ? "active" : "inactive (no interface configured)";
  }
  return `error: ${health.Error}`;
}

export function routingText(state: RoutingState): string {
  switch (state) {
    case "Disabled":
      return "disabled";
    case "NoDomains":
      return "no domains configured";
    case "VpnDown":
      return "VPN down";
    case "NoDnsFromVpn":
      return "VPN up, but it pushes no DNS";
    case "Applied":
      return "applied";
    case "ApplyFailed":
      return "apply failed (out of sync)";
    case "ConfigInvalid":
      return "config file invalid (using last-good)";
  }
}

export function driftText(drift: DriftVerdict): string {
  if (drift === "NotApplicable") return "not applicable (nothing applied)";
  if (drift === "InSync") return "in sync";
  const { missing_servers, unrouted_domains } = drift.Drifted;
  const parts: string[] = [];
  if (missing_servers.length) parts.push(`missing servers: ${missing_servers.join(", ")}`);
  if (unrouted_domains.length) parts.push(`unrouted domains: ${unrouted_domains.join(", ")}`);
  return `drifted — ${parts.join("; ")}`;
}

/** The connection banner: health dot + label, plus the daemon/client guidance
 *  verbatim (permission-denied note, version-skew "update" message, etc.). */
export function connectionHeader(vm: ViewModel): HTMLElement {
  const banner = el("div", { class: `banner health-${vm.connection.health}` }, [
    el("span", { class: "dot", text: "●" }),
    el("span", { text: HEALTH_LABEL[vm.connection.health] }),
  ]);
  const nodes = [banner];
  if (vm.connection.message) {
    nodes.push(el("p", { class: "banner-message", text: vm.connection.message }));
  }
  return el("header", {}, nodes);
}

/** The read-only status rows (no toggle — that is an interactive control). Returns
 *  the labelled rows, or a single muted note when no trustworthy status is held
 *  (dropped on permission-denied / version-mismatch too, where the banner explains
 *  the reason). Surfaces the daemon's belief: routing state + the applied mapping. */
export function statusRows(status: StatusInfo | null): Node[] {
  if (!status) {
    return [el("p", { class: "muted", text: "Live status unavailable — see the banner above." })];
  }
  const applied = status.applied
    ? `${status.applied.interface} → [${status.applied.domains.join(", ")}] via [${status.applied.dns_servers.join(", ")}]`
    : "(nothing applied)";
  return [
    row("enabled", String(status.enabled)),
    row("interface", status.interface || "(unset)"),
    row("vpn up", String(status.vpn_up)),
    row("routing", routingText(status.routing_state)),
    row("applied", applied),
    row("detector", detectorText(status.detector_health)),
  ];
}

export function verifySection(verify: VerifyView): HTMLElement {
  switch (verify.state) {
    case "Unknown":
      return section("Live DNS (verify)", [el("p", { class: "muted", text: "not checked yet" })]);
    case "Unavailable":
      return section("Live DNS (verify)", [el("p", { class: "muted", text: verify.message })]);
    case "Available": {
      const { live, drift } = verify;
      return section("Live DNS (verify)", [
        row("link servers", live.servers.length ? live.servers.join(", ") : "(none)"),
        row(
          "link routing domains",
          live.routing_domains.length ? live.routing_domains.join(", ") : "(none)",
        ),
        row("drift", driftText(drift)),
      ]);
    }
  }
}

export function interfacesSection(vm: ViewModel): HTMLElement {
  if (vm.interfaces.length === 0) {
    return section("Interfaces", [el("p", { class: "muted", text: "(none enumerated)" })]);
  }
  const list = el(
    "ul",
    { class: "interfaces" },
    vm.interfaces.map((iface) => {
      const state = iface.up ? "up" : "down";
      const vpn = iface.vpn_like ? ", vpn" : "";
      return el("li", { text: `${iface.name} (${state}${vpn})` });
    }),
  );
  return section("Interfaces", [list]);
}

export function configFileSection(vm: ViewModel): HTMLElement {
  return section("Config file", [row("active", vm.config_path || "(unknown)")]);
}
