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
        # 1st choice: sfwm's NATIVE region selector — drag a rect, or click a
        # window to snap to its exact rect. Empty reply = Esc/cancel.
        if geo=$(sc select_region 2>/dev/null); then
            [[ -z "$geo" ]] && exit 0
        else
            # Older sfwm without select_region: try slurp with window boxes.
            # slurp segfaults under river on some setups; treat death-by-signal
            # (rc >= 128) as "no slurp".
            geo=""
            if command -v slurp >/dev/null; then
                geo=$(sc list_geometry | slurp)
                rc=$?
                if [ $rc -ne 0 ] && [ $rc -lt 128 ]; then
                    exit 0   # Esc = cancel
                elif [ $rc -ge 128 ]; then
                    geo=""
                fi
            fi
            # Last resort: flameshot-style frozen fullscreen, crop in satty.
            if [[ -z "$geo" ]]; then
                grim - | satty --filename - --fullscreen --early-exit \
                    --init-tool crop --copy-command wl-copy --output-filename "$out"
                exit $?
            fi
        fi
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
