//! Site favicons for browser windows, and the derived icons (badged,
//! stacked, layout lines) baked into the font.
//!
//! Favicons come from Firefox's favicons.sqlite, and the most visited sites
//! from places.sqlite; Firefox holds both locked while running, so they are
//! read from a private copy.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use image::RgbaImage;
use image::imageops::{self, FilterType};
use regex::Regex;

use crate::icon_map::FAVICON_PREFIX;
use crate::{atspi, raster, xdg};

const BROWSER_APP_IDS: [&str; 7] = [
    "firefox",
    "org.mozilla.firefox",
    "firefox-esr",
    "google-chrome",
    "google-chrome-canary",
    "chromium",
    "chromium-browser",
];
/// Browser suffixes on window titles, which AT-SPI and Sway don't always
/// agree on (e.g. "- Google Chrome" vs "- Google Chrome Canary").
static BROWSER_TITLE_SUFFIX: LazyLock<Result<Regex, regex::Error>> =
    LazyLock::new(|| Regex::new(r" [—-] (Mozilla Firefox|Google Chrome|Chromium)( [\w ]+)?$"));
/// Icons at least this wide are downscaled into the font's 109px strike;
/// smaller ones are upscaled, so the largest available is preferred below it.
const PREFERRED_ICON_WIDTH: u32 = 109;
const SVG_ICON_WIDTH: i64 = 65535;
const REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const MISS_RETRY: Duration = Duration::from_secs(2);
/// Bump when favicon rendering changes, so cached images are re-rendered.
const RENDER_VERSION: u32 = 2;

pub const ICON_CANVAS_PX: u32 = 2 * PREFERRED_ICON_WIDTH;
/// Stacked icons leave room above the pair for the stacked-layout line.
const STACKED_ICON_FRACTION: f64 = 0.42;
const STACK_GAP_PX: u32 = 16;
const LINE_PX: u32 = 8;
/// Emoji start in the symbol blocks; anything below is ordinary text.
const EMOJI_MIN_CODEPOINT: u32 = 0x2190;
/// Draws emoji text with the bundled colour emoji font.
const EMOJI_STYLE: &str = "text, tspan { font-family: 'Noto Color Emoji' }";
const BADGE_FRACTION: f64 = 0.5;
/// Transparent gap cut around the badge so it reads on any titlebar colour.
const BADGE_GAP_FRACTION: f64 = 0.06;

/// Program-map key for a site's favicon.
pub fn favicon_program(host: &str) -> String {
    format!("{FAVICON_PREFIX}{host}")
}

pub fn is_browser(program: Option<&str>) -> bool {
    program.is_some_and(|p| BROWSER_APP_IDS.contains(&p.to_lowercase().as_str()))
}

pub fn page_title(window_title: &str) -> Result<String> {
    let suffix = BROWSER_TITLE_SUFFIX
        .as_ref()
        .map_err(Clone::clone)
        .context("compiling the browser title suffix pattern")?;
    Ok(suffix.replace(window_title, "").into_owned())
}

/// Browser family of an app id or AT-SPI application name.
pub fn browser_family(name: &str) -> &'static str {
    if name.to_lowercase().contains("chrom") {
        "chrome"
    } else {
        "firefox"
    }
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "only used for small positive fractions of the icon canvas"
)]
fn round(value: f64) -> u32 {
    value.round_ties_even() as u32
}

/// Values of `key` in every section of an INI file.
fn ini_values(path: &Path, key: &str) -> Vec<HashMap<String, String>> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut sections: Vec<HashMap<String, String>> = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            sections.push(HashMap::new());
        } else if let (Some(section), Some((name, value))) =
            (sections.last_mut(), line.split_once('='))
        {
            section.insert(name.trim().to_lowercase(), value.trim().to_string());
        }
    }
    sections.retain(|s| s.contains_key(&key.to_lowercase()));
    sections
}

