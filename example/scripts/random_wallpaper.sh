#!/usr/bin/env bash
set -euo pipefail

# Picks a random image below MSX_WALLPAPER_DIR and applies it to the output
# reported by the workspace tag-change event.

wallpaper_dir="${MSX_WALLPAPER_DIR:-}"
if [[ -z "$wallpaper_dir" ]]; then
    echo "MSX_WALLPAPER_DIR is not set" >&2
    exit 1
fi

case "$wallpaper_dir" in
    "~")
        wallpaper_dir="$HOME"
        ;;
    "~/"*)
        wallpaper_dir="${HOME}/${wallpaper_dir:2}"
        ;;
esac

if [[ ! -d "$wallpaper_dir" ]]; then
    echo "wallpaper directory not found: $wallpaper_dir" >&2
    exit 1
fi

output="$(jq -r '.output // empty' <<< "${MSX_CURR:-null}" 2>/dev/null || true)"
if [[ -z "$output" && $# -gt 0 ]]; then
    output="$(jq -r '.curr.output // empty' <<< "$1" 2>/dev/null || true)"
fi

if [[ -z "$output" ]]; then
    msx_bin="${MSX_BIN:-msx}"
    if msx_output="$("$msx_bin" get output 2>/dev/null)"; then
        output="$(jq -r '. // empty' <<< "$msx_output" 2>/dev/null || true)"
    fi
fi

if [[ -z "$output" ]]; then
    echo "active output not found" >&2
    exit 1
fi

if ! command -v awww >/dev/null 2>&1; then
    echo "awww is required to set per-output wallpapers" >&2
    exit 1
fi

if ! IFS= read -r -d "" wallpaper < <(
    find "$wallpaper_dir" -type f \
        \( -iname "*.avif" \
        -o -iname "*.bmp" \
        -o -iname "*.gif" \
        -o -iname "*.jpeg" \
        -o -iname "*.jpg" \
        -o -iname "*.png" \
        -o -iname "*.webp" \) \
        -print0 \
        | shuf -z -n 1
); then
    echo "no wallpaper images found in $wallpaper_dir" >&2
    exit 1
fi

# awww-daemon may not be accepting connections yet right after login or
# resume; retry briefly instead of failing on the first broken pipe.
max_attempts=5
for ((attempt = 1; attempt <= max_attempts; attempt++)); do
    if awww img -o "$output" --transition-type none "$wallpaper"; then
        exit 0
    fi
    if ((attempt < max_attempts)); then
        sleep 2
    fi
done

echo "failed to set wallpaper after $max_attempts attempts" >&2
exit 1
