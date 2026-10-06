# Workspace Icon Daemon

> A fork of [David Ott's workspace-icon-daemon](https://github.com/David0tt/workspace-icon-daemon),
> the original idea and implementation. This fork is a rewrite in Rust with a
> few extras.
>
> Also this is ☣️VIBECODED. Use at your own risk

Real, full-color application icons in your **Sway** workspace names and window
titlebars.

![Workspace Icon Daemon in action on Sway with Waybar](docs/assets/demo.gif)

![i3bar with workspace icons](docs/assets/i3bar-example.png)

No icon packs, no hand-written program-to-glyph mappings. The daemon bakes the
icons of everything installed on your system into a color font, then renames
workspaces and titlebars as windows open, close and move. Any bar that can
render fonts can show them — i3bar and Waybar are tested. Browser windows get
the favicon of the site they're showing.

## Install

```sh
cargo install --git https://github.com/omertuc/workspace-icon-daemon
```

Runtime needs Fontconfig and `notify-send` (`fontconfig libnotify-bin` on
Debian/Ubuntu, `fontconfig libnotify` on Arch).

## Setup

**Sway + Waybar** — in the Sway config:

```swayconfig
font pango:monospace 18
exec_always ~/.cargo/bin/workspace-icon-daemon
```

and in `~/.config/waybar/style.css`:

```css
* { font-family: WorkspaceIconDaemon, sans-serif; }
```

**i3** — in the i3 config:

```i3config
font pango:monospace 10
bar { font WorkspaceIconDaemon 20 }
exec_always --no-startup-id ~/.cargo/bin/workspace-icon-daemon
```

The first run builds the icon font and asks you to **log out and back in
once**. After that, icons just appear.

Since workspace names now change, bind workspaces by number:
`bindsym $mod+1 workspace number 1`.

## More

- [Configuration and options](docs/configuration.md)
- [How it works, limitations and credits](docs/how-it-works.md)
- [Using the font builder on its own](docs/icon-font-builder.md)

MIT licensed.