fn firefox_profile_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for root in [
        xdg::home().join(".config/mozilla/firefox"),
        xdg::home().join(".mozilla/firefox"),
    ] {
        for section in ini_values(&root.join("installs.ini"), "default") {
            dirs.extend(section.get("default").map(|default| root.join(default)));
        }
        for section in ini_values(&root.join("profiles.ini"), "path") {
            let Some(path) = section.get("path") else {
                continue;
            };
            let relative = section.get("isrelative").is_none_or(|v| v == "1");
            dirs.push(if relative {
                root.join(path)
            } else {
                PathBuf::from(path)
            });
        }
    }
    let mut unique: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        if !unique.contains(&dir) && dir.join("places.sqlite").is_file() {
            unique.push(dir);
        }
    }
    unique
}

/// Sort key: SVG first, then the smallest icon covering the strike, then
/// the largest of the rest.
fn icon_rank(width: i64) -> (u8, i64) {
    if width == SVG_ICON_WIDTH {
        (0, 0)
    } else if width >= i64::from(PREFERRED_ICON_WIDTH) {
        (1, width)
    } else {
        (2, -width)
    }
}

/// Resolve browser window titles to sites and export their favicons.
pub struct FirefoxFavicons {
    icon_dir: PathBuf,
    db_dir: PathBuf,
    profile_dirs: Vec<PathBuf>,
    copied_at: Option<Instant>,
    title_hosts: HashMap<(String, String), Option<String>>,
    looked_up_at: Option<Instant>,
}

impl FirefoxFavicons {
    pub fn new(cache_dir: &Path) -> Self {
        Self {
            icon_dir: cache_dir.join("favicons"),
            db_dir: cache_dir.join("firefox-db"),
            profile_dirs: firefox_profile_dirs(),
            copied_at: None,
            title_hosts: HashMap::new(),
            looked_up_at: None,
        }
    }

    pub fn available(&self) -> bool {
        !self.profile_dirs.is_empty()
    }

    /// Copy the databases (with their write-ahead logs) out from under the
    /// running browser. Returns whether a fresh copy was made.
    fn refresh(&mut self, force: bool) -> bool {
        if !force
            && self
                .copied_at
                .is_some_and(|t| t.elapsed() < REFRESH_INTERVAL)
        {
            return false;
        }
        self.copied_at = Some(Instant::now());
        for (index, profile) in self.profile_dirs.iter().enumerate() {
            let destination = self.db_dir.join(index.to_string());
            let _ = std::fs::create_dir_all(&destination);
            for name in ["places.sqlite", "favicons.sqlite"] {
                for suffix in ["", "-wal"] {
                    let source = profile.join(format!("{name}{suffix}"));
                    let target = destination.join(format!("{name}{suffix}"));
                    let result = if source.is_file() {
                        std::fs::copy(&source, &target).map(|_| ())
                    } else {
                        std::fs::remove_file(&target).or_else(|e| {
                            if e.kind() == std::io::ErrorKind::NotFound {
                                Ok(())
                            } else {
                                Err(e)
                            }
                        })
                    };
                    if let Err(error) = result {
                        log::debug!("Could not copy {}: {error}", source.display());
                    }
                }
            }
            // A stale shared-memory index would not match the fresh log.
            let _ = std::fs::remove_file(destination.join("places.sqlite-shm"));
            let _ = std::fs::remove_file(destination.join("favicons.sqlite-shm"));
        }
        true
    }

    fn databases(&self) -> Vec<(PathBuf, PathBuf)> {
        (0..self.profile_dirs.len())
            .map(|i| self.db_dir.join(i.to_string()))
            .filter(|dir| dir.join("places.sqlite").is_file())
            .map(|dir| (dir.join("places.sqlite"), dir.join("favicons.sqlite")))
            .collect()
    }

    /// Drop cached title -> site lookups, e.g. after a window title changed.
    pub fn forget_window_titles(&mut self) {
        self.title_hosts.clear();
        self.looked_up_at = None;
    }

