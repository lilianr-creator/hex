#!/usr/bin/env bash
# Install Chrome's native messaging host for the Hex presence extension, so
# the popup pairs itself instead of taking a pasted endpoint and token.
#
#   scripts/install-presence-host.sh                 # Chrome, auto-detected Hex
#   scripts/install-presence-host.sh --all           # Chrome, Chromium, Brave, Edge
#   scripts/install-presence-host.sh --browser brave
#   scripts/install-presence-host.sh --binary /Applications/Hex.app/Contents/MacOS/hex
#   scripts/install-presence-host.sh --uninstall
#
# Chrome reads its host manifests at startup: restart it, then reload the
# extension (its ID is fixed by "key" in manifest.json). macOS only.
set -euo pipefail

host=com.anomaly.hex.presence
extension=mdgfgdngkeobanbfneobkjockindglgn
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
binary=${HEX_PRESENCE_BINARY:-}
browsers=()
uninstall=0

usage() {
  sed -n '2,12p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
  case "$1" in
    --binary)
      binary=${2:?--binary needs a path}
      shift 2
      ;;
    --browser)
      browsers+=("${2:?--browser needs a name}")
      shift 2
      ;;
    --all)
      browsers+=(chrome chromium brave edge)
      shift
      ;;
    --uninstall)
      uninstall=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [ "${#browsers[@]}" -eq 0 ]; then
  browsers=(chrome)
fi

# A Hex older than the extension has no presence-host: Chrome would launch
# it, see it exit, and report "Native host has exited".
has_presence_host() {
  [ -x "$1" ] && "$1" help presence-host >/dev/null 2>&1
}

if [ -z "$binary" ] && [ "$uninstall" -eq 0 ]; then
  for candidate in \
    "/Applications/Hex.app/Contents/MacOS/hex" \
    "$here/target/debug/hex" \
    "$here/target/release/hex"; do
    if has_presence_host "$candidate"; then
      binary=$candidate
      break
    fi
  done
fi

if [ "$uninstall" -eq 0 ]; then
  [ -n "$binary" ] || {
    echo "No Hex binary with presence-host found; pass --binary PATH." >&2
    exit 1
  }
  has_presence_host "$binary" || {
    echo "$binary has no presence-host command; it predates the extension." >&2
    exit 1
  }
  binary="$(cd "$(dirname "$binary")" && pwd)/$(basename "$binary")"
fi

# Chrome launches a native host with only the caller's origin as argument,
# so point it at a wrapper that runs the subcommand instead. The wrapper has
# its own folder: on a case-insensitive disk "Hex" is Hex's data folder.
wrapper_dir="$HOME/Library/Application Support/Hex Presence Host"
wrapper="$wrapper_dir/presence-host"
# Where earlier versions of this script put it.
legacy_wrapper="$HOME/Library/Application Support/Hex/presence-host"

write_wrapper() {
  mkdir -p "$wrapper_dir"
  # %q quotes the path for the shell, whatever characters it holds.
  printf '#!/bin/sh\nexec %q presence-host\n' "$binary" >"$wrapper"
  chmod 755 "$wrapper"
  rm -f "$legacy_wrapper"
}

# Whether any browser still has this host installed.
any_manifest_left() {
  for name in chrome chromium brave edge; do
    [ -e "$(host_dir "$name")/$host.json" ] && return 0
  done
  return 1
}

host_dir() {
  case "$1" in
    chrome) echo "$HOME/Library/Application Support/Google/Chrome/NativeMessagingHosts" ;;
    chromium) echo "$HOME/Library/Application Support/Chromium/NativeMessagingHosts" ;;
    brave) echo "$HOME/Library/Application Support/BraveSoftware/Brave-Browser/NativeMessagingHosts" ;;
    edge) echo "$HOME/Library/Application Support/Microsoft Edge/NativeMessagingHosts" ;;
    *) return 1 ;;
  esac
}

for browser in "${browsers[@]}"; do
  if ! dir=$(host_dir "$browser"); then
    echo "unknown browser: $browser (chrome, chromium, brave, edge)" >&2
    exit 2
  fi
  file="$dir/$host.json"
  if [ "$uninstall" -eq 1 ]; then
    rm -f "$file"
    echo "removed $file"
    continue
  fi
  mkdir -p "$dir"
  write_wrapper
  python3 - "$file" "$host" "$wrapper" "$extension" <<'PY'
import json
import sys

path, host, wrapper, extension = sys.argv[1:5]
with open(path, "w", encoding="utf-8") as handle:
    json.dump(
        {
            "name": host,
            "description": "Hex meeting presence",
            "path": wrapper,
            "type": "stdio",
            "allowed_origins": [f"chrome-extension://{extension}/"],
        },
        handle,
        indent=2,
    )
    handle.write("\n")
PY
  chmod 644 "$file"
  echo "installed $file -> $wrapper (exec $binary presence-host)"
done

# The wrapper is shared: keep it while another browser still uses it.
if [ "$uninstall" -eq 1 ] && ! any_manifest_left; then
  rm -f "$wrapper" "$legacy_wrapper"
  rmdir "$wrapper_dir" 2>/dev/null || true
  echo "removed $wrapper"
fi
if [ "$uninstall" -eq 0 ]; then
  echo "Restart the browser, reload the extension, then press Pair with Hex."
fi
