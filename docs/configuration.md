# Configuration

Run `workspace-icon-daemon --help` for the full list of options.

## Requirements

Fontconfig and `notify-send` (`fontconfig libnotify-bin` on Debian/Ubuntu,
`fontconfig libnotify` on Arch).

## Options

```bash
workspace-icon-daemon --no-titlebar-icons                  # Don't put icons into the titlebars
workspace-icon-daemon --no-workspace-icons                 # Don't put icons into the workspaces
workspace-icon-daemon --unique-icons <MODE>                # nonunique | unique | numbers_subscript (default) | numbers_superscript
workspace-icon-daemon --no-placeholder-icon                # Hide programs whose icon can't be found
workspace-icon-daemon --title-text-size <PT>               # Title text size; set the compositor font larger for taller titlebars
workspace-icon-daemon --reset                              # Stop daemon, restore workspace and title names, remove state, and exit
workspace-icon-daemon --reset-and-rebuild                  # Reset, prebuild/install the font, and exit
workspace-icon-daemon --verbose                            # Enable debug output
```

## Titlebar icons

The daemon applies a per-window `title_format` such as
`<span font_family='WorkspaceIconDaemon'>ICON</span> %title`. The generated
font is used only for the icon; normal title text continues to use the font
configured in the compositor. Pango markup must be enabled for the title font
(`font pango:...`), and a normal titlebar must be enabled for the icon to be
visible.

## First start and newly installed applications

On the first start, the daemon scans all XDG desktop entries, builds and
installs the font, sends a desktop notification, and exits without renaming
anything. Log out and back in once; the second start enters the normal
monitoring loop.

When an application not present in the session's loaded font is discovered
(e.g. after installing a new program), the daemon installs an updated font for
the next login and sends a notification. It continues running and displays a
placeholder glyph for that application in the current session.

## Workspace keybindings

Workspace names change dynamically, so switch and move by **number**:

```text
bindsym $mod+1 workspace number 1
bindsym $mod+2 workspace number 2
bindsym $mod+Shift+1 move container to workspace number 1
bindsym $mod+Shift+2 move container to workspace number 2
# Repeat for your remaining numbered workspaces.
```

## Browser favicons

Browser windows show the favicon of the current site. The site is read from the
browser's address bar over the AT-SPI accessibility bus, which most desktops
already run, and the favicon from the browser's own profile. Firefox and
Chromium-based browsers (Chrome in any channel, Chromium, Brave, Vivaldi, Edge)
are supported, including Flatpak and Snap installs. Each favicon carries a small
badge of the browser it's shown in.

## Files

- `$XDG_CONFIG_HOME/workspace-icon-daemon/program_icon_map.yaml`
- `$XDG_CACHE_HOME/workspace-icon-daemon/WorkspaceIconDaemon.ttf`
- `$XDG_DATA_HOME/fonts/WorkspaceIconDaemon.ttf`
- `$XDG_CACHE_HOME/workspace-icon-daemon/daemon.pid`
- `$XDG_DATA_HOME/workspace-icon-daemon/placeholder_icon.svg`

The usual XDG defaults apply when those environment variables are unset.

## Building from source

```sh
git clone https://github.com/omertuc/workspace-icon-daemon
cargo install --path ./workspace-icon-daemon
```

For development:

```sh
cargo test
cargo run -- --verbose
```
