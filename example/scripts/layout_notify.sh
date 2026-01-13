#!/bin/bash
# Notify on layout change
#
# Input:
#   $1         - JSON payload: {"event":"...", "prev":{...}, "curr":{...}}
#   MSX_CURR   - Same current-state payload as JSON
#   MSX_EVENT  - Event name

name=$(echo "$1" | jq -r '.curr.layout_name // "unknown"')
notify-send "msx: Layout" "$name"
