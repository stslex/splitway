#!/bin/bash
#
# bootstrap.sh — the privileged install/disable steps for the Splitway macOS
# self-installer, run as ROOT via osascript's `do shell script ... with
# administrator privileges` (one native password prompt). It is bundled inside
# Splitway.app (Contents/Resources) alongside the helper binaries and the
# LaunchDaemon plist; the Tauri `install_service` / `disable_service` commands
# invoke it as `/bin/bash <this> install` / `<this> disable`.
#
# SECURITY: this script is the privileged surface of an UNSIGNED app, so it is
# deliberately inert — it takes exactly one fixed subcommand (install|disable)
# and NO data derived from the GUI or any user input. The console user it adds to
# the group is read from the live system (`stat -f %Su /dev/console`), not passed
# in. Every step is idempotent and a failure aborts before the next, so a partial
# run never leaves the system half-configured (mirrors the daemon's own
# apply-or-rollback contract). See docs/design/macos-self-install.md.

set -euo pipefail

# Pin PATH to the system locations so every privileged tool we invoke (stat,
# install, dseditgroup, launchctl, xattr, …) resolves to a trusted system binary,
# never to something on the caller's inherited PATH. A root script must not trust
# the environment it was launched from.
export PATH=/usr/bin:/bin:/usr/sbin:/sbin

# Resolve our own directory (the bundle's Contents/Resources) so the binaries and
# plist that travel beside us are found regardless of where the .app lives
# (/Applications, ~/Applications, a mounted volume, …). No hardcoded app path.
SELF_DIR="$(cd "$(dirname "$0")" && pwd)"

readonly LABEL="com.splitway.daemon"
readonly PLIST_DST="/Library/LaunchDaemons/${LABEL}.plist"
readonly BIN_DIR="/usr/local/bin"
readonly GROUP="splitway"

log() { printf 'splitway-bootstrap: %s\n' "$*"; }
die() { printf 'splitway-bootstrap: error: %s\n' "$*" >&2; exit 1; }

# --- install ---------------------------------------------------------------

install_binaries() {
    # The daemon is exec'd as ROOT by launchd, so its directory MUST be writable
    # only by root — otherwise a non-root admin (e.g. the Homebrew-on-Intel layout
    # where /usr/local/bin is admin-owned) could swap the binary and have launchd
    # run it as root at the next boot (a persistent local privilege escalation).
    # Create the dir if absent, then pin it to root:wheel 0755 and refuse to
    # proceed if it cannot be made root-owned. BSD `install` has no -D, so the dir
    # is handled here.
    mkdir -p "$BIN_DIR"
    chown root:wheel "$BIN_DIR" || die "cannot make ${BIN_DIR} root-owned; refusing to install a root-run binary into a non-root-writable directory"
    chmod 755 "$BIN_DIR"
    local owner
    owner="$(stat -f '%Su:%Sg' "$BIN_DIR")"
    [ "$owner" = "root:wheel" ] || die "${BIN_DIR} is ${owner}, not root:wheel; refusing to install a root-run binary there"

    local name src dst
    for name in splitway-daemon splitway; do
        src="${SELF_DIR}/${name}"
        dst="${BIN_DIR}/${name}"
        [ -f "$src" ] || die "bundled binary not found: ${src}"
        # root:wheel 0755 — matches the plist install and the now-hardened dir.
        install -o root -g wheel -m 755 "$src" "$dst"
        # A quarantined binary loads but then fails silently under launchd, so
        # strip the flag on the installed copy (ignore "attribute not found").
        xattr -d com.apple.quarantine "$dst" 2>/dev/null || true
        log "installed ${dst}"
    done
}

console_user() {
    # The GUI user to grant socket access. The script runs as root, so $USER is
    # `root` and $SUDO_USER is unset under osascript escalation — read the user
    # at the console instead. Empty / `root` / `loginwindow` means no GUI session.
    stat -f '%Su' /dev/console 2>/dev/null || true
}

ensure_group() {
    if dseditgroup -o read "$GROUP" >/dev/null 2>&1; then
        log "group ${GROUP} already exists"
    else
        dseditgroup -o create "$GROUP"
        log "created group ${GROUP}"
    fi
}

