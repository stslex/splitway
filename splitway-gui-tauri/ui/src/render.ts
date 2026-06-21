// The frontend's entire job: ViewModel -> DOM. Read-only, deterministic, no
// local state. `render` clears the root and rebuilds it from the given VM each
// call; the caller invokes it once per pushed VM (last-wins). DOM is built with
// createElement + textContent (never innerHTML with interpolated daemon strings),
// so domain names / error messages from the daemon can never inject markup.

import type {
  ConfigFields,
  DetectorHealth,
  DriftVerdict,
  Health,
  StatusInfo,
  VerifyView,
  ViewModel,
} from "./bindings/view-model";

/** Tiny element helper: tag + optional class + text/children. */
function el(
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
function row(label: string, value: string): HTMLElement {
  return el("div", { class: "row" }, [
    el("span", { class: "label", text: label }),
    el("span", { class: "value", text: value }),
  ]);
}

function section(title: string, body: Node[]): HTMLElement {
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

function detectorText(health: DetectorHealth): string {
  if (typeof health === "string") {
    return health === "Active" ? "active" : "inactive (no interface configured)";
  }
  return `error: ${health.Error}`;
}

function driftText(drift: DriftVerdict): string {
  if (drift === "NotApplicable") return "not applicable (nothing applied)";
  if (drift === "InSync") return "in sync";
  const { missing_servers, unrouted_domains } = drift.Drifted;
  const parts: string[] = [];
  if (missing_servers.length) parts.push(`missing servers: ${missing_servers.join(", ")}`);
  if (unrouted_domains.length) parts.push(`unrouted domains: ${unrouted_domains.join(", ")}`);
  return `drifted — ${parts.join("; ")}`;
}

function renderConnection(vm: ViewModel): HTMLElement {
  const banner = el("div", { class: `banner health-${vm.connection.health}` }, [
    el("span", { class: "dot", text: "●" }),
    el("span", { text: HEALTH_LABEL[vm.connection.health] }),
  ]);
  const nodes = [banner];
  if (vm.connection.message) {
    // The daemon/client guidance verbatim (e.g. the permission-denied "not in
    // the daemon's group / try sudo" note, or the version-skew "update" message).
    nodes.push(el("p", { class: "banner-message", text: vm.connection.message }));
  }
  return el("header", {}, nodes);
}

function renderStatus(status: StatusInfo | null): HTMLElement {
  if (!status) {
    // Status is dropped on permission-denied / version-mismatch too, where the
    // daemon IS reachable — the banner above already states the precise reason.
    return section("Status", [
      el("p", { class: "muted", text: "Live status unavailable — see the banner above." }),
    ]);
  }
  const applied = status.applied
    ? `${status.applied.interface} → [${status.applied.domains.join(", ")}] via [${status.applied.dns_servers.join(", ")}]`
    : "(nothing applied)";
  return section("Status", [
    row("enabled", String(status.enabled)),
    row("interface", status.interface || "(unset)"),
    row("vpn up", String(status.vpn_up)),
    row("routing", routingText(status.routing_state)),
    row("applied", applied),
    row("detector", detectorText(status.detector_health)),
  ]);
}

function routingText(state: StatusInfo["routing_state"]): string {
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

function renderDomains(status: StatusInfo | null): HTMLElement {
  const domains = status?.domains ?? [];
  if (domains.length === 0) {
    return section("Routed domains", [el("p", { class: "muted", text: "(no domains configured)" })]);
  }
  const list = el(
    "ul",
    { class: "domains" },
    domains.map((d) => el("li", { text: d })),
  );
  return section("Routed domains", [list]);
}

function renderVerify(verify: VerifyView): HTMLElement {
  switch (verify.state) {
    case "Unknown":
      return section("Live DNS (verify)", [
        el("p", { class: "muted", text: "not checked yet" }),
      ]);
    case "Unavailable":
      return section("Live DNS (verify)", [
        el("p", { class: "muted", text: verify.message }),
      ]);
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

function renderConfig(config: ConfigFields | null, configLoaded: boolean): HTMLElement {
  if (!configLoaded || !config) {
    return section("Configuration", [el("p", { class: "muted", text: "loading config…" })]);
  }
  const rows = [
    row("interface (vpn_name)", config.vpn_name || "(none)"),
    row("backend", config.vpn_backend),
  ];
  if (config.vpn_backend === "openvpn") {
    rows.push(row("openvpn management", config.openvpn_management || "(unset)"));
    rows.push(
      row("openvpn password file", config.openvpn_management_password_file ?? "(none)"),
    );
  }
  return section("Configuration", rows);
}

function renderInterfaces(vm: ViewModel): HTMLElement {
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

function renderMessage(vm: ViewModel): HTMLElement | null {
  if (!vm.message) return null;
  return el("div", { class: `message message-${vm.message.kind}`, text: vm.message.text });
}

/** Render the whole view-model into `root`, replacing its contents. */
export function render(vm: ViewModel, root: HTMLElement): void {
  const children: Node[] = [
    renderConnection(vm),
    renderStatus(vm.status),
    renderDomains(vm.status),
    renderVerify(vm.verify),
    renderConfig(vm.config, vm.config_loaded),
    renderInterfaces(vm),
    section("Config file", [row("active", vm.config_path || "(unknown)")]),
  ];
  if (vm.working) {
    children.unshift(el("div", { class: "working", text: "working…" }));
  }
  const message = renderMessage(vm);
  if (message) children.push(message);

  root.replaceChildren(...children);
}