    /// The host a browser window with this title is showing.
    pub fn host_for_window(&mut self, program: &str, window_title: Option<&str>) -> Option<String> {
        let window_title = window_title.filter(|t| !t.is_empty())?;
        let title = page_title(window_title)
            .inspect_err(|error| log::warn!("{error:#}"))
            .ok()?;
        let key = (browser_family(program).to_string(), title);
        // Misses are retried: right after login the browser may not have
        // restored its windows onto the accessibility bus yet.
        let stale = self.looked_up_at.is_none_or(|t| t.elapsed() > MISS_RETRY);
        if self.title_hosts.get(&key).cloned().flatten().is_none() && stale {
            self.title_hosts = atspi::address_bar_hosts();
            self.looked_up_at = Some(Instant::now());
        }
        self.title_hosts.get(&key).cloned().flatten()
    }

    /// Most frecent sites, best first.
    pub fn top_hosts(&mut self, limit: usize) -> Vec<String> {
        self.refresh(true);
        let mut hosts: HashMap<String, i64> = HashMap::new();
        for (places, _) in self.databases() {
            let result = rusqlite::Connection::open(&places).and_then(|db| {
                let mut statement = db.prepare(
                    "SELECT host, MAX(frecency) FROM moz_origins WHERE frecency > 0 GROUP BY host",
                )?;
                let rows = statement.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })?;
                for row in rows {
                    let (host, frecency) = row?;
                    let best = hosts.entry(host).or_insert(0);
                    *best = (*best).max(frecency);
                }
                Ok(())
            });
            if let Err(error) = result {
                log::debug!("Could not query {}: {error}", places.display());
            }
        }
        let mut sorted: Vec<(String, i64)> = hosts.into_iter().collect();
        sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        sorted
            .into_iter()
            .take(limit)
            .map(|(host, _)| host)
            .collect()
    }

    /// An icon file with a badge in its corner, e.g. Neovim's icon badged
    /// with the terminal it runs in.
    pub fn badged_icon(&self, icon: &Path, badge: &Path, name: &str) -> Option<PathBuf> {
        let cached = self
            .icon_dir
            .join(format!("{name}+{}.v{RENDER_VERSION}.png", file_stem(badge)));
        if cached.is_file() {
            return Some(cached);
        }
        let data = std::fs::read(icon).ok()?;
        let width = if raster::is_svg(icon) {
            SVG_ICON_WIDTH
        } else {
            0
        };
        let image = add_badge(rasterize(&data, width)?, badge)?;
        save(&image, &cached)
    }

    fn favicon_candidates(&self, host: &str) -> Vec<(i64, Vec<u8>)> {
        let mut candidates = Vec::new();
        let (http, https) = (format!("http://{host}/%"), format!("https://{host}/%"));
        for (_, favicons) in self.databases() {
            if !favicons.is_file() {
                continue;
            }
            let result = rusqlite::Connection::open(&favicons).and_then(|db| {
                let mut statement = db.prepare(
                    "SELECT i.width, i.data FROM moz_icons i
                     JOIN moz_icons_to_pages ip ON ip.icon_id = i.id
                     JOIN moz_pages_w_icons p ON p.id = ip.page_id
                     WHERE p.page_url LIKE ?1 OR p.page_url LIKE ?2
                     UNION
                     SELECT width, data FROM moz_icons
                     WHERE root = 1 AND (icon_url LIKE ?1 OR icon_url LIKE ?2)",
                )?;
                let rows = statement.query_map([&http, &https], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, Option<Vec<u8>>>(1)?))
                })?;
                for row in rows {
                    let (width, data) = row?;
                    candidates.push((width, data.unwrap_or_default()));
                }
                Ok(())
            });
            if let Err(error) = result {
                log::debug!("Could not query {}: {error}", favicons.display());
            }
        }
        candidates
    }

    /// Write the site's best favicon, turned clockwise by `rotation` degrees
    /// and with a badge (e.g. the browser's icon) in its corner, to the cache
    /// and return its path.
    pub fn export_icon(
        &mut self,
        host: &str,
        badge: Option<&Path>,
        rotation: u32,
    ) -> Option<PathBuf> {
        let mut name = match badge {
            Some(badge) => format!("{host}+{}", file_stem(badge)),
            None => host.to_string(),
        };
        if rotation != 0 {
            name = format!("{name}@{rotation}");
        }
        let cached = self.icon_dir.join(format!("{name}.v{RENDER_VERSION}.png"));
        if cached.is_file() {
            return Some(cached);
        }
        let mut candidates = self.favicon_candidates(host);
        if candidates.is_empty() && self.refresh(false) {
            // The site may be newer than our copy of the database.
            candidates = self.favicon_candidates(host);
        }
        candidates.sort_by_key(|(width, _)| icon_rank(*width));
        for (width, data) in candidates {
            let Some(mut image) = (!data.is_empty())
                .then(|| rasterize(&data, width))
                .flatten()
            else {
                continue;
            };
            image = match rotation % 360 {
                90 => imageops::rotate90(&image),
                180 => imageops::rotate180(&image),
                270 => imageops::rotate270(&image),
                _ => image,
            };
            if let Some(badge) = badge {
                image = add_badge(image, badge)?;
            }
            return save(&image, &cached);
        }
        None
    }
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn save(image: &RgbaImage, path: &Path) -> Option<PathBuf> {
    std::fs::create_dir_all(path.parent()?).ok()?;
    match raster::save_png(image, path) {
        Ok(()) => Some(path.to_path_buf()),
        Err(error) => {
            log::warn!("{error:#}");
            None
        }
    }
}

