"""Site favicons for browser windows.

Which site a window shows is read from its address bar over AT-SPI (the
accessibility bus), matching AT-SPI windows to Sway windows by their live
title. Favicons come from Firefox's favicons.sqlite, and the most visited
sites from places.sqlite; Firefox holds both locked while running, so they
are read from a private copy.
"""

from __future__ import annotations

import configparser
import logging
import re
import shutil
import sqlite3
import time
from io import BytesIO
from xml.etree import ElementTree
from pathlib import Path
from urllib.parse import urlsplit

import cairosvg
from PIL import Image, ImageChops, ImageDraw, ImageFilter

logger = logging.getLogger(__name__)

FAVICON_PREFIX = "favicon:"
BROWSER_APP_IDS = {
    "firefox",
    "org.mozilla.firefox",
    "firefox-esr",
    "google-chrome",
    "google-chrome-canary",
    "chromium",
    "chromium-browser",
}
# Browser suffixes on window titles, which AT-SPI and Sway don't always agree
# on (e.g. "- Google Chrome" vs "- Google Chrome Canary").
BROWSER_TITLE_SUFFIX = re.compile(
    r" [—-] (Mozilla Firefox|Google Chrome|Chromium)( [\w ]+)?$"
)
# Icons at least this wide are downscaled into the font's 109px strike;
# smaller ones are upscaled, so the largest available is preferred below it.
PREFERRED_ICON_WIDTH = 109
SVG_ICON_WIDTH = 65535
REFRESH_INTERVAL_S = 5.0
MISS_RETRY_S = 2.0
# Bump when favicon rendering changes, so cached images are re-rendered.
RENDER_VERSION = 2


def favicon_program(host: str) -> str:
    """Program-map key for a site's favicon."""
    return f"{FAVICON_PREFIX}{host}"


def _firefox_profile_dirs() -> list[Path]:
    profile_dirs = []
    for root in (
        Path.home() / ".config/mozilla/firefox",
        Path.home() / ".mozilla/firefox",
    ):
        installs = configparser.ConfigParser()
        installs.read(root / "installs.ini")
        for section in installs.sections():
            if default := installs[section].get("Default"):
                profile_dirs.append(root / default)
        profiles = configparser.ConfigParser()
        profiles.read(root / "profiles.ini")
        for section in profiles.sections():
            if path := profiles[section].get("Path"):
                profile_dirs.append(
                    root / path
                    if profiles[section].get("IsRelative", "1") == "1"
                    else Path(path)
                )
    return [
        path
        for path in dict.fromkeys(profile_dirs)
        if (path / "places.sqlite").is_file()
    ]


def _icon_rank(width: int) -> tuple[int, int]:
    """Sort key: SVG first, then the smallest icon covering the strike, then
    the largest of the rest."""
    if width == SVG_ICON_WIDTH:
        return (0, 0)
    if width >= PREFERRED_ICON_WIDTH:
        return (1, width)
    return (2, -width)


