// The application: it owns the cached view-model and the request-lifecycle store,
// subscribes to view-model-changed, renders, and wires the mutation controls to
// the Tauri commands.
//
// THE TRUTH CONTRACT, made structural here:
//   - `lastVm` is the ONLY authoritative state, and is assigned ONLY in
//     `applyVm` (the VM-event / initial-fetch path). Search the file: nothing
//     else writes it.
//   - Every mutation handler goes daemon-first: set pending → await the command →
//     clear pending (+ per-action error on failure). It NEVER edits `lastVm`. The
//     daemon's resulting truth arrives via the next view-model-changed event (the
//     backend fires refresh-now after each mutation), and `applyVm` re-renders.
//   - The only other state is the lifecycle store (pending / per-action error /
//     ephemeral CheckDomain result / config-editor input buffers) — see
//     lifecycle.ts. It describes the in-flight interaction, not daemon truth.

import {
  configFileSection,
  connectionHeader,
  el,
  interfacesSection,
  row,
  section,
  statusRows,
  verifySection,
} from "./render";
import {
  beginAction,
  endAction,
  errorFor,
  isPending,
  newLifecycle,
  type ActionKey,
  type CheckState,
  type ConfigForm,
  type Lifecycle,
} from "./lifecycle";
import * as api from "./api";
import type { ConfigFields, ViewModel, VpnBackend } from "./bindings/view-model";

/** Callbacks the rendered controls invoke. Defined as an interface so `renderApp`
 *  stays a pure `(vm, lifecycle, actions) -> nodes` builder, decoupled from the
 *  controller that owns the state. */
export interface Actions {
  toggle(enable: boolean): void;
  addDomain(): void;
  removeDomain(domain: string): void;
  saveConfig(): void;
  resync(): void;
  check(): void;
  setAddInput(value: string): void;
  setCheckInput(value: string): void;
  setConfigText(
    field: "vpn_name" | "openvpn_management" | "openvpn_management_password_file",
    value: string,
  ): void;
  setBackend(value: VpnBackend): void;
}

const BACKENDS: ReadonlyArray<readonly [VpnBackend, string]> = [
  ["network-manager", "NetworkManager"],
  ["openvpn", "OpenVPN"],
];

const DNS_FOOTNOTE =
  "Checks DNS only — whether the name resolves through the VPN's resolver, not whether the address is reachable through the tunnel.";

// --- small builders ---------------------------------------------------------

function errorNote(text: string): HTMLElement {
  return el("p", { class: "message message-Error action-error", text });
}

/** A labelled row whose value cell holds interactive controls. */
function controlRow(label: string, controls: Node[]): HTMLElement {
  return el("div", { class: "row" }, [
    el("span", { class: "label", text: label }),
    el("span", { class: "value" }, controls),
  ]);
}

function textInput(
  id: string,
  value: string,
  placeholder: string,
  disabled: boolean,
  onInput: (value: string) => void,
  onEnter?: () => void,
): HTMLInputElement {
  const input = el("input") as HTMLInputElement;
  input.id = id;
  input.type = "text";
  input.value = value;
  input.placeholder = placeholder;
  input.disabled = disabled;
  input.autocomplete = "off";
  input.addEventListener("input", () => onInput(input.value));
  if (onEnter) {
    input.addEventListener("keydown", (event) => {
      if (event.key === "Enter") onEnter();
    });
  }
  return input;
}

function button(label: string, disabled: boolean, onClick: () => void): HTMLButtonElement {
  const btn = el("button", { text: label }) as HTMLButtonElement;
  btn.disabled = disabled;
  btn.addEventListener("click", onClick);
  return btn;
}

// --- sections ---------------------------------------------------------------

/** A prominent banner when the on-disk config is malformed: the daemon froze on
 *  the last-good config and rejects every mutation until it is fixed on disk. */
function frozenBanner(vm: ViewModel): HTMLElement | null {
  if (vm.status?.routing_state !== "ConfigInvalid") return null;
  return el("div", { class: "frozen-banner" }, [
    el("strong", { text: "Config file invalid. " }),
    el("span", {
      text:
        "The config on disk could not be parsed — edits are rejected until it is fixed on disk. " +
        "The daemon is running on the last-good config.",
    }),
  ]);
}