/// Render a favicon blob onto a square canvas, or None if it is unusable.
fn rasterize(data: &[u8], width: i64) -> Option<RgbaImage> {
    let start = data.trim_ascii_start();
    let is_svg =
        width == SVG_ICON_WIDTH || start.starts_with(b"<?xml") || start.starts_with(b"<svg ");
    let image = if is_svg && svg_has_emoji_text(data) {
        // Emoji favicons draw an emoji as SVG <text>; it is cropped to its
        // drawn pixels so it fills the icon.
        let size = 2 * ICON_CANVAS_PX;
        let image = raster::render_svg_styled(data, size, size, None, Some(EMOJI_STYLE)).ok()?;
        let (x, y, w, h) = raster::alpha_bbox(&image)?;
        imageops::crop_imm(&image, x, y, w, h).to_image()
    } else if is_svg {
        raster::render_svg(data, ICON_CANVAS_PX, ICON_CANVAS_PX, None).ok()?
    } else {
        match raster::decode(data) {
            Ok(image) => image,
            Err(error) => {
                log::debug!("Unreadable favicon: {error}");
                return None;
            }
        }
    };
    raster::alpha_bbox(&image)?;
    let side = image.width().max(image.height());
    let mut square = RgbaImage::new(side, side);
    imageops::replace(
        &mut square,
        &image,
        i64::from((side - image.width()) / 2),
        i64::from((side - image.height()) / 2),
    );
    // Upscale tiny favicons crisply rather than blurring them.
    let filter = if side < PREFERRED_ICON_WIDTH {
        FilterType::Nearest
    } else {
        FilterType::Lanczos3
    };
    Some(imageops::resize(
        &square,
        ICON_CANVAS_PX,
        ICON_CANVAS_PX,
        filter,
    ))
}

/// Whether an SVG's <text> elements contain an emoji.
fn svg_has_emoji_text(data: &[u8]) -> bool {
    if !data.windows(5).any(|w| w == b"<text") {
        return false;
    }
    let Ok(text) = std::str::from_utf8(data) else {
        return false;
    };
    let Ok(document) = roxmltree::Document::parse(text) else {
        return false;
    };
    document
        .descendants()
        .filter(|node| node.tag_name().name() == "text")
        .flat_map(|node| node.descendants().filter_map(|n| n.text()))
        .flat_map(str::chars)
        .any(|c| c as u32 >= EMOJI_MIN_CODEPOINT)
}

