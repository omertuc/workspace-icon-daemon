# Using the font builder standalone

The font builder is also installed as a standalone tool to create an icon font
from PNG and SVG files:

```sh
icon-font-builder --help
```

The base font must be a bitmap color font containing CBDT and CBLC tables. A
suitable `NotoColorEmoji.ttf` is bundled and used by default; `--base-font`
selects another. Font generation is generally very tricky, and other base fonts
are not guaranteed to work.

## Building a font

Either put the desired PNG and SVG files into a directory (`--input-folder`),
or pass an explicit list (`--icon-paths`):

```sh
icon-font-builder \
    --icon-paths /usr/share/icons/hicolor/scalable/apps/firefox.svg /usr/share/icons/breeze/apps/16/utilities-terminal.svg \
    --output ./MyIconFont.ttf \
    --family-name MyIconFont \
    --pua-start 0xE100 \
    --remove-original-symbols
```

PNG images are resized to the base font's bitmap strike size when necessary
(for NotoColorEmoji this is 109×109); SVG images are rasterized at that size.
`--remove-original-symbols` removes the base font's existing emoji glyphs and
produces an icon-only font. Omit it to retain the original glyphs as well.

Use `font-manager MyIconFont.ttf` to inspect the generated font and `fc-cache`
to install it. If that doesn't work, try the font viewer directly:
`/usr/lib/font-manager/font-viewer MyIconFont.ttf`.
