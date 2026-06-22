// The application: it owns the cached view-model and the request-lifecycle store,
// subscribes to view-model-changed, renders the Variant B design, and wires the
// controls to the Tauri commands.
//
// THE TRUTH CONTRACT, made structural here:
//   - `lastVm` is the ONLY authoritative state, and is assigned ONLY in `applyVm`
//     (the VM-event / initial-fetch path). Search the file: nothing else writes it.
//   - Every mutation handler goes daemon-first: set pending → await the command →
//     clear pending (+ per-action error on failure). It NEVER edits `lastVm`. The
//     daemon's resulting truth arrives via the next view-model-changed event (the
//     backend fires refresh-now after each mutation), and `applyVm` re-renders.
//   - The only other state is the lifecycle store (pending / per-action error /
//     add-row input / ephemeral CheckDomain result / ephemeral undo snackbar) —
//     see lifecycle.ts. It describes the in-flight interaction, not daemon truth.
//   - DOM is built with createElement / createElementNS + textContent (helpers in
//     render.ts) — never innerHTML on daemon strings — so daemon data can't inject
//     markup. (`grep -rn innerHTML ui/src` returns nothing: keep it so.)
//
// The Variant B simplification: two inputs only — interface + domains. There is
// no backend field and no settings screen; the interface selector is the GUI's
// ONLY config writer, and it round-trips the hidden vpn_backend / openvpn_* fields
// unchanged via `configInputForInterface` (see config-input.ts) so an OpenVPN
// user's daemon-side config is never reset.

import {
  brandMark,
  blockerIcon,
  chipFor,
  connIndicator,
  domainDrifted,
  driftOf,
  el,
  stageFor,
  statusLineNodes,
  type BlockerVariant,
  type BlockerView,
  type ConnIndicator,
  type MainMode,
  type Stage,
} from "./render";
import {
  beginAction,
  endAction,
  errorFor,
  isPending,
  newLifecycle,
  type ActionKey,
  type CheckState,
  type Lifecycle,
} from "./lifecycle";
import { configInputForInterface } from "./config-input";
import * as api from "./api";
import type { DomainCheckInfo, InterfaceInfo, StatusInfo, ViewModel } from "./bindings/view-model";

/** How long the undo snackbar lingers before the delete is left committed. The
 *  delete already happened daemon-side; this is only the window to re-add. */
const UNDO_MS = 11000;

const PITCH =
  "Send specific domains through the VPN. Everything else stays on your normal connection.";
const CHECK_DESC =
  "Test whether a host would route through the VPN or go direct. This only checks — it changes nothing.";
const DNS_FOOTNOTE =
  "Checks DNS only — whether the name resolves through the VPN's resolver, not whether the address is reachable through the tunnel.";

/** Callbacks the rendered controls invoke. An interface so the section builders
 *  stay pure `(vm, lc, actions) -> nodes`, decoupled from the controller. */
export interface Actions {
  toggle(enable: boolean): void;
  setInterface(name: string): void;
  openAdd(): void;
  cancelAdd(): void;
  setAddInput(value: string): void;
  submitAdd(): void;
  removeDomain(domain: string): void;
  undoRemove(): void;
  dismissUndo(): void;
  setCheckInput(value: string): void;
  check(): void;
  resync(): void;
}

// --- small builders ---------------------------------------------------------

function actionError(text: string): HTMLElement {
  return el("p", { class: "action-error", text });
}

// --- topbar -----------------------------------------------------------------

function blockerConn(variant: BlockerVariant): ConnIndicator {
  switch (variant) {
    case "disconnected":
    case "error":
      return { text: "Disconnected", level: "off" };
    case "no-permission":
      return { text: "No access", level: "warn" };
    case "frozen":
      return { text: "Config error", level: "warn" };
    case "version":
      return { text: "Update needed", level: "warn" };
  }
}

function connFor(stage: Stage): ConnIndicator {
  switch (stage.kind) {
    case "connecting":
      return { text: "Connecting…", level: "off" };
    case "blocker":
      return blockerConn(stage.blocker.variant);
    case "main":
      return connIndicator(stage.mode);
  }
}

