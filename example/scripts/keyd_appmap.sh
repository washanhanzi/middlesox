#!/bin/bash
set -euo pipefail

state_dir="${XDG_RUNTIME_DIR:-/tmp}/middlesox"
state_file="${state_dir}/keyd-appmap-appid"
lock_file="${state_dir}/keyd-appmap.lock"
msx_bin="${MSX_BIN:-${HOME}/.local/bin/msx}"

mkdir -p "$state_dir"
exec 9>"$lock_file"
flock -x 9

previous_appid=""
if [[ -f "$state_file" ]]; then
    previous_appid="$(< "$state_file")"
fi

event_appid="$(jq -r '.appid // .curr.appid // ""' <<< "${MSX_CURR:-null}")"
if [[ -n "$event_appid" && "$event_appid" == "$previous_appid" ]]; then
    exit 0
fi

appid=""
if output="$("$msx_bin" get appid 2>/dev/null)"; then
    appid="$(jq -r '. // ""' <<< "$output" 2>/dev/null || true)"
fi

if [[ -z "$appid" ]]; then
    appid="$event_appid"
fi

if [[ "$appid" == "$previous_appid" ]]; then
    exit 0
fi

case "$appid" in
    kitty | dev-warp-warp | dev.warp.Warp)
        keyd bind reset \
            'meta.c = M-c' \
            'meta.v = M-v'
        ;;
    *)
        keyd bind reset
        ;;
esac

printf '%s\n' "$appid" > "$state_file"
