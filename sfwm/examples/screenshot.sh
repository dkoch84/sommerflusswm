#!/usr/bin/env bash
# screenshot.sh — flameshot-style screenshots for sfwm.
#
# Stack: grim (wlr-screencopy capture) + slurp (region select, snapping to
# sfwm's real window rects) + satty (annotate/crop/arrow/blur, save + copy)
# + wl-copy (clipboard). Install: pacman -S grim slurp satty wl-clipboard
#
# Usage: screenshot.sh [gui|window|output|full] [savedir]
#   gui     region select first — click a window to snap to it, or drag freely —
#           then annotate in satty (default)
#   window  focused window straight into satty
#   output  focused monitor straight into satty
#   full    every output
#   savedir preset for satty's save button (default ~/Pictures)

mode="${1:-gui}"
savedir="${2:-$HOME/Pictures}"

for dep in grim satty; do
    command -v "$dep" >/dev/null || {
        command -v notify-send >/dev/null && notify-send "screenshot" "$dep is not installed"
        echo "screenshot.sh: missing $dep (pacman -S grim slurp satty wl-clipboard)" >&2
        exit 1
    }
done

mkdir -p "$savedir"
out="$savedir/screenshot-$(date +%Y%m%d-%H%M%S).png"

# hlwm-style WxH+X+Y → grim/slurp "X,Y WxH".
to_box() {
    [[ "$1" =~ ^([0-9]+)x([0-9]+)([+-][0-9]+)([+-][0-9]+)$ ]] || return 1
    echo "${BASH_REMATCH[3]#+},${BASH_REMATCH[4]#+} ${BASH_REMATCH[1]}x${BASH_REMATCH[2]}"
}

geo=""
case "$mode" in
    gui)
        command -v slurp >/dev/null || { echo "screenshot.sh: slurp missing" >&2; exit 1; }
        # Feed sfwm's visible window rects so a click snaps to a window.
        geo=$(sc list_geometry | slurp) || exit 0   # Esc = cancel
        ;;
    window)
        geo=$(to_box "$(sc get_attr clients.focus.geometry)") || {
            echo "screenshot.sh: no focused window" >&2; exit 1; }
        ;;
    output)
        i=$(sc get_attr monitors.focus)
        geo="$(sc get_attr monitors.$i.x),$(sc get_attr monitors.$i.y) $(sc get_attr monitors.$i.width)x$(sc get_attr monitors.$i.height)"
        ;;
    full)
        geo=""
        ;;
    *)
        echo "usage: screenshot.sh [gui|window|output|full] [savedir]" >&2
        exit 2
        ;;
esac

if [[ -n "$geo" ]]; then
    grim -g "$geo" - | satty --filename - --early-exit \
        --copy-command wl-copy --output-filename "$out"
else
    grim - | satty --filename - --early-exit \
        --copy-command wl-copy --output-filename "$out"
fi
