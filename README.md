# Workspace Icon Daemon

> A fork of [David Ott's workspace-icon-daemon](https://github.com/David0tt/workspace-icon-daemon),
> the original idea and implementation. This fork is a rewrite in Rust with a
> few extras.
>
> Also this is ☣️VIBECODED. Use at your own risk

Shows the icons of your open apps in your Sway workspace names and window
titlebars.

![Apps opening into splits, tabs and stacks across workspaces](docs/assets/hero.webp)

Browser windows show the site you're on.

![Browsing between sites, the icon follows](docs/assets/browser.webp)

Terminals can show some app icons instead of the terminal's:

![Opening nvim in a terminal](docs/assets/terminal.webp)

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
(workspaces should appear in scripts as e.g. `workspace number 1` rather than
`workspace 1`).

[Options](docs/configuration.md) ·
[How it works](docs/how-it-works.md) ·
[Font builder](docs/icon-font-builder.md)

# Known problems

[ ] Favicons sometimes use the wrong icon (e.g. Google Flights logo on google.com)
[ ] Dynamic favicons are "frozen" (e.g. Google Calendar changes its icon depending on the current day) 
[ ] Should remove special handling for Claude Code and spin the icon of any window that has a constantly changing title
[ ] Font icons might link into other app that use those codepoints