function topbar(stage: Stage): HTMLElement {
  const conn = connFor(stage);
  const connEl = el("span", { class: "conn" }, [
    el("span", { class: `dot ${conn.level}` }),
    el("span", { text: conn.text }),
  ]);
  return el("div", { class: "topbar reveal d1" }, [
    brandMark(),
    // The app's main heading (h1) so the blocker h2s sit under a level-1 heading
    // rather than starting heading order at h3.
    el("h1", { class: "wordmark", text: "Splitway" }),
    el("span", { class: "grow" }),
    connEl,
  ]);
}

// --- hero -------------------------------------------------------------------

function switchControl(status: StatusInfo, lc: Lifecycle, actions: Actions): HTMLButtonElement {
  const enabled = status.enabled;
  const pending = isPending(lc, "toggle");
  const btn = el("button", { class: "switch" }) as HTMLButtonElement;
  btn.id = "sw";
  btn.type = "button";
  btn.setAttribute("role", "switch");
  btn.setAttribute("aria-checked", String(enabled));
  btn.setAttribute("aria-label", "Routing");
  btn.disabled = pending;
  btn.append(
    el("span", { class: "lbl on", text: "ON" }),
    el("span", { class: "lbl off", text: "OFF" }),
    el("span", { class: "thumb" }),
  );
  btn.addEventListener("click", () => actions.toggle(!enabled));
  return btn;
}

function hero(
  status: StatusInfo,
  lc: Lifecycle,
  actions: Actions,
  mode: MainMode,
  vm: ViewModel,
): HTMLElement {
  const top = el("div", { class: "hero-top" }, [
    el("span", { class: "pitch", text: PITCH }),
    switchControl(status, lc, actions),
  ]);

  const line = el("p", { class: "status-line" }, statusLineNodes(mode, status));
  line.id = "statusLine";
  const statusBox = el("div", { class: "status" }, [line]);

  const chip = chipFor(mode, vm.verify);
  if (chip) {
    const cls = chip.tone === "ok" ? "chip" : `chip ${chip.tone}`;
    const chipEl = el("span", { class: cls });
    if (chip.check) chipEl.appendChild(el("span", { class: "ic", text: "✓" }));
    chipEl.appendChild(document.createTextNode(chip.text));
    statusBox.appendChild(chipEl);
  }

  // Apply-failed is the one mode with a recovery action the mockup's happy path
  // doesn't carry: offer a Resync so the user can re-drive reconciliation.
  if (mode === "apply-failed") {
    const resync = el("button", { class: "add" }) as HTMLButtonElement;
    resync.type = "button";
    const pending = isPending(lc, "reload");
    resync.textContent = pending ? "Resyncing…" : "Resync";
    resync.disabled = pending;
    resync.addEventListener("click", () => actions.resync());
    statusBox.appendChild(resync);
  }

  const err = errorFor(lc, "toggle") ?? errorFor(lc, "reload");
  if (err) statusBox.appendChild(actionError(err));

  return el("section", { class: "hero reveal d2" }, [top, statusBox]);
}

// --- interface block --------------------------------------------------------

function interfaceLabel(iface: InterfaceInfo): string {
  // Keep the option text close to the mockup's clean bare-name look; the up/down
  // signal is carried by the status line + chip, so options stay uncluttered.
  return iface.name;
}

/** Picker entries: the enumerated interfaces, plus the configured interface when
 *  it is not among them (a VPN that is down right now), so the user's choice is
 *  never dropped from the list. Mirrors gui-core's `interface_choices`. */
function interfaceChoices(
  interfaces: InterfaceInfo[],
  configured: string,
): { name: string; label: string }[] {
  const choices = interfaces.map((iface) => ({ name: iface.name, label: interfaceLabel(iface) }));
  const c = configured.trim();
  if (c !== "" && !interfaces.some((iface) => iface.name === c)) {
    const label = interfaces.length === 0 ? `${c} (configured)` : `${c} (not connected)`;
    choices.push({ name: c, label });
  }
  return choices;
}

