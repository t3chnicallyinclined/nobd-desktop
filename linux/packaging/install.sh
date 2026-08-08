#!/usr/bin/env bash
# NOBD Desktop (Linux) installer.
#
# Designed for an immutable host first — Bazzite, Silverblue, SteamOS. Nothing
# here touches /usr, layers an rpm, or needs a reboot:
#
#   /usr/local/bin           on ostree this is a symlink to /var/usrlocal,
#                            writable and preserved across system updates
#   /etc/udev/rules.d        writable, preserved
#   /etc/systemd/system      writable, preserved
#   /etc/nobd                writable, preserved
#
# Bazzite's own docs recommend Flatpak over rpm-ostree layering; a Flatpak can
# neither install udev rules nor open /dev/uinput, so a plain system install is
# the correct shape for this particular program. See docs/LINUX.md.
#
# Usage:  sudo ./install.sh [--user <name>] [--no-service]
#         sudo ./install.sh --uninstall

set -euo pipefail

PREFIX="${PREFIX:-/usr/local}"
BINDIR="$PREFIX/bin"
UDEVDIR=/etc/udev/rules.d
UNITDIR=/etc/systemd/system
CONFDIR=/etc/nobd
MODPROBEDIR=/etc/modprobe.d
MODLOADDIR=/etc/modules-load.d
GROUP=nobd

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

say()  { printf '  %s\n' "$*"; }
ok()   { printf '  \033[32m✓\033[0m %s\n' "$*"; }
warn() { printf '  \033[33m!\033[0m %s\n' "$*"; }
die()  { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }

[ "$(id -u)" -eq 0 ] || die "run as root (sudo $0)"

TARGET_USER="${SUDO_USER:-}"
DO_SERVICE=1
UNINSTALL=0
while [ $# -gt 0 ]; do
  case "$1" in
    --user) TARGET_USER="$2"; shift 2 ;;
    --no-service) DO_SERVICE=0; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    -h|--help) sed -n '2,25p' "$0"; exit 0 ;;
    *) die "unknown option $1" ;;
  esac
done

# --------------------------------------------------------------------------
if [ "$UNINSTALL" -eq 1 ]; then
  echo "Removing NOBD…"
  systemctl disable --now nobd.service 2>/dev/null || true
  rm -f "$UNITDIR/nobd.service" "$BINDIR/nobdd"
  rm -f "$UDEVDIR/83-nobd.rules" "$UDEVDIR/60-nobd-hide.rules"
  rm -f "$MODPROBEDIR/nobd-usbhid.conf" "$MODLOADDIR/nobd.conf"
  systemctl daemon-reload
  udevadm control --reload-rules && udevadm trigger || true
  ok "removed (config left in $CONFDIR — delete it by hand if you want it gone)"
  exit 0
fi

# --------------------------------------------------------------------------
echo "Installing NOBD…"

BIN_SRC=""
for cand in "$HERE/nobdd" "$HERE/../../target/release/nobdd" "$HERE/../target/release/nobdd"; do
  [ -x "$cand" ] && { BIN_SRC="$cand"; break; }
done
[ -n "$BIN_SRC" ] || die "nobdd binary not found next to this script or in target/release. Build it: cargo build -p nobd-linux --release"

install -d "$BINDIR" "$UDEVDIR" "$UNITDIR" "$CONFDIR" "$MODPROBEDIR" "$MODLOADDIR"
install -m 0755 "$BIN_SRC" "$BINDIR/nobdd"
ok "nobdd → $BINDIR/nobdd"

if ! getent group "$GROUP" >/dev/null; then
  groupadd --system "$GROUP"
  ok "created group $GROUP"
fi
if [ -n "$TARGET_USER" ] && id "$TARGET_USER" >/dev/null 2>&1; then
  if id -nG "$TARGET_USER" | tr ' ' '\n' | grep -qx "$GROUP"; then
    say "$TARGET_USER already in $GROUP"
  else
    usermod -aG "$GROUP" "$TARGET_USER"
    ok "added $TARGET_USER to $GROUP (log out and back in for it to take effect)"
  fi
else
  warn "no target user — pass --user <name> so you can run 'nobdd list' unprivileged"
fi

install -m 0644 "$HERE/83-nobd.rules" "$UDEVDIR/83-nobd.rules"
ok "udev rules → $UDEVDIR/83-nobd.rules"

# uinput is usually autoloaded, but not always at boot — and the static_node
# permission in the udev rule only applies once the module is there.
echo uinput > "$MODLOADDIR/nobd.conf"
modprobe uinput 2>/dev/null || warn "could not modprobe uinput now (it will load at next boot)"
ok "uinput module ensured"

# usbhid joystick poll interval. Written but commented: forcing it is a
# deliberate choice, not a default, because it applies to every HID joystick.
cat > "$MODPROBEDIR/nobd-usbhid.conf" <<'EOF'
# NOBD: force a 1 ms polling interval for USB HID joysticks.
# usbhid otherwise honours whatever bInterval the device advertises, which for
# some sticks is lazier than the hardware can actually sustain. Uncomment, then
# reboot (or reload usbhid) to apply. `nobdd run --jspoll_ms=1` does it live for
# devices bound after the write.
#options usbhid jspoll=1
EOF
ok "modprobe drop-in → $MODPROBEDIR/nobd-usbhid.conf (jspoll commented out)"

if [ ! -f "$CONFDIR/nobdd.conf" ]; then
  install -m 0644 "$HERE/nobdd.conf" "$CONFDIR/nobdd.conf"
  ok "config → $CONFDIR/nobdd.conf"
else
  install -m 0644 "$HERE/nobdd.conf" "$CONFDIR/nobdd.conf.new"
  say "config exists — new default written to $CONFDIR/nobdd.conf.new"
fi

install -m 0644 "$HERE/60-nobd-hide.rules.template" "$CONFDIR/60-nobd-hide.rules.template"
say "Steam-hiding rule template → $CONFDIR/60-nobd-hide.rules.template (opt-in)"

udevadm control --reload-rules && udevadm trigger || warn "udevadm reload failed"

if [ "$DO_SERVICE" -eq 1 ]; then
  install -m 0644 "$HERE/nobd.service" "$UNITDIR/nobd.service"
  systemctl daemon-reload
  systemctl enable --now nobd.service
  ok "service enabled and started"
else
  install -m 0644 "$HERE/nobd.service" "$UNITDIR/nobd.service"
  systemctl daemon-reload
  say "service installed but not enabled (--no-service)"
fi

cat <<EOF

Done.

  nobdd list            what sticks it can see
  nobdd tune            which latency tunings this machine allows
  nobdd probe           measure the uinput hop on YOUR hardware
  systemctl status nobd
  journalctl -u nobd -f

  nobdd set window_ms 6     change the window live
  nobdd set enabled 0       A/B it mid-session
  nobdd stats               grouping + measured latency

If a game sees BOTH your stick and the NOBD pad, Steam is reading the raw
device through hidraw. Fix it with $CONFDIR/60-nobd-hide.rules.template
(instructions are in the file), or turn Steam Input off for the physical stick.
EOF
