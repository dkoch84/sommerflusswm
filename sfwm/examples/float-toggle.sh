#!/usr/bin/env bash

# float-toggle.sh — sc port of the herbstluftwm float-toggle script.
# Toggle the focused window to/from the floating scratchpad (tag 9 on the
# raised float2 overlay monitor) and back to the tag it came from.
#
# Also the default action for the 3-finger swipe-up gesture (swipe-down
# restores, which is the same toggle when the window is on the float tag).

FLOAT_TAG="9"
FLOAT_MONITOR="float2"

# Focused window?
winid=$(sc get_attr clients.focus.winid 2>/dev/null)
[[ -z "$winid" ]] && exit 0

current_tag=$(sc get_attr clients.focus.tag 2>/dev/null)

if [[ "$current_tag" == "$FLOAT_TAG" ]]; then
    # On the float tag — send it home.
    original_tag=$(sc get_attr clients.focus.my_original_tag 2>/dev/null)
    if [[ -n "$original_tag" ]]; then
        sc move "$original_tag"
        sc remove_attr clients.focus.my_original_tag 2>/dev/null
    fi
else
    # Remember where it lives, then send it to the float scratchpad.
    sc new_attr string clients.focus.my_original_tag 2>/dev/null
    sc set_attr clients.focus.my_original_tag "$current_tag"
    sc move "$FLOAT_TAG"
    sc focus_monitor "$FLOAT_MONITOR"
    sc raise_monitor "$FLOAT_MONITOR"
fi
