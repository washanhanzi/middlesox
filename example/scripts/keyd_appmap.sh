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

event_name="${MSX_EVENT:-command}"
event_appid="$(jq -r '.appid // .curr.appid // ""' <<< "${MSX_CURR:-null}")"
appid="$event_appid"
appid_origin="event"

if [[ -z "$appid" && "$event_name" == "command" ]]; then
    appid_origin="query"
    if output="$("$msx_bin" get appid 2>/dev/null)"; then
        appid="$(jq -r '. // ""' <<< "$output" 2>/dev/null || true)"
    fi
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
printf 'keyd_appmap: appid=%s appid_origin=%s previous_appid=%s event=%s\n' \
    "${appid:-<empty>}" \
    "$appid_origin" \
    "${previous_appid:-<empty>}" \
    "$event_name"