/// Overlay a small icon in the bottom-right corner, cutting a transparent
/// gap around it.
fn add_badge(mut image: RgbaImage, badge_path: &Path) -> Option<RgbaImage> {
    let size = round(f64::from(ICON_CANVAS_PX) * BADGE_FRACTION);
    let badge = if raster::is_svg(badge_path) {
        raster::render_svg_file(badge_path, size, size).ok()?
    } else {
        let decoded = raster::decode(&std::fs::read(badge_path).ok()?).ok()?;
        imageops::resize(&decoded, size, size, FilterType::Lanczos3)
    };
    let origin = ICON_CANVAS_PX - size;
    let gap = round(f64::from(ICON_CANVAS_PX) * BADGE_GAP_FRACTION) as usize;
    // Grow the badge's own silhouette by the gap and erase the image under it.
    let canvas = ICON_CANVAS_PX as usize;
    let mut silhouette = vec![0u8; canvas * canvas];
    for (x, y, pixel) in badge.enumerate_pixels() {
        *silhouette.get_mut((origin + y) as usize * canvas + (origin + x) as usize)? = pixel[3];
    }
    let silhouette = raster::dilate(&silhouette, canvas, canvas, gap);
    for (pixel, cut) in image.pixels_mut().zip(&silhouette) {
        pixel[3] = pixel[3].saturating_sub(*cut);
    }
    imageops::overlay(&mut image, &badge, i64::from(origin), i64::from(origin));
    Some(image)
}

