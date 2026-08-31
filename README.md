# ScreenInk

A lightweight screenshot tool for Pop!_OS/GNOME X11. Select any rectangular area,
annotate it with a red pen or rectangle, then save a PNG and copy it to the
clipboard in one action.

## Controls

1. Run `screenink`.
2. Drag to select an area of the screen.
3. Use **Pen** or **Rectangle** to add red annotations.
4. Use **Undo** or `Ctrl+Z` to remove the last annotation.
5. Use **Save** or `Enter` to save and copy the image. `Esc` cancels.

## Build

Install the system dependencies and a current Rust toolchain. The Rust package
shipped with Pop!_OS 22.04 is too old:

```bash
sudo apt install build-essential pkg-config libgtk-4-dev gnome-screenshot
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustup default stable
```

Build and install the application:

```bash
cargo build --release
bash install.sh
```

Set the output directory in `~/.config/screenink/config.toml`. It defaults to
`~/Pictures/Screenshots`. Screen scaling is automatically read from the active
GNOME monitor.

## Replacing GNOME Print Screen

Disable GNOME's built-in screenshot overlay:

```bash
gsettings set org.gnome.shell.keybindings show-screenshot-ui '[]'
```

Then open **Settings → Keyboard → View and Customize Shortcuts → Custom
Shortcuts**, add `ScreenInk` with command `screenink`, and bind it to `Print`.
This preserves your other custom shortcuts.

To restore the default GNOME screenshot UI:

```bash
gsettings reset org.gnome.shell.keybindings show-screenshot-ui
```
