#!/bin/bash
# Notify on layout change
#
# Input: $1 - JSON: {"event":"...", "prev":{...}, "curr":{...}}

name=$(echo "$1" | jq -r '.curr.layout_name // "unknown"')
notify-send "msx: Layout" "$name"