function interfaceSelect(vm: ViewModel, lc: Lifecycle, actions: Actions): HTMLSelectElement {
  const sel = el("select", { class: "sel" }) as HTMLSelectElement;
  sel.id = "iface";
  sel.setAttribute("aria-label", "Network interface");
  const configured = vm.status?.interface ?? "";
  for (const choice of interfaceChoices(vm.interfaces, configured)) {
    const opt = el("option", { text: choice.label }) as HTMLOptionElement;
    opt.value = choice.name;
    if (choice.name === configured) opt.selected = true;
    sel.appendChild(opt);
  }
  if (configured === "") {
    // No interface configured yet: a leading placeholder so nothing reads as the
    // current selection.
    const opt = el("option", { text: "Select an interface…" }) as HTMLOptionElement;
    opt.value = "";
    opt.selected = true;
    opt.disabled = true;
    sel.insertBefore(opt, sel.firstChild);
  }
  // Disabled while the config has not loaded (we cannot safely round-trip the
  // hidden fields yet) or while a previous interface write is in flight.
  sel.disabled = !vm.config_loaded || isPending(lc, "iface");
  sel.addEventListener("change", () => actions.setInterface(sel.value));
  return sel;
}

function dnsReadout(vm: ViewModel, mode: MainMode): HTMLElement {
  const status = vm.status as StatusInfo;
  const iface = status.interface || "this interface";
  const detected = status.detected_dns;

  if (detected.length > 0) {
    const ok = el("div", { class: "dns-ok" });
    ok.append(
      document.createTextNode("Using "),
      el("span", { class: "ip", text: detected.join(", ") }),
      document.createTextNode(" · detected from "),
      el("span", { text: iface }),
    );
    return ok;
  }

  if (mode === "dns-missing") {
    // DNS-not-detected fix — informational ONLY. The daemon auto-derives DNS from
    // the interface and has no manual-DNS override (a real future daemon feature,
    // not presentation), so a manual-entry box would be a dead/fake control and
    // would violate the truth contract. Point at the real fixes instead.
    const miss = el("div", { class: "dns-miss" });
    const warn = el("div", { class: "warn" }, [
      el("span", { class: "ic", text: "⚠" }),
      document.createTextNode(
        `No DNS detected on ${iface}. This interface isn't providing a DNS server — ` +
          `pick the VPN's interface above, or check that the VPN is configured to push DNS.`,
      ),
    ]);
    miss.appendChild(warn);
    return miss;
  }

  // off / waiting / empty without detected DNS: a faint neutral note.
  const note =
    mode === "waiting"
      ? `Waiting for ${iface} to come up.`
      : `No DNS detected for ${iface} yet.`;
  return el("div", { class: "dns-ok", text: note });
}

function interfaceBlock(vm: ViewModel, lc: Lifecycle, actions: Actions, mode: MainMode): HTMLElement {
  const label = el("label", { text: "Route DNS through" });
  label.setAttribute("for", "iface");
  const head = el("div", { class: "head" }, [label, interfaceSelect(vm, lc, actions)]);
  const dns = el("div", { class: "dns" }, [dnsReadout(vm, mode)]);
  const block = el("div", { class: "iface-block panel reveal d3" }, [head, dns]);
  const err = errorFor(lc, "iface");
  if (err) block.appendChild(actionError(err));
  return block;
}

// --- domains ----------------------------------------------------------------

function addRow(lc: Lifecycle, actions: Actions): HTMLElement {
  const pending = isPending(lc, "add");
  const input = el("input", { class: "field" }) as HTMLInputElement;
  input.id = "add-domain-input";
  input.type = "text";
  input.value = lc.addInput;
  input.placeholder = "corp.example.com";
  input.autocomplete = "off";
  input.setAttribute("aria-label", "Domain to add");
  input.disabled = pending;
  input.addEventListener("input", () => actions.setAddInput(input.value));
  input.addEventListener("keydown", (event) => {
    if (event.key === "Enter") actions.submitAdd();
    else if (event.key === "Escape") actions.cancelAdd();
  });
  const add = el("button", { class: "btn", text: pending ? "Adding…" : "Add" }) as HTMLButtonElement;
  add.type = "button";
  add.disabled = pending || lc.addInput.trim() === "";
  add.addEventListener("click", () => actions.submitAdd());
  return el("div", { class: "add-row" }, [input, add]);
}

