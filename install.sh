#!/usr/bin/env bash
set -euo pipefail

# Replacing a file does not update an already running process. Stop only
# ScreenInk before installation so the next Print keypress starts the new binary.
if pgrep -x screenink >/dev/null; then
  pkill -x screenink
fi

PROGRAM_DIR=/barsv/progs/screenink
install -Dm755 target/release/screenink "$PROGRAM_DIR/screenink"
if [[ ! -f "$PROGRAM_DIR/config.toml" ]]; then
  install -Dm644 config.example.toml "$PROGRAM_DIR/config.toml"
fi
gsettings set \
  org.gnome.settings-daemon.plugins.media-keys.custom-keybinding:/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/screenink/ \
  name ScreenInk
gsettings set \
  org.gnome.settings-daemon.plugins.media-keys.custom-keybinding:/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/screenink/ \
  command "$PROGRAM_DIR/screenink --capture"
gsettings set \
  org.gnome.settings-daemon.plugins.media-keys.custom-keybinding:/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/screenink/ \
  binding Print
gsettings set org.gnome.settings-daemon.plugins.media-keys custom-keybindings \
  "['/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/screenink/']"

echo "ScreenInk installed to $PROGRAM_DIR"
echo "Bind $PROGRAM_DIR/screenink --capture to Print Screen."
