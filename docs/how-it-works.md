# How it works

Most bars can't display images, but they can all render fonts. So the daemon
creates a custom color font from program icons on the fly:

1. At startup, the daemon discovers all installed programs and their icons.
   1. It first scans desktop entries in XDG precedence order:
      1. `$XDG_DATA_HOME/applications`
      2. Each directory in `$XDG_DATA_DIRS`, normally:
         - `/usr/local/share/applications`
         - `/usr/share/applications`
      3. `/var/lib/snapd/desktop/applications`
   2. For each `.desktop` file, the daemon extracts the `Icon=`
      and `StartupWMClass=` values.
   3. Icons are discovered with the following precedence:
      1. An absolute SVG or PNG path specified directly by `Icon=`.
      2. SVG application icons. The daemon searches `$XDG_DATA_HOME` first,
         followed by each directory in `$XDG_DATA_DIRS`. Within each data
         directory it checks, in order:
         1. `icons/hicolor/scalable/apps`
         2. `icons/Humanity/apps` application-icon directories
         3. `icons/HighContrast/scalable/apps`
         4. `pixmaps`
      3. PNG application icons, using the same XDG data-directory precedence.
         Within each data directory it checks:
         1. `icons/hicolor` application-icon directories, starting at 128×128,
            then larger sources, followed by progressively smaller fallbacks.
            The target font uses a 109×109 pixel strike, so nearby resolutions
            are preferred to avoid upscaling.
         2. `pixmaps`
      4. Recursive SVG fallback search through:
         1. `$XDG_DATA_HOME/icons`
         2. `$XDG_DATA_HOME/pixmaps`
         3. The corresponding `icons` and `pixmaps` directories under
            `$XDG_DATA_DIRS`
      5. Recursive PNG fallback search through the same directories.
      6. Note: The explicit application-icon directories are searched before
         recursive theme fallbacks. This prevents symbolic or monochrome theme
         icons from overriding full-color application icons in hicolor. Within
         each format/search tier, user-installed icons under `$XDG_DATA_HOME`
         take precedence over identically named icons in system data
         directories.
2. It reserves `U+100000` for the placeholder icon and assigns stable code
   points from Supplementary Private Use Area-B to discovered applications
   (from `U+100001`) and site favicons (from `U+100400`). Icon fonts such as
   Nerd Fonts occupy the other Private Use Areas, and fontconfig would show
   this font's icons in any application whose font lacks one of their glyphs.
3. The first run builds the custom icon font, atomically installs it, refreshes
   fontconfig, notifies, and exits. No workspace or titlebar names are changed,
   since hot-swapping the loaded fonts for running applications (compositor,
   bar) is not possible and a restart (logout/login) is required.
4. Later runs use the installed and loaded icon font and dynamically modify
   workspace names and titlebar names to show these icons, based on window
   events.
5. A newly discovered program (e.g. after a new installation) triggers a
   next-session font build and notification; in the current session the
   placeholder icon is used.

## Limitations

- Newly installed application icons require a logout/login before they replace
  the placeholder glyph.
- Numbers indicating program counts use subscripts/superscripts, which disrupt
  equal spacing between icons. A better solution might use Unicode diacritics
  or embed numbers directly in icons, but this has not been implemented yet.
- The Unicode PUA used contains 6,399 code points, so having more applications
  than that installed could be an issue.
- This system is relatively hacky. If you want something simpler, you can use
  the default approach of mapping programs to Nerd Font symbols used by many
  other setups.

## Possible future features

- [ ] Limit maximum number of icons shown per workspace
- [ ] Better icon spacing when using count indicators
- [ ] Support for more bars. In theory this works with any bar that shows
      workspaces by their name and lets you set the font (with reasonable
      emoji rendering), but the update sequence may need adapting per bar.

## Inspiration

- [i3-workspace-names-daemon](https://github.com/cboddy/i3-workspace-names-daemon)
- [i3scripts/autoname_workspaces.py](https://github.com/justbuchanan/i3scripts)
- [sway-dynamic-names](https://github.com/j-waters/sway-dynamic-names)
- Program icons in titlebars are inspired by
  [1](https://gist.github.com/dmelliot/437924ff581f3f1edd59f44833be6cc6),
  [2](https://github.com/iguanajuice/sway-font-awesome), and
  [3](https://github.com/swaywm/sway/issues/4882#issuecomment-611464474).

Unlike these projects, which rely on pre-existing icon fonts (like FontAwesome
or Nerd Fonts) with predefined program-to-icon mappings, this daemon uses the
actual program icons from your system, so it can show color icons, for almost
any program, automatically.

If you encounter a program where no icon or the wrong icon is shown, please
open an issue.