function domainCard(
  domain: string,
  iface: string,
  lc: Lifecycle,
  actions: Actions,
  showVerify: boolean,
  drifted: boolean,
): HTMLElement {
  const card = el("div", { class: drifted ? "card is-drift" : "card" });
  const row = el("div", { class: "card-row" }, [
    el("span", { class: "name", text: domain }),
    el("span", { class: "route" }, [
      el("span", { class: "arrow", text: "→" }),
      document.createTextNode(" "),
      el("span", { class: "ifn", text: iface || "direct" }),
    ]),
  ]);
  if (showVerify) {
    row.appendChild(
      el("span", { class: drifted ? "vstate drift" : "vstate ok", text: drifted ? "⚠" : "✓" }),
    );
  }
  const del = el("button", { class: "del", text: "✕" }) as HTMLButtonElement;
  del.type = "button";
  del.setAttribute("aria-label", `Remove ${domain}`);
  del.disabled = isPending(lc, `remove:${domain}`);
  del.addEventListener("click", () => actions.removeDomain(domain));
  row.appendChild(del);
  card.appendChild(row);

  if (drifted) {
    const sub = el("div", { class: "sub warn" });
    sub.append(
      document.createTextNode("expected "),
      el("span", { class: "ifn", text: iface }),
      document.createTextNode(" — system currently resolves it direct"),
    );
    card.appendChild(sub);
  }
  const err = errorFor(lc, `remove:${domain}`);
  if (err) card.appendChild(actionError(err));
  return card;
}

function emptyPlaceholder(): HTMLElement {
  return el("div", { class: "empty" }, [
    el("div", { class: "t", text: "No domains yet" }),
    el("div", {
      class: "s",
      text: "Add a domain to start routing it through the VPN. Everything stays direct until you do.",
    }),
  ]);
}

function everythingElse(): HTMLElement {
  return el("div", { class: "everything" }, [
    el("span", { class: "rest", text: "Everything else" }),
    el("span", { class: "direct" }, [
      el("span", { class: "arrow", text: "→" }),
      document.createTextNode(" direct"),
    ]),
  ]);
}

function domainsSection(
  vm: ViewModel,
  lc: Lifecycle,
  actions: Actions,
  mode: MainMode,
): HTMLElement {
  const status = vm.status as StatusInfo;
  const domains = status.domains;

  const count = el("span", { class: "count", text: String(domains.length) });
  count.id = "count";
  const eyebrowLabel = el("span", { class: "label" }, [document.createTextNode("Domains "), count]);

  const add = el("button", { class: "add" }) as HTMLButtonElement;
  add.type = "button";
  add.disabled = isPending(lc, "add");
  add.append(el("span", { class: "plus", text: "+" }), document.createTextNode(" Add domain"));
  add.addEventListener("click", () => (lc.addOpen ? actions.cancelAdd() : actions.openAdd()));

  const children: Node[] = [el("div", { class: "eyebrow" }, [eyebrowLabel, add])];
  if (lc.addOpen) children.push(addRow(lc, actions));
  const addErr = errorFor(lc, "add");
  if (addErr) children.push(actionError(addErr));

  if (mode === "empty") {
    children.push(emptyPlaceholder());
  } else {
    const list = el("div", { class: "domains" });
    list.id = "domains";
    // Per-domain verify (✓ / ⚠) is only meaningful when rules are actually applied
    // AND a live read-back exists; otherwise show no badge rather than a fake ✓.
    const drift = mode === "healthy" ? driftOf(vm) : null;
    const showVerify = mode === "healthy" && drift !== null;
    for (const domain of domains) {
      list.appendChild(
        domainCard(domain, status.interface, lc, actions, showVerify, domainDrifted(drift, domain)),
      );
    }
    children.push(list);
  }
  children.push(everythingElse());
  return el("section", { class: "section reveal d4" }, children);
}

// --- check a domain ---------------------------------------------------------