function statusSection(vm: ViewModel, lc: Lifecycle, actions: Actions): HTMLElement {
  const body = statusRows(vm.status);
  const pending = isPending(lc, "toggle");
  const enabled = vm.status?.enabled ?? false;
  const toggle = button(
    pending ? "working…" : enabled ? "Disable" : "Enable",
    !vm.connected || pending || vm.status === null,
    () => actions.toggle(!enabled),
  );
  const control: Node[] = [el("div", { class: "control" }, [toggle])];
  const err = errorFor(lc, "toggle");
  if (err) control.push(errorNote(err));
  return section("Status", [...body, ...control]);
}

function domainsSection(vm: ViewModel, lc: Lifecycle, actions: Actions): HTMLElement {
  const domains = vm.status?.domains ?? [];
  const body: Node[] = [];

  if (domains.length === 0) {
    body.push(el("p", { class: "muted", text: "(no domains configured)" }));
  } else {
    const list = el("ul", { class: "domains" });
    for (const domain of domains) {
      const key: ActionKey = `remove:${domain}`;
      const pending = isPending(lc, key);
      const remove = button(pending ? "…" : "✖", !vm.connected || pending, () =>
        actions.removeDomain(domain),
      );
      remove.classList.add("remove");
      const li = el("li", {}, [remove, el("span", { text: ` ${domain}` })]);
      const err = errorFor(lc, key);
      if (err) li.appendChild(errorNote(err));
      list.appendChild(li);
    }
    body.push(list);
  }

  const addPending = isPending(lc, "add");
  const input = textInput(
    "add-domain-input",
    lc.addInput,
    "add a domain (e.g. corp.example.com)",
    !vm.connected || addPending,
    (value) => actions.setAddInput(value),
    () => actions.addDomain(),
  );
  const add = button(
    addPending ? "…" : "Add",
    !vm.connected || addPending || lc.addInput.trim() === "",
    () => actions.addDomain(),
  );
  body.push(el("div", { class: "control" }, [input, add]));
  const addErr = errorFor(lc, "add");
  if (addErr) body.push(errorNote(addErr));

  return section("Routed domains", body);
}

function configSection(vm: ViewModel, lc: Lifecycle, actions: Actions): HTMLElement {
  if (!vm.config_loaded) {
    return section("Configuration", [el("p", { class: "muted", text: "loading config…" })]);
  }
  const pending = isPending(lc, "config");
  const disabled = !vm.connected || pending;
  const cfg = lc.config;
  const rows: Node[] = [];

  // Active-file-changed-under-an-unsaved-edit warning (parity with the egui editor).
  if (lc.configPathWarning) {
    rows.push(el("p", { class: "message message-Error config-path-warning", text: lc.configPathWarning }));
  }

  // Interface (vpn_name): free text with a datalist of enumerated interfaces.
  const nameInput = textInput(
    "config-vpn-name",
    cfg.vpn_name,
    "interface name (e.g. tun0)",
    disabled,
    (value) => actions.setConfigText("vpn_name", value),
  );
  nameInput.setAttribute("list", "iface-options");
  const datalist = el("datalist") as HTMLDataListElement;
  datalist.id = "iface-options";
  for (const iface of vm.interfaces) {
    const option = el("option") as HTMLOptionElement;
    option.value = iface.name;
    datalist.appendChild(option);
  }
  rows.push(controlRow("interface (vpn_name)", [nameInput, datalist]));

  // Backend.
  const backend = el("select") as HTMLSelectElement;
  backend.id = "config-backend";
  backend.disabled = disabled;
  for (const [value, label] of BACKENDS) {
    const option = el("option", { text: label }) as HTMLOptionElement;
    option.value = value;
    if (cfg.vpn_backend === value) option.selected = true;
    backend.appendChild(option);
  }
  backend.addEventListener("change", () => actions.setBackend(backend.value as VpnBackend));
  rows.push(controlRow("backend", [backend]));

  // OpenVPN fields only matter for the OpenVPN backend.
  if (cfg.vpn_backend === "openvpn") {
    rows.push(
      controlRow("openvpn management", [
        textInput(
          "config-openvpn-mgmt",
          cfg.openvpn_management,
          "127.0.0.1:7505 or /run/openvpn/mgmt.sock",
          disabled,
          (value) => actions.setConfigText("openvpn_management", value),
        ),
      ]),
    );
    rows.push(
      controlRow("openvpn password file", [
        textInput(
          "config-openvpn-pass",
          cfg.openvpn_management_password_file,
          "(optional) password file path",
          disabled,
          (value) => actions.setConfigText("openvpn_management_password_file", value),
        ),
      ]),
    );
  }

  const save = button(
    pending ? "saving…" : "Save configuration",
    disabled || !cfg.dirty,
    () => actions.saveConfig(),
  );
  rows.push(el("div", { class: "control" }, [save]));
  const err = errorFor(lc, "config");
  if (err) rows.push(errorNote(err));

  return section("Configuration", rows);
}

