#!/usr/bin/env bash

# window-switcher.sh — rofi-window replacement using sfwm's built-in dmenu
# (`sc menu`). Lists all clients as "WID [tag] app — title", jumps to the pick.

choice=$(
  sc list_clients \
    | sed -n 's/^win=\([0-9][0-9]*\) tag=\([0-9][0-9]*\) app_id=\(.*\) title=/\1 [\2] \3 — /p' \
    | sc menu
)
[[ -n "$choice" ]] && exec sc jumpto "${choice%% *}"