class FirefoxFavicons:
    """Resolve Firefox window titles to sites and export their favicons."""

    def __init__(self, cache_dir: Path) -> None:
        self.cache_dir = cache_dir
        self.icon_dir = cache_dir / "favicons"
        self.db_dir = cache_dir / "firefox-db"
        self.profile_dirs = _firefox_profile_dirs()
        self._copied_at = 0.0
        self._title_hosts: dict[tuple[str, str], str | None] = {}
        self._looked_up_at = 0.0

    @property
    def available(self) -> bool:
        return bool(self.profile_dirs)

    @staticmethod
    def is_browser(program: str | None) -> bool:
        return program is not None and program.casefold() in BROWSER_APP_IDS

    @staticmethod
    def page_title(window_title: str) -> str:
        return BROWSER_TITLE_SUFFIX.sub("", window_title)

    def _refresh(self, *, force: bool = False) -> bool:
        """Copy the databases (with their write-ahead logs) out from under the
        running browser. Returns whether a fresh copy was made."""
        if not force and time.monotonic() - self._copied_at < REFRESH_INTERVAL_S:
            return False
        self._copied_at = time.monotonic()
        for index, profile in enumerate(self.profile_dirs):
            destination = self.db_dir / str(index)
            destination.mkdir(parents=True, exist_ok=True)
            for name in ("places.sqlite", "favicons.sqlite"):
                for suffix in ("", "-wal"):
                    source = profile / f"{name}{suffix}"
                    target = destination / f"{name}{suffix}"
                    try:
                        if source.is_file():
                            shutil.copyfile(source, target)
                        else:
                            target.unlink(missing_ok=True)
                    except OSError as exc:
                        logger.debug("Could not copy %s: %s", source, exc)
        return True

    def _databases(self) -> list[tuple[Path, Path]]:
        return [
            (self.db_dir / str(i) / "places.sqlite", self.db_dir / str(i) / "favicons.sqlite")
            for i in range(len(self.profile_dirs))
            if (self.db_dir / str(i) / "places.sqlite").is_file()
        ]

    def forget_window_titles(self) -> None:
        """Drop cached title -> site lookups, e.g. after a window title changed."""
        self._title_hosts.clear()
        self._looked_up_at = 0.0

    def host_for_window(self, program: str, window_title: str | None) -> str | None:
        """Return the host a browser window with this title is showing."""
        if not window_title:
            return None
        key = (_browser(program), self.page_title(window_title))
        # Misses are retried: right after login the browser may not have
        # restored its windows onto the accessibility bus yet.
        stale = time.monotonic() - self._looked_up_at > MISS_RETRY_S
        if self._title_hosts.get(key) is None and stale:
            self._title_hosts = _address_bar_hosts()
            self._looked_up_at = time.monotonic()
        return self._title_hosts.get(key)

    def top_hosts(self, limit: int) -> list[str]:
        """Most frecent sites, best first."""
        self._refresh(force=True)
        hosts: dict[str, int] = {}
        for places, _ in self._databases():
            try:
                with sqlite3.connect(places) as db:
                    for host, frecency in db.execute(
                        "SELECT host, MAX(frecency) FROM moz_origins "
                        "WHERE frecency > 0 GROUP BY host"
                    ):
                        hosts[host] = max(hosts.get(host, 0), frecency)
            except sqlite3.Error as exc:
                logger.debug("Could not query %s: %s", places, exc)
        return sorted(hosts, key=hosts.__getitem__, reverse=True)[:limit]

    def badged_icon(self, icon: Path, badge: Path, name: str) -> Path | None:
        """Write an icon file with a badge in its corner, e.g. Neovim's icon
        badged with the terminal it runs in."""
        cached = self.icon_dir / f"{name}+{badge.stem}.v{RENDER_VERSION}.png"
        if cached.is_file():
            return cached
        is_svg = icon.suffix.lower() == ".svg"
        try:
            data = icon.read_bytes()
        except OSError:
            return None
        image = _rasterize(data, SVG_ICON_WIDTH if is_svg else 0)
        if image is None:
            return None
        self.icon_dir.mkdir(parents=True, exist_ok=True)
        _add_badge(image, badge).save(cached)
        return cached

    def export_icon(
        self, host: str, badge: Path | None = None, rotation: int = 0
    ) -> Path | None:
        """Write the site's best favicon, turned clockwise by `rotation`
        degrees and with a badge (e.g. the browser's icon) in its corner, to
        the cache and return its path."""
        name = f"{host}+{badge.stem}" if badge else host
        if rotation:
            name += f"@{rotation}"
        cached = self.icon_dir / f"{name}.v{RENDER_VERSION}.png"
        if cached.is_file():
            return cached
        candidates: list[tuple[int, bytes]] = []
        for _, favicons in self._databases():
            if not favicons.is_file():
                continue
            try:
                with sqlite3.connect(favicons) as db:
                    candidates.extend(
                        db.execute(
                            """
                            SELECT i.width, i.data FROM moz_icons i
                            JOIN moz_icons_to_pages ip ON ip.icon_id = i.id
                            JOIN moz_pages_w_icons p ON p.id = ip.page_id
                            WHERE p.page_url LIKE ? OR p.page_url LIKE ?
                            UNION
                            SELECT width, data FROM moz_icons
                            WHERE root = 1 AND (icon_url LIKE ? OR icon_url LIKE ?)
                            """,
                            (f"http://{host}/%", f"https://{host}/%") * 2,
                        ).fetchall()
                    )
            except sqlite3.Error as exc:
                logger.debug("Could not query %s: %s", favicons, exc)
        if not candidates and self._refresh():
            # The site may be newer than our copy of the database.
            return self.export_icon(host)
        for width, data in sorted(candidates, key=lambda c: _icon_rank(c[0])):
            if data and (image := _rasterize(data, width)):
                if rotation:
                    image = image.rotate(-rotation, Image.Resampling.BICUBIC)
                if badge:
                    image = _add_badge(image, badge)
                self.icon_dir.mkdir(parents=True, exist_ok=True)
                image.save(cached)
                return cached
        return None


ICON_CANVAS_PX = 2 * PREFERRED_ICON_WIDTH
# Stacked icons leave room above the pair for the stacked-layout line.
STACKED_ICON_FRACTION = 0.42
STACK_GAP_PX = 16
LINE_PX = 8
# Emoji start in the symbol blocks; anything below is ordinary text.
EMOJI_MIN_CODEPOINT = 0x2190
BADGE_FRACTION = 0.5
# Transparent gap cut around the badge so it reads on any titlebar colour.
BADGE_GAP_FRACTION = 0.06


