#!/usr/bin/env bash

# resize-viewport.sh — sc port of the herbstluftwm viewport-resize script.
# Resize the focused (floating) window so the browser VIEWPORT hits an exact
# size, accounting for Chrome UI overhead (tabs + address bar + bookmarks bar).
#
# Usage: resize-viewport.sh [width height] [chrome_overhead]
#        resize-viewport.sh                # defaults to 1920x1200 viewport
#        resize-viewport.sh 1280 800       # custom viewport size
#        resize-viewport.sh 1920 1200 115  # custom overhead

VIEWPORT_WIDTH="${1:-1920}"
VIEWPORT_HEIGHT="${2:-1200}"
CHROME_OVERHEAD="${3:-123}"

WINDOW_WIDTH="$VIEWPORT_WIDTH"
WINDOW_HEIGHT=$((VIEWPORT_HEIGHT + CHROME_OVERHEAD))

winid=$(sc get_attr clients.focus.winid 2>/dev/null)
if [[ -z "$winid" ]]; then
    echo "Error: No focused window" >&2
    exit 1
fi

# Preserve the current position.
current_geo=$(sc get_attr clients.focus.floating_geometry 2>/dev/null)
if [[ "$current_geo" =~ ([0-9]+)x([0-9]+)([+-][0-9]+)([+-][0-9]+) ]]; then
    current_x="${BASH_REMATCH[3]}"
    current_y="${BASH_REMATCH[4]}"
else
    current_x="+0"
    current_y="+0"
fi

sc set_attr clients.focus.floating_geometry "${WINDOW_WIDTH}x${WINDOW_HEIGHT}${current_x}${current_y}"

echo "Set window to ${WINDOW_WIDTH}x${WINDOW_HEIGHT} for ${VIEWPORT_WIDTH}x${VIEWPORT_HEIGHT} viewport"
echo "(Chrome overhead: ${CHROME_OVERHEAD}px)"