function checkRoutingText(routingState: string, covered: boolean): string {
  if (!covered) return "not configured to route through the VPN";
  switch (routingState) {
    case "Applied":
      return "routed through the VPN's DNS";
    case "Disabled":
      return "configured to route, but rule application is disabled — not routed right now";
    case "VpnDown":
      return "configured to route, but the VPN is down — not routed right now";
    case "NoDnsFromVpn":
      return "configured to route, but the VPN pushes no DNS — not routed right now";
    case "ApplyFailed":
      return "configured to route, but applying the rules failed (out of sync) — not routed right now";
    case "ConfigInvalid":
      return "the config file on disk is invalid; routing reflects the last-good config";
    default:
      return "configured to route";
  }
}

function checkResult(check: CheckState): HTMLElement {
  if (check === "idle") {
    return el("p", {
      class: "muted check-result",
      text: "Enter a host to check whether it routes through the VPN.",
    });
  }
  if (check === "pending") {
    return el("p", { class: "muted check-result", text: "checking…" });
  }
  if (check.state === "Error") {
    return el("p", { class: "message message-Error check-result", text: check.message });
  }

  const info = check.result;
  const live = info.resolution;
  // Live attribution is authoritative over belief: if the daemon believes the
  // host is routed (`Applied`) but the name actually resolved via a non-VPN link
  // (out-of-band DNS drift), don't reassure "routed through the VPN" — say the
  // live result disagrees and is the one to trust (mirrors the CLI's drift line).
  const answeredElsewhere =
    live?.via_interface != null &&
    info.vpn_interface !== "" &&
    live.via_interface !== info.vpn_interface;
  const routingVerdict =
    info.covered && info.routing_state === "Applied" && answeredElsewhere
      ? `the daemon believes it is routed, but the name resolved via ${live!.via_interface}, ` +
        `not the VPN (${info.vpn_interface}) — trust the live result`
      : checkRoutingText(info.routing_state, info.covered);

  const rows: Node[] = [
    row("host", info.host),
    row(
      "coverage",
      info.covered
        ? info.matched_domain
          ? `covered by ${info.matched_domain}`
          : "covered"
        : "NOT covered by any configured domain",
    ),
    row("routing", routingVerdict),
  ];
  if (live) {
    rows.push(row("resolved", live.addresses.length ? live.addresses.join(", ") : "(none)"));
    if (live.via_interface) {
      // Attribute the answering link relative to the configured VPN interface.
      let viaNote = live.via_interface;
      if (info.vpn_interface !== "") {
        viaNote =
          live.via_interface === info.vpn_interface
            ? `${live.via_interface} — the VPN's link`
            : `${live.via_interface} — not the VPN link (${info.vpn_interface})`;
      }
      rows.push(row("via link", viaNote));
    }
  } else {
    rows.push(row("resolved", "(live resolution unavailable)"));
  }
  rows.push(el("p", { class: "muted footnote", text: DNS_FOOTNOTE }));
  return el("div", { class: "check-result" }, rows);
}

function checkSection(vm: ViewModel, lc: Lifecycle, actions: Actions): HTMLElement {
  const pending = lc.check === "pending";
  const input = textInput(
    "check-input",
    lc.checkInput,
    "paste a host or URL to check",
    !vm.connected || pending,
    (value) => actions.setCheckInput(value),
    () => actions.check(),
  );
  const check = button(
    pending ? "checking…" : "Check",
    !vm.connected || pending || lc.checkInput.trim() === "",
    () => actions.check(),
  );
  return section("Check a domain", [el("div", { class: "control" }, [input, check]), checkResult(lc.check)]);
}