function checkRoutingText(routingState: string, covered: boolean): string {
  if (!covered) return "not configured to route through the VPN";
  switch (routingState) {
    case "Applied":
      return "routed through the VPN's DNS";
    case "Disabled":
      return "configured to route, but routing is off right now";
    case "VpnDown":
      return "configured to route, but the VPN is down right now";
    case "NoDnsFromVpn":
      return "configured to route, but the VPN pushes no DNS right now";
    case "ApplyFailed":
      return "configured to route, but the rules are out of sync right now";
    case "ConfigInvalid":
      return "the config file is invalid; routing reflects the last-good config";
    default:
      return "configured to route";
  }
}

function checkVerdictNodes(info: DomainCheckInfo): Node[] {
  const live = info.resolution;
  // Live attribution is authoritative over belief: if the daemon believes the
  // host is routed but the name resolved via a non-VPN link, say the live result
  // disagrees and is the one to trust (mirrors the CLI's drift line).
  const answeredElsewhere =
    live?.via_interface != null &&
    info.vpn_interface !== "" &&
    live.via_interface !== info.vpn_interface;

  const nodes: Node[] = [];
  if (info.covered) {
    nodes.push(document.createTextNode("Matches "));
    nodes.push(el("code", { text: info.matched_domain ?? info.host }));
    nodes.push(
      document.createTextNode(
        " — " +
          (answeredElsewhere && info.routing_state === "Applied"
            ? `the daemon believes it routes, but it resolved via ${live!.via_interface}, not the VPN — trust the live result`
            : checkRoutingText(info.routing_state, true)),
      ),
    );
  } else {
    nodes.push(document.createTextNode("No matching rule — "));
    nodes.push(el("code", { text: info.host }));
    nodes.push(document.createTextNode(" goes direct"));
  }

  if (live && live.addresses.length > 0) {
    const viaNote =
      live.via_interface != null && info.vpn_interface !== ""
        ? live.via_interface === info.vpn_interface
          ? ` via ${live.via_interface} (the VPN's link)`
          : ` via ${live.via_interface} (not the VPN link)`
        : live.via_interface != null
          ? ` via ${live.via_interface}`
          : "";
    nodes.push(el("span", { class: "footnote", text: `Resolved to ${live.addresses.join(", ")}${viaNote}.` }));
  }
  nodes.push(el("span", { class: "footnote", text: DNS_FOOTNOTE }));
  return nodes;
}

function checkResult(check: CheckState): HTMLElement | null {
  if (check === "idle" || check === "pending") return null;
  if (check.state === "Error") {
    return el("div", { class: "result bad" }, [
      el("span", { class: "pin", text: "→" }),
      el("span", { text: check.message }),
    ]);
  }
  const info = check.result;
  const cls = info.covered ? "result" : "result miss";
  return el("div", { class: cls }, [
    el("span", { class: "pin", text: "→" }),
    el("span", {}, checkVerdictNodes(info)),
  ]);
}

function checkSection(lc: Lifecycle, actions: Actions): HTMLElement {
  const pending = lc.check === "pending";
  const input = el("input", { class: "field" }) as HTMLInputElement;
  input.id = "check-input";
  input.type = "text";
  input.value = lc.checkInput;
  input.placeholder = "host.example.com";
  input.autocomplete = "off";
  input.setAttribute("aria-label", "Domain to check");
  input.disabled = pending;
  input.addEventListener("input", () => actions.setCheckInput(input.value));
  input.addEventListener("keydown", (event) => {
    if (event.key === "Enter") actions.check();
  });

  const btn = el("button", { class: "btn" }) as HTMLButtonElement;
  btn.type = "button";
  if (pending) {
    btn.append(el("span", { class: "spinner" }), document.createTextNode("Checking"));
  } else {
    btn.textContent = "Check";
  }
  btn.disabled = pending || lc.checkInput.trim() === "";
  btn.addEventListener("click", () => actions.check());

  const children: Node[] = [
    el("div", { class: "eyebrow" }, [el("span", { class: "label", text: "Check a domain" })]),
    el("p", { class: "desc", text: CHECK_DESC }),
    el("div", { class: "check-row" }, [input, btn]),
  ];
  const result = checkResult(lc.check);
  if (result) children.push(result);
  return el("section", { class: "section reveal d5" }, children);
}

