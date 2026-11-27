#!/bin/sh
# Fast local dev loop for Desktop Phoenix on Linux: build, relaunch with
# LD_PRELOAD and SUNBIRD_DEVTOOLS=1, inspect /health and /logs, reset the
# first-launch modal, and run host tests.
set -eu

REPO=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
TARGET="${CARGO_TARGET_DIR:-$REPO/target}"
DATA="${LIBDISCORD_DATA_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/discord-phoenix}/sunbird"
AUTOSTART="${XDG_CONFIG_HOME:-$HOME/.config}/autostart/sunbird.desktop"
LOG="${SUNBIRD_LOG:-/tmp/sunbird-dev.log}"

find_discord() {
   if [ -n "${DISCORD_APP:-}" ]; then
      if [ ! -f "$DISCORD_APP" ]; then
         echo "No file at DISCORD_APP=$DISCORD_APP" >&2
         exit 1
      fi
      printf '%s\n' "$DISCORD_APP"
      return
   fi
   found=$(command -v discord-phoenix 2>/dev/null || true)
   if [ -z "$found" ]; then
      found=$(ls -t "$PWD"/*[Dd]iscord*.AppImage "$HOME"/Applications/*[Dd]iscord*.AppImage \
         "$HOME"/Downloads/*[Dd]iscord*.AppImage "$HOME"/Desktop/*[Dd]iscord*.AppImage \
         "$HOME"/Applications/*[Pp]hoenix*.AppImage "$HOME"/Downloads/*[Pp]hoenix*.AppImage \
         2>/dev/null | head -n 1 || true)
   fi
   if [ -z "$found" ]; then
      echo "Could not find Discord Phoenix. Set DISCORD_APP=/path/to/Discord.AppImage" >&2
      exit 1
   fi
   printf '%s\n' "$found"
}

api() {
   tok=$(cat "$DATA/debug-shell-token" 2>/dev/null || true)
   curl -fsS -H "X-Sunbird-Token: $tok" "http://127.0.0.1:4242$1"
}

launch() {
   profile="$1"
   lib="$TARGET/$profile/libphoenix.so"
   if [ ! -f "$lib" ]; then
      echo "Missing $lib (run '$0 run' or '$0 release' first)" >&2
      exit 1
   fi
   app=$(find_discord)
   pkill -x discord-phoenix 2>/dev/null || true
   sleep 0.6
   export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
   if [ -z "${WAYLAND_DISPLAY:-}" ] && [ -S "$XDG_RUNTIME_DIR/wayland-0" ]; then
      export WAYLAND_DISPLAY="wayland-0"
   fi
   export DISPLAY="${DISPLAY:-:0}"
   if [ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ] && [ -S "$XDG_RUNTIME_DIR/bus" ]; then
      export DBUS_SESSION_BUS_ADDRESS="unix:path=$XDG_RUNTIME_DIR/bus"
   fi
   export SUNBIRD_DEVTOOLS=1
   export LD_PRELOAD="$lib${LD_PRELOAD:+:$LD_PRELOAD}"
   nohup "$app" >"$LOG" 2>&1 </dev/null &
   echo "Launched $app ($profile) with $lib (log: $LOG)"
}

cmd="${1:-run}"
case "$cmd" in
   run)
      (cd "$REPO" && cargo build --profile dev-fast -p phoenix-loader)
      launch dev-fast
      ;;
   release)
      (cd "$REPO" && cargo build --release -p phoenix-loader)
      launch release
      ;;
   launch)
      launch "${2:-dev-fast}"
      ;;
   reset)
      rm -f "$DATA/launcher.conf"
      echo "Removed $DATA/launcher.conf"
      launch "${2:-dev-fast}"
      ;;
   health)
      api /health
      echo
      ;;
   logs)
      api /logs | tail -n "${2:-40}"
      ;;
   status)
      echo "=== Running ==="
      ps -u "$USER" -o pid,comm | grep discord || echo "(not running)"
      echo "=== launcher.conf ($DATA/launcher.conf) ==="
      cat "$DATA/launcher.conf" 2>/dev/null || echo "(missing)"
      echo "=== autostart ($AUTOSTART) ==="
      cat "$AUTOSTART" 2>/dev/null || echo "(not set)"
      ;;
   test)
      (cd "$REPO" && cargo test -p phoenix-ui-desktop -p phoenix-runtime -p phoenix-runtime-desktop)
      ;;
   install-launcher)
      app=$(find_discord)
      sh "$REPO/scripts/sunbird-linux.sh" "$app"
      ;;
   uninstall)
      sh "$REPO/scripts/sunbird-linux.sh" --uninstall
      ;;
   -h|--help|help)
      cat <<HELP
Usage: $0 <command>

  run               Build dev-fast and relaunch Discord (default)
  release           Build release and relaunch Discord
  launch [profile]  Relaunch without rebuilding (default: dev-fast)
  reset [profile]   Delete launcher.conf (re-triggers onboarding) and relaunch
  health            Show GET /health JSON
  logs [n]          Show last n runtime log lines (default: 40)
  status            Show running PIDs, launcher.conf, and autostart entry
  test              Run Rust unit tests
  install-launcher  Install ~/.local/share/sunbird/sunbird from local script
  uninstall         Uninstall launcher and autostart entry

Environment:
  DISCORD_APP       Path to Discord Phoenix binary or AppImage
  CARGO_TARGET_DIR  Override Cargo target directory (default: <repo>/target)
  SUNBIRD_LOG       Output log file (default: /tmp/sunbird-dev.log)
HELP
      ;;
   *)
      echo "Unknown command: $cmd (try '$0 help')" >&2
      exit 1
      ;;
esac
