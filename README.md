# ScreenInk

A lightweight screenshot tool for Pop!_OS/GNOME. It invokes GNOME's native
area selector through XDG Desktop Portal, then opens the selected PNG for
annotation. Add freehand strokes or rectangles, then save a PNG and copy it to
the clipboard in one action.

## Controls

1. Run `screenink --capture`.
2. Use GNOME's selector to choose an area of the screen.
3. Use **Pen** or **Rectangle** to add red annotations.
4. Use **Undo** or `Ctrl+Z` to remove the last annotation.
5. Use **Save** or `Enter` to save and copy the image. `Esc` cancels either
   the GNOME selector or the editor.

## Build

Install the system dependencies and a current Rust toolchain. The Rust package
shipped with Pop!_OS 22.04 is too old. The project may keep its Rust toolchain
locally in `.cargo/` and `.rustup/`:

```bash
sudo apt install build-essential pkg-config libgtk-4-dev
wget -qO- https://sh.rustup.rs | env CARGO_HOME="$PWD/.cargo" RUSTUP_HOME="$PWD/.rustup" sh -s -- -y --no-modify-path
PATH="$PWD/.cargo/bin:$PATH" CARGO_HOME="$PWD/.cargo" RUSTUP_HOME="$PWD/.rustup" cargo build --release
```

Install the application to `/barsv/progs/screenink`:

```bash
bash install.sh
```

Set the output directory in `/barsv/progs/screenink/config.toml`. It defaults
to `~/Pictures/Screenshots`. `portal_timeout_seconds` defaults to `60`; set it
to `0` to disable the selector timeout. Screen scaling is automatically read
from GNOME.

## Replacing GNOME Print Screen

Disable GNOME's built-in screenshot overlay:

```bash
gsettings set org.gnome.shell.keybindings show-screenshot-ui '[]'
```

Then open **Settings → Keyboard → View and Customize Shortcuts → Custom
Shortcuts**, add `ScreenInk` with command `/barsv/progs/screenink/screenink --capture`,
and bind it to `Print`.
This preserves your other custom shortcuts.

To restore the default GNOME screenshot UI:

```bash
gsettings reset org.gnome.shell.keybindings show-screenshot-ui
```