/// Top edges of the half-size icons in a stacked column, leaving room above
/// the pair and between its halves for layout lines.
fn stack_offsets() -> [(&'static str, u32); 3] {
    let size = round(f64::from(ICON_CANVAS_PX) * STACKED_ICON_FRACTION);
    let bottom = ICON_CANVAS_PX - size;
    let top = bottom - STACK_GAP_PX - size;
    [
        ("top", top),
        ("bottom", bottom),
        ("middle", top.midpoint(bottom)),
    ]
}

/// Half-size copies of an icon at the top, bottom and middle of the left
/// half of its square, so a top and a bottom glyph stack in one column.
pub fn stacked_variants(icon: &Path, dest_dir: &Path, name: &str) -> Option<[PathBuf; 3]> {
    let column = ICON_CANVAS_PX / 2;
    let size = round(f64::from(ICON_CANVAS_PX) * STACKED_ICON_FRACTION);
    let offsets = stack_offsets();
    let paths = offsets.map(|(position, _)| dest_dir.join(format!("{name}-{position}-v2.png")));
    if paths.iter().all(|p| p.is_file()) {
        return Some(paths);
    }
    let data = std::fs::read(icon).ok()?;
    let width = if raster::is_svg(icon) {
        SVG_ICON_WIDTH
    } else {
        0
    };
    let image = rasterize(&data, width)?;
    let small = imageops::resize(&image, size, size, FilterType::Lanczos3);
    std::fs::create_dir_all(dest_dir).ok()?;
    for (path, (_, top)) in paths.iter().zip(offsets) {
        let mut canvas = RgbaImage::new(ICON_CANVAS_PX, ICON_CANVAS_PX);
        imageops::replace(
            &mut canvas,
            &small,
            i64::from((column - size) / 2),
            i64::from(top),
        );
        raster::save_png(&canvas, path).ok()?;
    }
    Some(paths)
}

/// A layout line to draw over an icon: along the bottom of a full icon
/// ("under"), or above ("over") or between ("between") a stacked column.
pub fn line_icon(dest_dir: &Path, position: &str, line_color: &str) -> Result<PathBuf> {
    let path = dest_dir.join(format!(
        "line-{position}-{}.png",
        line_color.trim_start_matches('#')
    ));
    if path.is_file() {
        return Ok(path);
    }
    let column = ICON_CANVAS_PX / 2;
    let bottom = stack_offsets()[1].1;
    let gap_middle = bottom - STACK_GAP_PX / 2;
    let last = ICON_CANVAS_PX - 1;
    let (x0, y0, x1, y1) = match position {
        "under" => (0, ICON_CANVAS_PX - LINE_PX, last, last),
        "over" => (0, 0, column - 1, LINE_PX - 1),
        "between" => (
            column / 6,
            gap_middle - LINE_PX / 2,
            column - 1 - column / 6,
            gap_middle + LINE_PX / 2 - 1,
        ),
        _ => bail!("unknown line position {position:?}"),
    };
    std::fs::create_dir_all(dest_dir)
        .with_context(|| format!("creating {}", dest_dir.display()))?;
    let color = raster::parse_color(line_color);
    let mut canvas = RgbaImage::new(ICON_CANVAS_PX, ICON_CANVAS_PX);
    for y in y0..=y1 {
        for x in x0..=x1 {
            canvas.put_pixel(x, y, color);
        }
    }
    raster::save_png(&canvas, &path).with_context(|| format!("saving {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_titles_drop_browser_suffixes() {
        assert_eq!(
            page_title("Inbox - Gmail — Mozilla Firefox").unwrap(),
            "Inbox - Gmail"
        );
        assert_eq!(page_title("Docs - Google Chrome Canary").unwrap(), "Docs");
        assert_eq!(page_title("New Tab").unwrap(), "New Tab");
        assert_eq!(browser_family("Google Chrome"), "chrome");
        assert_eq!(browser_family("org.mozilla.firefox"), "firefox");
        assert!(is_browser(Some("Firefox")));
        assert!(!is_browser(Some("foot")));
    }

    #[test]
    fn favicon_ranking_prefers_svg_then_covering_sizes() {
        let mut widths = vec![16, 256, SVG_ICON_WIDTH, 32, 128];
        widths.sort_by_key(|w| icon_rank(*w));
        assert_eq!(widths, [SVG_ICON_WIDTH, 128, 256, 32, 16]);
    }

    #[test]
    fn emoji_favicons_fill_the_icon() {
        let svg = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 100'><text y='.9em' font-size='90'>🎯</text></svg>";
        assert!(svg_has_emoji_text(svg.as_bytes()));
        let image = rasterize(svg.as_bytes(), SVG_ICON_WIDTH).unwrap();
        assert_eq!(image.dimensions(), (ICON_CANVAS_PX, ICON_CANVAS_PX));
        let (_, _, width, height) = raster::alpha_bbox(&image).unwrap();
        assert!(width.max(height) > ICON_CANVAS_PX * 9 / 10);
        let colorful = image
            .pixels()
            .any(|p| p[3] > 0 && (i32::from(p[0]) - i32::from(p[2])).abs() > 60);
        assert!(colorful, "emoji should render in colour");
    }

    #[test]
    fn derived_icons() {
        let dir = tempfile::tempdir().unwrap();
        let icon = dir.path().join("icon.svg");
        std::fs::write(
            &icon,
            "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 10 10'><rect width='10' height='10' fill='#0a0'/></svg>",
        )
        .unwrap();
        let [top, bottom, middle] = stacked_variants(&icon, dir.path(), "EC01").unwrap();
        for (path, y) in [(top, 18), (bottom, 126), (middle, 72)] {
            let image = raster::decode(&std::fs::read(path).unwrap()).unwrap();
            assert_eq!(raster::alpha_bbox(&image), Some((8, y, 92, 92)));
        }
        let line = line_icon(dir.path(), "between", "#81b29a").unwrap();
        let image = raster::decode(&std::fs::read(line).unwrap()).unwrap();
        assert_eq!(raster::alpha_bbox(&image), Some((18, 114, 73, 8)));

        let badged = FirefoxFavicons::new(dir.path())
            .badged_icon(&icon, &icon, "job")
            .unwrap();
        let image = raster::decode(&std::fs::read(badged).unwrap()).unwrap();
        // The gap around the badge is transparent; the badge itself is not.
        assert_eq!(image.get_pixel(100, 150)[3], 0);
        assert_eq!(image.get_pixel(160, 160)[3], 255);
        assert_eq!(image.get_pixel(20, 20)[3], 255);
    }
}
