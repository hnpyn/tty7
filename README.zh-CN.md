# ntty7

[English](README.md)

**ntty7** — 极简的跨平台 GPU 终端（macOS · Windows · Linux）。一个进程，一个窗口。没有标签页，没有分屏，没有后台服务。

## 里面有什么

- 用 [gpui](https://github.com/zed-industries/zed) 做 GPU 渲染
- VT 核心来自 [`alacritty_terminal`](https://github.com/alacritty/alacritty)
- shell 在进程内通过 [portable-pty](https://github.com/wezterm/wezterm) 启动，Windows 上附带 ConPTY
- 提示符处的按键（含 Tab 与 Ctrl+R）直接交给 shell
- Kitty 图形协议
- 主题、透明和窗口模糊
- 跟随系统外观的浅色 / 深色主题

刻意不做：标签页、分屏、侧边栏、会话恢复、后台服务。

## 从源码构建

macOS 和 Windows：

```bash
cargo build --release
```

产物是 `target/release/ntty7`（Windows 上是 `ntty7.exe`）。

Linux 需要先装 GPU 工具链：

```bash
sudo apt-get update
sudo apt-get install -y pkg-config cmake clang libxkbcommon-dev \
  libxkbcommon-x11-dev libfontconfig1-dev libfreetype6-dev \
  libwayland-dev libx11-dev libxcb1-dev libzstd-dev
cargo build --release
```

## 配置

配置文件在 `~/.config/ntty7/config.json`（Windows 为 `%APPDATA%\ntty7\config.json`）。`⌘,` / `Ctrl+,` 打开它。`NTTY7_CONFIG_DIR` 可以改这个目录。

不认识的字段会被忽略，所以把旧的 tty7 `config.json` 复制过来也能加载。

自定义主题放在配置目录下的 `themes/*.yaml` 或 `themes/*.itermcolors`。

## 快捷键

`secondary` 在 macOS 上是 `⌘`，在其他系统上是 `Ctrl`。

| 操作 | macOS | Windows / Linux |
| --- | --- | --- |
| 新窗口 | ⌘N | Ctrl+Shift+N |
| 关闭窗口 | ⌘W | Ctrl+Shift+W |
| 复制 | ⌘C | Ctrl+Shift+C |
| 粘贴 | ⌘V | Ctrl+Shift+V |
| 查找 | ⌘F | Ctrl+Shift+F |
| 下一个 / 上一个匹配 | ⌘G / ⌘⇧G | F3 / Shift+F3 |
| 清空回滚 | ⌘K | Ctrl+Shift+K |
| 放大 / 缩小字体 | ⌘= / ⌘- | Ctrl+= / Ctrl+- |
| 重置字体 | ⌘0 | Ctrl+0 |
| 换行 | Shift+Enter | Shift+Enter |
| 打开输入框 | ⌘I | Ctrl+Shift+I |
| 全屏 | ⌘⌃F | F11 |
| 打开配置 | ⌘, | Ctrl+, |
| 退出 | ⌘Q | Ctrl+Shift+Q |

macOS 上复制和粘贴走系统菜单。Windows 和 Linux 上，终端不在备用屏幕时，Ctrl+V 也会粘贴。

## 许可

[Apache-2.0](LICENSE)。ntty7 从 [tty7](https://github.com/l0ng-ai/tty7) 来。窗口层是 Zed 的 gpui，VT 解析是 `alacritty_terminal`。
