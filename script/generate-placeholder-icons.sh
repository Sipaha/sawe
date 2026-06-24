#!/usr/bin/env bash
# Generates placeholder icons for Sawe (a single 'S' on a colored
# background) in every size / format the project needs. Replace the output
# files with proper artwork later; this script is the single source of truth
# for placeholder geometry.

set -euo pipefail

if command -v magick >/dev/null 2>&1; then
    IM=magick
elif command -v convert >/dev/null 2>&1; then
    IM=convert
else
    echo "Need ImageMagick (magick or convert)." >&2
    exit 1
fi

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

BG=#2563EB
FG=#FFFFFF

render() {
    local size="$1"
    local out="$2"
    "$IM" -size "${size}x${size}" "xc:$BG" \
        -gravity center -fill "$FG" -font 'DejaVu-Sans-Bold' \
        -pointsize $((size * 6 / 10)) -annotate 0 'S' "$out"
}

# PNG variants (Linux desktop, macOS retina source)
for variant in '' '-dev' '-preview' '-nightly'; do
    render 512  "$ROOT/crates/zed/resources/app-icon${variant}.png"
    render 1024 "$ROOT/crates/zed/resources/app-icon${variant}@2x.png"
done

# macOS .icns (Document.icns is the per-document file; same placeholder)
if command -v png2icns >/dev/null 2>&1; then
    render 1024 "$TMP/document.png"
    png2icns "$ROOT/crates/zed/resources/Document.icns" "$TMP/document.png"
fi

# Windows .ico — pack 16, 32, 48, 64, 128, 256
mkdir -p "$ROOT/crates/zed/resources/windows"
for variant in '' '-dev' '-preview' '-nightly'; do
    sizes=()
    for s in 16 32 48 64 128 256; do
        f="$TMP/ico-${s}${variant}.png"
        render "$s" "$f"
        sizes+=("$f")
    done
    "$IM" "${sizes[@]}" "$ROOT/crates/zed/resources/windows/app-icon${variant}.ico"
done

echo "Placeholder icons regenerated. Replace with real artwork when ready."
