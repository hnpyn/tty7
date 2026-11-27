# ntty7

[中文](README.zh-CN.md)

**ntty7** — minimal cross-platform GPU terminal (macOS · Windows · Linux). One process, one window. No tabs, no splits, no daemon.

## What's inside

- GPU rendering on [gpui](https://github.com/zed-industries/zed)
- VT core from [`alacritty_terminal`](https://github.com/alacritty/alacritty)
- The shell runs in-process through [portable-pty](https://github.com/wezterm/wezterm), with the Windows ConPTY helper bundled in
- Keys at a prompt, including Tab and Ctrl+R, go straight to the shell
- Kitty graphics
- Themes, transparency, and window blur
- Light and dark presets that follow the system appearance

Deliberately absent: tabs, splits, sidebars, session restore, and a background server.

## Build

macOS and Windows:

```bash
cargo build --release
```

The binary is `target/release/ntty7` (`ntty7.exe` on Windows).

Linux needs the GPU toolkit first:

```bash
sudo apt-get update
sudo apt-get install -y pkg-config cmake clang libxkbcommon-dev \
  libxkbcommon-x11-dev libfontconfig1-dev libfreetype6-dev \
  libwayland-dev libx11-dev libxcb1-dev libzstd-dev
cargo build --release
```

## Configuration

Settings live in `~/.config/ntty7/config.json` (`%APPDATA%\ntty7\config.json` on Windows). `⌘,` / `Ctrl+,` opens that file. `NTTY7_CONFIG_DIR` overrides the directory.

Keys the app does not know are ignored, so a copied tty7 `config.json` still loads.

Custom themes are `themes/*.yaml` or `themes/*.itermcolors` next to `config.json`.

## Keyboard

`secondary` is `⌘` on macOS and `Ctrl` elsewhere.

| Action | macOS | Windows / Linux |
| --- | --- | --- |
| New window | ⌘N | Ctrl+Shift+N |
| Close window | ⌘W | Ctrl+Shift+W |
| Copy | ⌘C | Ctrl+Shift+C |
| Paste | ⌘V | Ctrl+Shift+V |
| Find | ⌘F | Ctrl+Shift+F |
| Find next / previous | ⌘G / ⌘⇧G | F3 / Shift+F3 |
| Clear scrollback | ⌘K | Ctrl+Shift+K |
| Larger / smaller font | ⌘= / ⌘- | Ctrl+= / Ctrl+- |
| Reset font | ⌘0 | Ctrl+0 |
| Newline | Shift+Enter | Shift+Enter |
| Toggle the composer | ⌘I | Ctrl+Shift+I |
| Full screen | ⌘⌃F | F11 |
| Open config | ⌘, | Ctrl+, |
| Quit | ⌘Q | Ctrl+Shift+Q |

On macOS, copy and paste are the system menu actions. On Windows and Linux, Ctrl+V pastes when the terminal is not on the alternate screen.

## License

[Apache-2.0](LICENSE). ntty7 descends from [tty7](https://github.com/l0ng-ai/tty7). The window is Zed's gpui; the VT parser is `alacritty_terminal`.
