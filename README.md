# Workspace Icon Daemon

> A fork of [David Ott's workspace-icon-daemon](https://github.com/David0tt/workspace-icon-daemon),
> the original idea and implementation. This fork is a rewrite in Rust with a
> few extras.
>
> Also this is ☣️VIBECODED. Use at your own risk

Shows the icons of your open apps in your Sway workspace names and window
titlebars.

![Workspace Icon Daemon in action on Sway with Waybar](docs/assets/demo.gif)

## Install

```sh
cargo install --git https://github.com/omertuc/workspace-icon-daemon
```

Add to your Sway config:

```swayconfig
font pango:monospace 18
exec_always ~/.cargo/bin/workspace-icon-daemon
```

and to `~/.config/waybar/style.css`:

```css
* { font-family: WorkspaceIconDaemon, sans-serif; }
```

Log out and back in once after the first run.

Workspace names change as windows move, so bind workspaces by number
(`workspace number 1`).

[Options](docs/configuration.md) ·
[How it works](docs/how-it-works.md) ·
[Font builder](docs/icon-font-builder.md)