add_user_to_group() {
    local user="$1"
    if dseditgroup -o checkmember -m "$user" "$GROUP" >/dev/null 2>&1; then
        log "${user} is already a member of ${GROUP}"
    else
        dseditgroup -o edit -a "$user" -t user "$GROUP"
        log "added ${user} to ${GROUP}"
    fi
}

install_plist() {
    local src="${SELF_DIR}/${LABEL}.plist"
    [ -f "$src" ] || die "bundled plist not found: ${src}"
    # launchd refuses a plist not owned by root or that is group-writable.
    install -m 644 -o root -g wheel "$src" "$PLIST_DST"
    log "installed ${PLIST_DST}"
}

# Wait until the service record is gone from the system domain (up to ~3s).
# `launchctl bootout` returns before launchd has finished reaping the old job and
# removing its record, and `KeepAlive=true` widens that window — so an immediate
# `bootstrap` of the same label can fail with "Operation already in progress (37)"
# or "Input/output error (5)". Polling `launchctl print` to not-found closes the
# race before we re-bootstrap.
wait_for_service_gone() {
    for _ in $(seq 1 30); do
        launchctl print "system/${LABEL}" >/dev/null 2>&1 || return 0
        sleep 0.1
    done
    # Fall through even if still present; the retry loop below absorbs the residue.
    return 0
}

bootstrap_daemon() {
    # Modern (not legacy `load -w`) idempotent sequence: tear down any prior
    # instance, then bring it up. bootout of an unloaded service errors, so it is
    # tolerated; after it we settle (the record may linger briefly) before
    # re-bootstrapping.
    launchctl bootout "system/${LABEL}" 2>/dev/null || true
    wait_for_service_gone
    # Clear any stale `launchctl disable` flag BEFORE bootstrap — a disabled label
    # makes bootstrap refuse to load. `enable` on a never-seen/already-enabled
    # label is a harmless no-op.
    launchctl enable "system/${LABEL}" 2>/dev/null || true
    # Retry bootstrap a few times: even after the settle poll, launchd can still
    # report the transient "already in progress"/I/O races right after a teardown.
    # The final failure stays fatal (no `|| true`) so a genuine error is not masked.
    for _ in $(seq 1 10); do
        if launchctl bootstrap system "$PLIST_DST" 2>/dev/null; then
            log "bootstrapped ${LABEL}"
            return 0
        fi
        sleep 0.3
    done
    # Last attempt without suppression, so its real error message surfaces on die.
    launchctl bootstrap system "$PLIST_DST" || die "launchctl bootstrap failed for ${LABEL}"
    log "bootstrapped ${LABEL}"
}

do_install() {
    log "installing the Splitway service"
    install_binaries
    ensure_group

    local user
    user="$(console_user)"
    if [ -z "$user" ] || [ "$user" = "root" ] || [ "$user" = "loginwindow" ]; then
        # No interactive desktop user to grant access to. Still install + start
        # the daemon (root can drive it via sudo); the GUI run by a real user
        # later will surface a clear "add me to the group" state.
        log "no console user detected; skipping group membership (run from a desktop session to grant GUI access)"
    else
        add_user_to_group "$user"
    fi

    install_plist
    bootstrap_daemon
    # The daemon self-creates /var/root/.config/splitway/config.json on first
    # run, so nothing is seeded here.
    log "install complete"
}

# --- disable ---------------------------------------------------------------

do_disable() {
    log "disabling the Splitway service"
    # bootout sends SIGTERM, which the daemon traps to revert /etc/resolver
    # before exiting — the system is left clean. Tolerate an already-stopped
    # service.
    launchctl bootout "system/${LABEL}" 2>/dev/null || true
    # Remove the plist so it does not relaunch at boot. Conservative scope: the
    # binaries, the group, group membership, and the config are left in place so
    # a later re-install needs no re-prompt (full uninstall is a separate step).
    rm -f "$PLIST_DST"
    log "disable complete"
}

# --- entry -----------------------------------------------------------------

case "${1:-}" in
    install) do_install ;;
    disable) do_disable ;;
    *) die "usage: bootstrap.sh <install|disable>" ;;
esac