// --- footer, message, toast, blockers --------------------------------------

function footer(vm: ViewModel): HTMLElement {
  return el("div", { class: "foot reveal d5", text: `config ${vm.config_path || "(unknown)"}` });
}

function messageBanner(vm: ViewModel): HTMLElement | null {
  if (!vm.message) return null;
  return el("div", { class: `message message-${vm.message.kind}`, text: vm.message.text });
}

function undoToast(lc: Lifecycle, actions: Actions): HTMLElement {
  const toast = el("div", { class: lc.undo ? "toast show" : "toast" });
  toast.id = "toast";
  const msg = el("span", { class: "msg" });
  toast.appendChild(msg);
  // The buttons exist ONLY while the toast is shown. When hidden, the toast is
  // opacity:0 + pointer-events:none, but those don't remove an element from the
  // tab order — leaving the buttons present would be focusable-but-invisible (a
  // keyboard tab trap). Conditional render keeps them out of the tab order when
  // hidden (the full-rebuild model already precludes a slide-out animation, so
  // nothing is lost by not keeping them in the DOM).
  if (!lc.undo) return toast;

  const domain = lc.undo.domain;
  // The undo re-add runs under the `remove:<domain>` key. Its outcome is surfaced
  // HERE, in the toast, because on the undo path the domain is absent from the VM
  // (the delete already landed) so there is no domain card to render its
  // per-action error on — without this the re-add failure would be silent.
  const key: ActionKey = `remove:${domain}`;
  const dismiss = (): HTMLButtonElement => {
    const x = el("button", { class: "x", text: "✕" }) as HTMLButtonElement;
    x.type = "button";
    x.setAttribute("aria-label", "Dismiss");
    x.addEventListener("click", () => actions.dismissUndo());
    return x;
  };

  if (isPending(lc, key)) {
    msg.append(
      el("span", { class: "spinner" }),
      document.createTextNode(" Restoring "),
      el("code", { text: domain }),
    );
    return toast; // no buttons while the re-add is in flight
  }

  const restoreError = errorFor(lc, key);
  if (restoreError) {
    msg.append(
      document.createTextNode("Couldn't restore "),
      el("code", { text: domain }),
      document.createTextNode(` — ${restoreError}`),
    );
    const retry = el("button", { class: "undo", text: "Retry" }) as HTMLButtonElement;
    retry.type = "button";
    retry.addEventListener("click", () => actions.undoRemove());
    toast.append(retry, dismiss());
    return toast;
  }

  msg.append(document.createTextNode("Removed "), el("code", { text: domain }));
  const undo = el("button", { class: "undo", text: "Undo" }) as HTMLButtonElement;
  undo.type = "button";
  undo.addEventListener("click", () => actions.undoRemove());
  toast.append(undo, dismiss());
  return toast;
}

function blockerNode(blocker: BlockerView): HTMLElement {
  const icon = el("div", { class: "ic" }, [blockerIcon(blocker.variant)]);
  const body = el("p");
  if (blocker.codeWord && blocker.body.includes("{}")) {
    const [before, after] = blocker.body.split("{}");
    body.append(
      document.createTextNode(before),
      el("span", { class: "inline-code", text: blocker.codeWord }),
      document.createTextNode(after),
    );
  } else {
    body.textContent = blocker.body;
  }
  const blk = el("div", { class: `blk ${blocker.tone}` }, [
    icon,
    el("h2", { text: blocker.title }),
    body,
  ]);
  if (blocker.command) blk.appendChild(el("code", { class: "cmd", text: blocker.command }));
  return el("div", { class: "blocker" }, [blk]);
}

function connectingNode(): HTMLElement {
  return el("div", { class: "blocker" }, [
    el("div", { class: "blk neutral" }, [el("p", { class: "muted", text: "Connecting…" })]),
  ]);
}

/** Pure builder: the whole UI from (vm, lifecycle, actions). `vm === null` only
 *  before the first event lands. */