function footerControls(vm: ViewModel, lc: Lifecycle, actions: Actions): HTMLElement {
  const pending = isPending(lc, "reload");
  const resync = button(pending ? "resyncing…" : "Resync", !vm.connected || pending, () =>
    actions.resync(),
  );
  const nodes: Node[] = [el("div", { class: "control" }, [resync])];
  const err = errorFor(lc, "reload");
  if (err) nodes.push(errorNote(err));
  return section("Resync", nodes);
}

/** Pure builder: the whole UI from (vm, lifecycle, actions). `vm === null` only
 *  before the first event lands. */
export function renderApp(vm: ViewModel | null, lc: Lifecycle, actions: Actions): Node[] {
  if (!vm) {
    return [el("p", { class: "muted", text: "Connecting…" })];
  }
  const children: Node[] = [connectionHeader(vm)];
  const frozen = frozenBanner(vm);
  if (frozen) children.push(frozen);
  children.push(
    statusSection(vm, lc, actions),
    domainsSection(vm, lc, actions),
    configSection(vm, lc, actions),
    checkSection(vm, lc, actions),
    verifySection(vm.verify),
    interfacesSection(vm),
    configFileSection(vm),
    footerControls(vm, lc, actions),
  );
  if (vm.message) {
    children.push(el("div", { class: `message message-${vm.message.kind}`, text: vm.message.text }));
  }
  return children;
}

// --- controller -------------------------------------------------------------

function adoptConfig(form: ConfigForm, config: ConfigFields): void {
  form.vpn_name = config.vpn_name;
  form.vpn_backend = config.vpn_backend;
  form.openvpn_management = config.openvpn_management;
  form.openvpn_management_password_file = config.openvpn_management_password_file ?? "";
  form.dirty = false;
}

function emptyToNull(value: string): string | null {
  const trimmed = value.trim();
  return trimmed === "" ? null : trimmed;
}

/** Whether the daemon's reported config already equals what the editor would
 *  send (trimmed; the daemon stores SetConfig's fields verbatim). True after a
 *  save *landed* — including the architecture-§2 "saved-but-apply-failed" case,
 *  where the config was persisted but reconciling it failed and the command still
 *  rejected. Lets the editor tell that apart from a persist/validation/frozen
 *  failure (where the daemon's config is unchanged) using daemon truth, not the
 *  opaque error string. */
function configMatchesDaemon(daemon: ConfigFields, form: ConfigForm): boolean {
  return (
    daemon.vpn_name === form.vpn_name.trim() &&
    daemon.vpn_backend === form.vpn_backend &&
    daemon.openvpn_management === form.openvpn_management.trim() &&
    (daemon.openvpn_management_password_file ?? "") === form.openvpn_management_password_file.trim()
  );
}

