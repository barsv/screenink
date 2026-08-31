#!/usr/bin/env bash
set -euo pipefail

# Replacing a file does not update an already running process. Stop only
# ScreenInk before installation so the next Print keypress starts the new binary.
if pgrep -x screenink >/dev/null; then
  pkill -x screenink
fi

install -Dm755 target/release/screenink "$HOME/.local/bin/screenink"
if [[ ! -f "$HOME/.config/screenink/config.toml" ]]; then
  install -Dm644 config.example.toml "$HOME/.config/screenink/config.toml"
fi

echo "ScreenInk installed to ~/.local/bin/screenink"
echo "Follow the README to bind it to Print Screen."