export function renderApp(vm: ViewModel | null, lc: Lifecycle, actions: Actions): Node[] {
  const stage: Stage = vm ? stageFor(vm) : { kind: "connecting" };
  const children: Node[] = [topbar(stage)];

  if (stage.kind === "connecting") {
    children.push(connectingNode());
    return children;
  }
  if (stage.kind === "blocker") {
    children.push(blockerNode(stage.blocker));
    return children;
  }

  // Main stage — vm is non-null here.
  const live = vm as ViewModel;
  children.push(
    hero(stage.status, lc, actions, stage.mode, live),
    interfaceBlock(live, lc, actions, stage.mode),
    domainsSection(live, lc, actions, stage.mode),
    checkSection(lc, actions),
  );
  const message = messageBanner(live);
  if (message) children.push(message);
  children.push(footer(live), undoToast(lc, actions));
  return children;
}

/** The data-on / data-mode the CSS keys off, derived from a stage. */
function rootDataset(stage: Stage): { on: string; mode: string } {
  if (stage.kind === "main") return { on: String(stage.status.enabled), mode: stage.mode };
  return { on: "false", mode: stage.kind === "blocker" ? stage.blocker.variant : "connecting" };
}

// --- controller -------------------------------------------------------------

/** Boot the app into `root`: subscribe, fetch once, render, and wire controls. */
export function start(root: HTMLElement): void {
  let lastVm: ViewModel | null = null; // the ONLY authoritative state; written only in applyVm
  const lc = newLifecycle();
  let undoTimer: ReturnType<typeof setTimeout> | null = null;
  let introStarted = false; // the staggered reveal plays once, on the first main paint

  function clearUndoTimer(): void {
    if (undoTimer !== null) {
      clearTimeout(undoTimer);
      undoTimer = null;
    }
  }

  function rerender(): void {
    // Preserve focus + caret across the full rebuild (text inputs survive a
    // background VM event mid-typing this way).
    const active = document.activeElement;
    const activeId = active instanceof HTMLElement ? active.id : "";
    const selStart = active instanceof HTMLInputElement ? active.selectionStart : null;
    const selEnd = active instanceof HTMLInputElement ? active.selectionEnd : null;

    const stage: Stage = lastVm ? stageFor(lastVm) : { kind: "connecting" };
    const ds = rootDataset(stage);
    root.dataset.on = ds.on;
    root.dataset.mode = ds.mode;
    // One-time entrance reveal: add `.intro` on the first main paint so the
    // staggered animation plays once, then drop it so later rebuilds (every VM
    // event / keystroke) don't replay it. The class lives on the persistent #app
    // root, not the rebuilt children. See styles.css `.app.intro .reveal`.
    if (stage.kind === "main" && !introStarted) {
      introStarted = true;
      root.classList.add("intro");
      setTimeout(() => root.classList.remove("intro"), 900);
    }
    root.replaceChildren(...renderApp(lastVm, lc, actions));

    if (activeId) {
      const restored = document.getElementById(activeId);
      if (restored) {
        restored.focus();
        if (restored instanceof HTMLInputElement && selStart !== null) {
          restored.setSelectionRange(selStart, selEnd ?? selStart);
        }
      }
    }
  }

  // The ONLY writer of lastVm.
  function applyVm(vm: ViewModel): void {
    lastVm = vm;
    rerender();
  }

  // Daemon-first mutation driver: pending → await → resolve. Never touches lastVm.
  async function runMutation(key: ActionKey, op: () => Promise<void>): Promise<void> {
    if (isPending(lc, key)) return; // double-submit guard
    beginAction(lc, key);
    rerender();
    try {
      await op();
      endAction(lc, key, null);
    } catch (err) {
      endAction(lc, key, String(err));
    }
    rerender();
  }

  // Update a persistent visually-hidden polite live region (in index.html, a
  // SIBLING of #app so the full-rebuild replaceChildren never wipes it). The
  // rebuild model precludes putting aria-live on a rebuilt node (it would
  // re-announce on every render), so dynamic changes (a removed domain, a check
  // result) are announced by setting this region's text explicitly.
  function announce(message: string): void {
    const live = document.getElementById("a11y-live");
    if (live) live.textContent = message;
  }

  function showUndo(domain: string): void {
    clearUndoTimer();
    lc.undo = { domain };
    announce(`Removed ${domain}. Press Undo to restore it.`);
    undoTimer = setTimeout(() => {
      undoTimer = null;
      lc.undo = null;
      rerender();
    }, UNDO_MS);
  }

  const actions: Actions = {
    toggle: (enable) => void runMutation("toggle", () => api.setEnabled(enable)),

    // The interface selector is the ONLY config writer. It round-trips the hidden
    // vpn_backend / openvpn_* fields unchanged (configInputForInterface), so an
    // OpenVPN user's daemon config is never reset by a GUI interface switch.
    setInterface: (name) => {
      const config = lastVm?.config;
      if (!config || name === "" || name === lastVm?.status?.interface) return;
      const sent = configInputForInterface(config, name);
      void runMutation("iface", () => api.setConfig(sent));
    },

    openAdd: () => {
      lc.addOpen = true;
      lc.errors.delete("add");
      rerender();
      document.getElementById("add-domain-input")?.focus();
    },
    cancelAdd: () => {
      lc.addOpen = false;
      lc.addInput = "";
      lc.errors.delete("add");
      rerender();
    },
    setAddInput: (value) => {
      lc.addInput = value;
      rerender();
    },
    submitAdd: () => {
      const domain = lc.addInput.trim();
      if (domain === "") return; // input hygiene; the daemon validates the rest
      void runMutation("add", async () => {
        await api.addDomain(domain);
        lc.addInput = ""; // accepted → clear + close (the VM event re-renders the list)
        lc.addOpen = false;
      });
    },

    removeDomain: (domain) =>
      void runMutation(`remove:${domain}`, async () => {
        await api.removeDomain(domain);
        showUndo(domain); // ephemeral; the delete already landed daemon-side
      }),
    undoRemove: () => {
      const domain = lc.undo?.domain;
      if (!domain) return;
      clearUndoTimer(); // stop the auto-commit; keep the toast up through the re-add
      // Keep `lc.undo` set so the toast stays visible and can surface a re-add
      // failure (the domain has no card to render its per-action error on). It is
      // cleared only when the re-add succeeds; the VM re-poll then restores the row.
      void runMutation(`remove:${domain}`, async () => {
        await api.addDomain(domain);
        lc.undo = null;
        announce(`Restored ${domain}.`);
      });
    },
    dismissUndo: () => {
      const domain = lc.undo?.domain;
      clearUndoTimer();
      lc.undo = null;
      // Drop any failed-restore error so a later delete of the same domain starts clean.
      if (domain) lc.errors.delete(`remove:${domain}`);
      rerender();
    },

    setCheckInput: (value) => {
      lc.checkInput = value;
      rerender();
    },
    check: () => {
      const host = lc.checkInput.trim();
      if (host === "") return;
      if (lc.check === "pending") return;
      lc.check = "pending";
      rerender();
      void api
        .checkDomain(host)
        .then((outcome) => {
          lc.check = outcome;
          if (outcome.state === "Error") {
            announce(`Check failed: ${outcome.message}`);
          } else {
            announce(
              outcome.result.covered
                ? `${outcome.result.host} matches a routed domain.`
                : `${outcome.result.host} is not routed; it goes direct.`,
            );
          }
        })
        .catch((err: unknown) => {
          lc.check = { state: "Error", message: String(err) };
          announce(`Check failed: ${String(err)}`);
        })
        .finally(() => rerender());
    },

    resync: () => void runMutation("reload", () => api.reload()),
  };

  // Subscribe BEFORE the initial fetch so no push is missed in the mount gap;
  // each event carries the whole VM (last-wins), so an event during the fetch is
  // benign and must not be clobbered by the older initial value.
  api
    .listenViewModel(applyVm)
    .then(async () => {
      const initial = await api.getViewModel();
      if (lastVm === null) applyVm(initial);
    })
    .catch((err: unknown) => {
      root.textContent = `Failed to start the Splitway UI: ${String(err)}`;
    });

  // First paint (the "Connecting…" placeholder) before any VM lands.
  rerender();
}