def _rasterize(data: bytes, width: int) -> Image.Image | None:
    """Render a favicon blob onto a square canvas, or None if it is unusable."""
    try:
        is_svg = width == SVG_ICON_WIDTH or data.lstrip()[:5] in (b"<?xml", b"<svg ")
        if is_svg and (emoji := _svg_emoji_text(data)):
            # Emoji favicons draw their emoji as SVG <text>, which needs a
            # colour font renderer; cairosvg and librsvg only draw outlines.
            image = _render_emoji(emoji)
        else:
            if is_svg:
                data = cairosvg.svg2png(
                    bytestring=data,
                    output_width=ICON_CANVAS_PX,
                    output_height=ICON_CANVAS_PX,
                )
            # Pillow opens the largest image of a multi-size ICO.
            image = Image.open(BytesIO(data)).convert("RGBA")
    except Exception as exc:  # Bad blobs raise assorted errors.
        logger.debug("Unreadable favicon: %s", exc)
        return None
    if image is None or image.getchannel("A").getbbox() is None:
        return None
    side = max(image.size)
    square = Image.new("RGBA", (side, side))
    square.paste(image, ((side - image.width) // 2, (side - image.height) // 2))
    # Upscale tiny favicons crisply rather than blurring them.
    resample = (
        Image.Resampling.NEAREST if side < PREFERRED_ICON_WIDTH else Image.Resampling.LANCZOS
    )
    return square.resize((ICON_CANVAS_PX, ICON_CANVAS_PX), resample)


def _svg_emoji_text(data: bytes) -> str | None:
    """The text of an SVG's <text> elements, if it contains an emoji."""
    if b"<text" not in data:
        return None
    try:
        text = "".join(
            "".join(element.itertext())
            for element in ElementTree.fromstring(data).iter()
            if element.tag.rpartition("}")[2] == "text"
        ).strip()
    except ElementTree.ParseError:
        return None
    return text if any(ord(char) >= EMOJI_MIN_CODEPOINT for char in text) else None


def _stack_offsets() -> dict[str, int]:
    """Top edges of the half-size icons in a stacked column, leaving room
    above the pair and between its halves for layout lines."""
    size = round(ICON_CANVAS_PX * STACKED_ICON_FRACTION)
    bottom = ICON_CANVAS_PX - size
    top = bottom - STACK_GAP_PX - size
    return {"top": top, "bottom": bottom, "middle": (top + bottom) // 2}


def stacked_variants(icon: Path, dest_dir: Path, name: str) -> tuple[Path, Path, Path] | None:
    """Half-size copies of an icon at the top, bottom and middle of the left
    half of its square, so a top and a bottom glyph stack in one column."""
    column = ICON_CANVAS_PX // 2
    size = round(ICON_CANVAS_PX * STACKED_ICON_FRACTION)
    offsets = _stack_offsets()
    paths = tuple(dest_dir / f"{name}-{position}-v2.png" for position in offsets)
    if all(path.is_file() for path in paths):
        return paths
    try:
        data = icon.read_bytes()
    except OSError:
        return None
    image = _rasterize(data, SVG_ICON_WIDTH if icon.suffix.lower() == ".svg" else 0)
    if image is None:
        return None
    small = image.resize((size, size), Image.Resampling.LANCZOS)
    dest_dir.mkdir(parents=True, exist_ok=True)
    for path, top in zip(paths, offsets.values()):
        canvas = Image.new("RGBA", (ICON_CANVAS_PX, ICON_CANVAS_PX))
        canvas.paste(small, ((column - size) // 2, top), small)
        canvas.save(path)
    return paths


def line_icon(dest_dir: Path, position: str, line_color: str) -> Path:
    """A layout line to draw over an icon: along the bottom of a full icon
    ("under"), or above ("over") or between ("between") a stacked column."""
    path = dest_dir / f"line-{position}-{line_color.lstrip('#')}.png"
    if path.is_file():
        return path
    column = ICON_CANVAS_PX // 2
    offsets = _stack_offsets()
    gap_middle = offsets["bottom"] - STACK_GAP_PX // 2
    box = {
        "under": (0, ICON_CANVAS_PX - LINE_PX, ICON_CANVAS_PX - 1, ICON_CANVAS_PX - 1),
        "over": (0, 0, column - 1, LINE_PX - 1),
        "between": (
            column // 6,
            gap_middle - LINE_PX // 2,
            column - 1 - column // 6,
            gap_middle + LINE_PX // 2 - 1,
        ),
    }[position]
    dest_dir.mkdir(parents=True, exist_ok=True)
    canvas = Image.new("RGBA", (ICON_CANVAS_PX, ICON_CANVAS_PX))
    ImageDraw.Draw(canvas).rectangle(box, fill=line_color)
    canvas.save(path)
    return path


def _render_emoji(text: str) -> Image.Image | None:
    """Render emoji text with Pango, cropped to its drawn pixels."""
    try:
        import cairo
        import gi

        gi.require_version("Pango", "1.0")
        gi.require_version("PangoCairo", "1.0")
        from gi.repository import Pango, PangoCairo
    except (ImportError, ValueError) as exc:
        logger.debug("Cannot render emoji favicon: %s", exc)
        return None
    size = 2 * ICON_CANVAS_PX
    surface = cairo.ImageSurface(cairo.FORMAT_ARGB32, size, size)
    context = cairo.Context(surface)
    layout = PangoCairo.create_layout(context)
    layout.set_font_description(Pango.FontDescription.from_string(f"sans {size // 2}px"))
    layout.set_text(text, -1)
    PangoCairo.show_layout(context, layout)
    surface.flush()
    image = Image.frombuffer(
        "RGBA", (size, size), bytes(surface.get_data()), "raw", "BGRA", 0, 1
    )
    if (box := image.getchannel("A").getbbox()) is None:
        return None
    return image.crop(box)


def _add_badge(image: Image.Image, badge_path: Path) -> Image.Image:
    """Overlay a small browser icon in the bottom-right corner."""
    size = round(ICON_CANVAS_PX * BADGE_FRACTION)
    if badge_path.suffix.lower() == ".svg":
        badge_data = cairosvg.svg2png(url=str(badge_path), output_width=size, output_height=size)
        badge = Image.open(BytesIO(badge_data)).convert("RGBA")
    else:
        badge = Image.open(badge_path).convert("RGBA")
        badge = badge.resize((size, size), Image.Resampling.LANCZOS)
    origin = ICON_CANVAS_PX - size
    gap = round(ICON_CANVAS_PX * BADGE_GAP_FRACTION)
    # Grow the badge's own silhouette by the gap and erase the favicon under it.
    silhouette = Image.new("L", image.size)
    silhouette.paste(badge.getchannel("A"), (origin, origin))
    silhouette = silhouette.filter(ImageFilter.MaxFilter(2 * gap + 1))
    alpha = ImageChops.subtract(image.getchannel("A"), silhouette)
    image.putalpha(alpha)
    image.alpha_composite(badge, (origin, origin))
    return image

def _browser(name: str) -> str:
    """Browser family of an app id or AT-SPI application name."""
    return "chrome" if re.search(r"chrom", name, re.I) else "firefox"


def _address_bar_hosts() -> dict[tuple[str, str], str | None]:
    """Map each browser window's page title to the host in its address bar."""
    try:
        import pyatspi  # Optional system package (python3-pyatspi).
    except ImportError:
        return {}
    hosts: dict[tuple[str, str], str | None] = {}
    conflicting: set[tuple[str, str]] = set()
    try:
        desktop = pyatspi.Registry.getDesktop(0)
        apps = [desktop.getChildAtIndex(i) for i in range(desktop.childCount)]
    except Exception as exc:  # AT-SPI raises assorted GLib errors.
        logger.debug("AT-SPI unavailable: %s", exc)
        return {}
    for app in apps:
        if app is None or not re.search(r"firefox|chrom", app.name or "", re.I):
            continue
        for index in range(app.childCount):
            window = app.getChildAtIndex(index)
            if window is None or not window.name:
                continue
            title = FirefoxFavicons.page_title(window.name)
            if title == window.name:
                continue  # No page title yet, e.g. a new or loading window.
            key = (_browser(app.name), title)
            host = _host(_address_bar_text(window, pyatspi))
            # Windows sharing a title can't be told apart, so if they show
            # different sites none of them gets a favicon.
            if key in conflicting or (key in hosts and hosts[key] != host):
                conflicting.add(key)
                host = None
            hosts[key] = host
    return hosts


def _address_bar_text(window: object, pyatspi: object) -> str | None:
    """Depth-first search for the address bar, skipping page content."""
    stack = [window]
    while stack:
        node = stack.pop()
        try:
            role = node.getRole()
            if role == pyatspi.ROLE_COMBO_BOX:
                text = node.queryText()
                return text.getText(0, text.characterCount)
            if role == pyatspi.ROLE_DOCUMENT_WEB:
                continue
            stack.extend(
                child
                for child in (node.getChildAtIndex(i) for i in range(node.childCount))
                if child is not None
            )
        except Exception:  # Nodes vanish and raise assorted GLib errors.
            continue
    return None


def _host(address: str | None) -> str | None:
    """Host of an address bar value, which Firefox shows without a scheme."""
    if not address or " " in address.strip():
        return None
    url = address.strip() if "://" in address else f"https://{address.strip()}"
    host = urlsplit(url).netloc.rpartition("@")[2]
    return host if host and ("." in host or ":" in host) else None