/** Boot the app into `root`: subscribe, fetch once, render, and wire controls. */
export function start(root: HTMLElement): void {
  let lastVm: ViewModel | null = null; // the ONLY authoritative state; written only in applyVm
  const lc = newLifecycle();

  function rerender(): void {
    // Preserve focus + caret across the full rebuild (text inputs survive a
    // background VM event mid-typing this way).
    const active = document.activeElement;
    const activeId = active instanceof HTMLElement ? active.id : "";
    const selStart = active instanceof HTMLInputElement ? active.selectionStart : null;
    const selEnd = active instanceof HTMLInputElement ? active.selectionEnd : null;

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
    const prevPath = lastVm?.config_path ?? null;
    lastVm = vm;
    if (
      vm.config_loaded &&
      vm.config &&
      (!lc.config.dirty || configMatchesDaemon(vm.config, lc.config))
    ) {
      // Adopt the daemon's config when it is safe: the editor is clean, OR the
      // daemon's config already equals what the editor would send. The latter
      // covers a *saved-but-apply-failed* save (architecture §2) — the write
      // landed (daemon == sent) even though the command rejected — so the editor
      // goes clean and Save disables, rather than showing the saved edit as
      // unsaved (the next action is Resync, not another write). A
      // persist/validation/frozen failure leaves the daemon's config unchanged
      // (!= the edit), so the editor stays dirty for a fix-and-retry. Adopting
      // also clears any stale path-change warning (the buffers now match the file).
      //
      // Deliberately NOT gated on the config-save pending flag: the refresh-now
      // re-poll can deliver the post-save VM *before* the command's rejection
      // clears pending, and a pending-gate would skip adoption then — leaving the
      // editor stranded dirty once the next (identical) snapshot is deduped. The
      // dirty+matches test is sufficient on its own: while a save is genuinely
      // in flight and unlanded the daemon's config still != the edit (no adopt,
      // no clobber); once it lands, daemon == sent so it adopts regardless of the
      // VM-event vs rejection ordering.
      adoptConfig(lc.config, vm.config);
      lc.configPathWarning = null;
    } else if (
      lc.config.dirty &&
      prevPath !== null &&
      vm.config_path !== "" &&
      vm.config_path !== prevPath
    ) {
      // The daemon's active config file changed under an unsaved edit. Keep the
      // buffers (don't clobber the edit) but warn: Save writes to the daemon's
      // *current* file, so saving old-file buffers would overwrite the new file's
      // editable fields without notice. Parity with GuiCore::load_config_view.
      lc.configPathWarning =
        `The daemon's active config file changed to ${vm.config_path} while you have ` +
        `unsaved edits — re-check before saving (Save writes to the daemon's current file).`;
    }
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

  const actions: Actions = {
    toggle: (enable) => void runMutation("toggle", () => api.setEnabled(enable)),
    addDomain: () => {
      const domain = lc.addInput.trim();
      if (domain === "") return; // input hygiene; the daemon validates the rest
      void runMutation("add", async () => {
        await api.addDomain(domain);
        lc.addInput = ""; // accepted → clear the field (the VM event re-renders the list)
      });
    },
    removeDomain: (domain) => void runMutation(`remove:${domain}`, () => api.removeDomain(domain)),
    saveConfig: () =>
      void runMutation("config", async () => {
        const sent = {
          vpn_name: lc.config.vpn_name.trim(),
          vpn_backend: lc.config.vpn_backend,
          openvpn_management: lc.config.openvpn_management.trim(),
          openvpn_management_password_file: emptyToNull(lc.config.openvpn_management_password_file),
        };
        await api.setConfig(sent);
        // Adopt the exact values we persisted, then mark clean. The daemon stores
        // SetConfig's fields verbatim, so the buffers now match disk. We must NOT
        // rely on the next VM event to correct an unnormalized buffer: when the
        // trimmed value equals what the daemon already had (e.g. the user only
        // added whitespace), the snapshot is unchanged and `should_emit` fires no
        // event — leaving the editor showing a value the daemon never persisted.
        lc.config.vpn_name = sent.vpn_name;
        lc.config.openvpn_management = sent.openvpn_management;
        lc.config.openvpn_management_password_file =
          sent.openvpn_management_password_file ?? "";
        lc.config.dirty = false;
        // The edits are now persisted to the daemon's current file, so the
        // active-file-changed warning (if any) no longer applies.
        lc.configPathWarning = null;
      }),
    resync: () => void runMutation("reload", () => api.reload()),
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
        })
        .catch((err: unknown) => {
          lc.check = { state: "Error", message: String(err) };
        })
        .finally(() => rerender());
    },
    // Rerender on every keystroke: a field's content drives its submit button's
    // disabled state (empty add/check → disabled; clean config → Save disabled),
    // and that's computed at render time, so the button must be rebuilt when the
    // field changes or it would stay stale (a disabled button swallows clicks).
    // `rerender` preserves focus + caret, so live typing is seamless; setting an
    // input's value programmatically does not re-fire "input", so there is no loop.
    setAddInput: (value) => {
      lc.addInput = value;
      rerender();
    },
    setCheckInput: (value) => {
      lc.checkInput = value;
      rerender();
    },
    setConfigText: (field, value) => {
      lc.config[field] = value;
      lc.config.dirty = true; // latch: a VM refresh stops syncing the form
      rerender(); // re-enable Save now that the form is dirty
    },
    setBackend: (value) => {
      lc.config.vpn_backend = value;
      lc.config.dirty = true;
      rerender(); // the OpenVPN fields show/hide on this change
    },
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
