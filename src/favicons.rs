//! Site favicons for browser windows, and the derived icons (badged,
//! stacked, layout lines) baked into the font.
//!
//! Favicons and the most visited sites come from each browser's profile
//! databases: places.sqlite and favicons.sqlite for Firefox, History and
//! Favicons for Chromium-based browsers. Browsers hold them locked while
//! running, so they are read from a private copy.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use image::RgbaImage;
use image::imageops::{self, FilterType};
use regex::Regex;

use crate::icon_map::FAVICON_PREFIX;
use crate::{atspi, raster, xdg};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Engine {
    Gecko,
    Chromium,
}

impl Engine {
    /// The (history, favicons) database file names in a profile.
    const fn databases(self) -> [&'static str; 2] {
        match self {
            Self::Gecko => ["places.sqlite", "favicons.sqlite"],
            Self::Chromium => ["History", "Favicons"],
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Browser {
    /// Stable name, part of its favicons' program-map keys.
    pub id: &'static str,
    engine: Engine,
    /// Window app ids and classes, lowercase.
    pub app_ids: &'static [&'static str],
    /// Profile roots, relative to the home directory.
    roots: &'static [&'static str],
}

const BROWSERS: [Browser; 9] = [
    Browser {
        id: "firefox",
        engine: Engine::Gecko,
        app_ids: &[
            "firefox",
            "org.mozilla.firefox",
            "firefox-esr",
            "firefox_firefox",
            "firefox-developer-edition",
            "firefox-nightly",
        ],
        roots: &[
            ".config/mozilla/firefox",
            ".mozilla/firefox",
            ".var/app/org.mozilla.firefox/.mozilla/firefox",
            ".var/app/org.mozilla.firefox/config/mozilla/firefox",
            "snap/firefox/common/.mozilla/firefox",
        ],
    },
    Browser {
        id: "google-chrome",
        engine: Engine::Chromium,
        app_ids: &["google-chrome", "google-chrome-stable", "com.google.chrome"],
        roots: &[
            ".config/google-chrome",
            ".var/app/com.google.Chrome/config/google-chrome",
        ],
    },
    Browser {
        id: "google-chrome-beta",
        engine: Engine::Chromium,
        app_ids: &["google-chrome-beta"],
        roots: &[".config/google-chrome-beta"],
    },
    Browser {
        id: "google-chrome-unstable",
        engine: Engine::Chromium,
        app_ids: &["google-chrome-unstable", "google-chrome-dev"],
        roots: &[".config/google-chrome-unstable"],
    },
    Browser {
        id: "google-chrome-canary",
        engine: Engine::Chromium,
        app_ids: &["google-chrome-canary"],
        roots: &[".config/google-chrome-canary"],
    },
    Browser {
        id: "chromium",
        engine: Engine::Chromium,
        app_ids: &[
            "chromium",
            "chromium-browser",
            "org.chromium.chromium",
            "chromium_chromium",
        ],
        roots: &[
            ".config/chromium",
            ".var/app/org.chromium.Chromium/config/chromium",
            "snap/chromium/common/chromium",
        ],
    },
    Browser {
        id: "brave",
        engine: Engine::Chromium,
        app_ids: &["brave-browser", "brave", "com.brave.browser"],
        roots: &[
            ".config/BraveSoftware/Brave-Browser",
            ".var/app/com.brave.Browser/config/BraveSoftware/Brave-Browser",
        ],
    },
    Browser {
        id: "vivaldi",
        engine: Engine::Chromium,
        app_ids: &["vivaldi-stable", "vivaldi", "com.vivaldi.vivaldi"],
        roots: &[".config/vivaldi"],
    },
    Browser {
        id: "microsoft-edge",
        engine: Engine::Chromium,
        app_ids: &[
            "microsoft-edge",
            "microsoft-edge-stable",
            "microsoft-edge-beta",
            "microsoft-edge-dev",
            "com.microsoft.edge",
        ],
        roots: &[
            ".config/microsoft-edge",
            ".var/app/com.microsoft.Edge/config/microsoft-edge",
        ],
    },
];

/// The browser a window's program is.
pub fn browser(program: &str) -> Option<&'static Browser> {
    let program = program.to_lowercase();
    BROWSERS
        .iter()
        .find(|b| b.app_ids.contains(&program.as_str()))
}

fn browser_by_id(id: &str) -> Option<&'static Browser> {
    BROWSERS.iter().find(|b| b.id == id)
}

/// Browser suffixes on window titles, which AT-SPI and Sway don't always
/// agree on (e.g. "- Google Chrome" vs "- Google Chrome Canary").
static BROWSER_TITLE_SUFFIX: LazyLock<Result<Regex, regex::Error>> = LazyLock::new(|| {
    Regex::new(
        r" [—-] (Mozilla Firefox|Google Chrome|Chromium|Brave|Vivaldi|Microsoft\W*Edge)( [\w ]+)?$",
    )
});
/// Icons at least this wide are downscaled into the font's 109px strike;
/// smaller ones are upscaled, so the largest available is preferred below it.
const PREFERRED_ICON_WIDTH: u32 = 109;
const SVG_ICON_WIDTH: i64 = 65535;
const REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const MISS_RETRY: Duration = Duration::from_secs(2);
/// Browsers whose history hasn't changed for this long aren't in use, and
/// get no favicons up front.
const IDLE_BROWSER: Duration = Duration::from_hours(24 * 30);
/// Chromium visit times count microseconds from 1601; Unix time starts this
/// many seconds later.
const CHROMIUM_EPOCH_OFFSET_SECS: i64 = 11_644_473_600;
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

/// Program-map key for a site's favicon as shown in a browser, which badges
/// it.
pub fn favicon_program(browser: &Browser, host: &str) -> String {
    format!("{FAVICON_PREFIX}{}/{host}", browser.id)
}

/// The browser and host of a favicon program-map key.
pub fn parse_favicon_program(program: &str) -> Option<(&'static Browser, &str)> {
    let (id, host) = program.strip_prefix(FAVICON_PREFIX)?.split_once('/')?;
    Some((browser_by_id(id)?, host))
}

pub fn is_browser(program: Option<&str>) -> bool {
    program.and_then(browser).is_some()
}

pub fn page_title(window_title: &str) -> Result<String> {
    let suffix = BROWSER_TITLE_SUFFIX
        .as_ref()
        .map_err(Clone::clone)
        .context("compiling the browser title suffix pattern")?;
    Ok(suffix.replace(window_title, "").into_owned())
}

/// Whether an AT-SPI application name is a browser.
pub fn is_browser_name(name: &str) -> bool {
    let name = name.to_lowercase();
    ["firefox", "chrom", "brave", "vivaldi", "edge"]
        .iter()
        .any(|n| name.contains(n))
}

/// Browser family of an app id or AT-SPI application name.
pub fn browser_family(name: &str) -> &'static str {
    if name.to_lowercase().contains("firefox") {
        "firefox"
    } else {
        "chrome"
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

fn firefox_profile_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
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
    dirs
}

/// Chromium keeps each profile ("Default", "Profile 1", ...) in its own
/// directory under the root.
fn chromium_profile_dirs(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    dirs.sort();
    dirs
}

#[derive(Debug, Clone)]
struct Profile {
    browser: &'static Browser,
    dir: PathBuf,
}

impl Profile {
    fn history_file(&self) -> PathBuf {
        self.dir.join(self.browser.engine.databases()[0])
    }

    /// Whether the browser was used recently in this profile.
    fn in_use(&self) -> bool {
        std::fs::metadata(self.history_file())
            .and_then(|m| m.modified())
            .is_ok_and(|t| t.elapsed().is_ok_and(|age| age < IDLE_BROWSER))
    }
}

fn browser_profiles(home: &Path) -> Vec<Profile> {
    let mut profiles: Vec<Profile> = Vec::new();
    for browser in &BROWSERS {
        for root in browser.roots.iter().map(|r| home.join(r)) {
            let dirs = match browser.engine {
                Engine::Gecko => firefox_profile_dirs(&root),
                Engine::Chromium => chromium_profile_dirs(&root),
            };
            for dir in dirs {
                let profile = Profile { browser, dir };
                if profile.history_file().is_file()
                    && !profiles.iter().any(|p| p.dir == profile.dir)
                {
                    profiles.push(profile);
                }
            }
        }
    }
    profiles
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

/// How much a Chromium visit `age_days` ago counts towards a site's rank,
/// after Firefox's frecency buckets.
fn recency_weight(age_days: i64) -> f64 {
    match age_days {
        ..4 => 1.0,
        4..14 => 0.7,
        14..31 => 0.5,
        31..90 => 0.3,
        _ => 0.1,
    }
}

fn chromium_now() -> i64 {
    let unix = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX));
    unix.saturating_add(CHROMIUM_EPOCH_OFFSET_SECS * 1_000_000)
}

/// Sites by how much they are visited, from a copy of a profile's history.
fn site_scores(engine: Engine, history: &Path) -> rusqlite::Result<HashMap<String, f64>> {
    let db = rusqlite::Connection::open(history)?;
    let mut scores: HashMap<String, f64> = HashMap::new();
    match engine {
        Engine::Gecko => {
            let mut statement = db.prepare(
                "SELECT host, MAX(frecency) FROM moz_origins WHERE frecency > 0 GROUP BY host",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
            })?;
            for row in rows {
                let (host, frecency) = row?;
                let best = scores.entry(host).or_insert(0.0);
                *best = best.max(frecency);
            }
        }
        Engine::Chromium => {
            let now = chromium_now();
            let mut statement = db.prepare(
                "SELECT url, visit_count, last_visit_time FROM urls
                 WHERE hidden = 0 AND visit_count > 0
                 AND (url LIKE 'http://%' OR url LIKE 'https://%')",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?;
            for row in rows {
                let (url, visits, visited_at) = row?;
                let Some(host) = atspi::host(Some(&url)) else {
                    continue;
                };
                let age_days = (now - visited_at) / (86_400 * 1_000_000);
                #[expect(clippy::cast_precision_loss, reason = "visit counts are small")]
                let score = visits as f64 * recency_weight(age_days);
                *scores.entry(host).or_insert(0.0) += score;
            }
        }
    }
    Ok(scores)
}

/// The (Unix time in microseconds, URL) of the latest visit to a page with
/// this title, from a copy of a profile's history.
fn title_visit(
    engine: Engine,
    history: &Path,
    title: &str,
) -> rusqlite::Result<Option<(i64, String)>> {
    let db = rusqlite::Connection::open(history)?;
    let (query, offset) = match engine {
        Engine::Gecko => (
            "SELECT last_visit_date, url FROM moz_places
             WHERE title = ?1 AND last_visit_date IS NOT NULL
             ORDER BY last_visit_date DESC LIMIT 1",
            0,
        ),
        Engine::Chromium => (
            "SELECT last_visit_time, url FROM urls
             WHERE title = ?1 ORDER BY last_visit_time DESC LIMIT 1",
            CHROMIUM_EPOCH_OFFSET_SECS * 1_000_000,
        ),
    };
    let mut statement = db.prepare(query)?;
    let mut rows = statement.query_map([title], |row| {
        Ok((row.get::<_, i64>(0)? - offset, row.get::<_, String>(1)?))
    })?;
    rows.next().transpose()
}

/// Every stored icon of a site, as (width, data), from a copy of a
/// profile's favicons database.
fn site_icons(
    engine: Engine,
    favicons: &Path,
    host: &str,
) -> rusqlite::Result<Vec<(i64, Vec<u8>)>> {
    let (http, https) = (format!("http://{host}/%"), format!("https://{host}/%"));
    let db = rusqlite::Connection::open(favicons)?;
    let mut statement = db.prepare(match engine {
        Engine::Gecko => {
            "SELECT i.width, i.data FROM moz_icons i
             JOIN moz_icons_to_pages ip ON ip.icon_id = i.id
             JOIN moz_pages_w_icons p ON p.id = ip.page_id
             WHERE p.page_url LIKE ?1 OR p.page_url LIKE ?2
             UNION
             SELECT width, data FROM moz_icons
             WHERE root = 1 AND (icon_url LIKE ?1 OR icon_url LIKE ?2)"
        }
        Engine::Chromium => {
            "SELECT b.width, b.image_data FROM favicon_bitmaps b
             JOIN icon_mapping m ON m.icon_id = b.icon_id
             WHERE m.page_url LIKE ?1 OR m.page_url LIKE ?2
             UNION
             SELECT b.width, b.image_data FROM favicon_bitmaps b
             JOIN favicons f ON f.id = b.icon_id
             WHERE f.url LIKE ?1 OR f.url LIKE ?2"
        }
    })?;
    let rows = statement.query_map([&http, &https], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, Option<Vec<u8>>>(1)?))
    })?;
    rows.map(|row| row.map(|(width, data)| (width, data.unwrap_or_default())))
        .collect()
}

/// Resolve browser window titles to sites and export their favicons.
pub struct Favicons {
    icon_dir: PathBuf,
    db_dir: PathBuf,
    profiles: Vec<Profile>,
    copied_at: Option<Instant>,
    /// Modification times of the database files when last copied.
    copied: HashMap<PathBuf, SystemTime>,
    title_hosts: HashMap<(String, String), Option<String>>,
    looked_up_at: Option<Instant>,
    /// Sites found for (browser, page title) in history, and when.
    history_hosts: HashMap<(&'static str, String), (Instant, Option<String>)>,
}

impl Favicons {
    pub fn new(cache_dir: &Path) -> Self {
        Self {
            icon_dir: cache_dir.join("favicons"),
            db_dir: cache_dir.join("browser-db"),
            profiles: browser_profiles(&xdg::home()),
            copied_at: None,
            copied: HashMap::new(),
            title_hosts: HashMap::new(),
            looked_up_at: None,
            history_hosts: HashMap::new(),
        }
    }

    pub fn available(&self) -> bool {
        !self.profiles.is_empty()
    }

    /// Copy the databases (with their journals) out from under the running
    /// browsers. Returns whether a fresh copy was made.
    fn refresh(&mut self, force: bool) -> bool {
        if !force
            && self
                .copied_at
                .is_some_and(|t| t.elapsed() < REFRESH_INTERVAL)
        {
            return false;
        }
        self.copied_at = Some(Instant::now());
        for (index, profile) in self.profiles.iter().enumerate() {
            let destination = self.db_dir.join(index.to_string());
            let _ = std::fs::create_dir_all(&destination);
            for name in profile.browser.engine.databases() {
                for suffix in ["", "-wal", "-journal"] {
                    let source = profile.dir.join(format!("{name}{suffix}"));
                    let target = destination.join(format!("{name}{suffix}"));
                    let modified = std::fs::metadata(&source).and_then(|m| m.modified()).ok();
                    if modified.is_some()
                        && self.copied.get(&source) == modified.as_ref()
                        && target.is_file()
                    {
                        continue;
                    }
                    let result = if let Some(modified) = modified {
                        self.copied.insert(source.clone(), modified);
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
                // A stale shared-memory index would not match a fresh log.
                let _ = std::fs::remove_file(destination.join(format!("{name}-shm")));
            }
        }
        true
    }

    /// Each profile with the copies of its (history, favicons) databases.
    fn databases(&self) -> Vec<(&Profile, PathBuf, PathBuf)> {
        self.profiles
            .iter()
            .enumerate()
            .map(|(i, profile)| {
                let dir = self.db_dir.join(i.to_string());
                let [history, favicons] = profile.browser.engine.databases();
                (profile, dir.join(history), dir.join(favicons))
            })
            .filter(|(_, history, _)| history.is_file())
            .collect()
    }

    /// Drop cached title -> site lookups, e.g. after a window title changed.
    pub fn forget_window_titles(&mut self) {
        self.title_hosts.clear();
        self.looked_up_at = None;
        self.history_hosts.retain(|_, (_, host)| host.is_some());
    }

    /// The host a browser window with this title is showing: read from its
    /// address bar, or else the site last visited with this page title.
    pub fn host_for_window(&mut self, program: &str, window_title: Option<&str>) -> Option<String> {
        let window_title = window_title.filter(|t| !t.is_empty())?;
        let title = page_title(window_title)
            .inspect_err(|error| log::warn!("{error:#}"))
            .ok()?;
        if title == window_title {
            return None; // No page title yet, e.g. a new or loading window.
        }
        let key = (browser_family(program).to_string(), title);
        // Misses are retried: right after login the browser may not have
        // restored its windows onto the accessibility bus yet.
        let stale = self.looked_up_at.is_none_or(|t| t.elapsed() > MISS_RETRY);
        if self.title_hosts.get(&key).cloned().flatten().is_none() && stale {
            self.title_hosts = atspi::address_bar_hosts();
            self.looked_up_at = Some(Instant::now());
        }
        if let Some(host) = self.title_hosts.get(&key).cloned().flatten() {
            return Some(host);
        }
        // Chromium shows its address bar over AT-SPI only to screen readers.
        self.history_host(browser(program)?, key.1)
    }

    /// The site most recently visited with this page title in the browser.
    fn history_host(&mut self, browser: &'static Browser, title: String) -> Option<String> {
        let key = (browser.id, title);
        if let Some((at, host)) = self.history_hosts.get(&key)
            && (host.is_some() || at.elapsed() < MISS_RETRY)
        {
            return host.clone();
        }
        // The visit may be newer than our copy of the history.
        self.refresh(false);
        let latest = self
            .databases()
            .into_iter()
            .filter(|(profile, _, _)| profile.browser == browser)
            .filter_map(|(profile, history, _)| {
                title_visit(profile.browser.engine, &history, &key.1)
                    .inspect_err(|error| {
                        log::debug!("Could not query {}: {error}", history.display());
                    })
                    .ok()
                    .flatten()
            })
            .max_by_key(|(visited_at, _)| *visited_at);
        let host = latest.and_then(|(_, url)| atspi::host(Some(&url)));
        self.history_hosts
            .insert(key, (Instant::now(), host.clone()));
        host
    }

    /// Each browser in use with its most visited sites, best first.
    pub fn top_hosts(&mut self, limit: usize) -> Vec<(&'static Browser, String)> {
        self.refresh(true);
        let mut by_browser: HashMap<&'static str, HashMap<String, f64>> = HashMap::new();
        for (profile, history, _) in self.databases() {
            if !profile.in_use() {
                continue;
            }
            let scores = match site_scores(profile.browser.engine, &history) {
                Ok(scores) => scores,
                Err(error) => {
                    log::debug!("Could not query {}: {error}", history.display());
                    continue;
                }
            };
            let hosts = by_browser.entry(profile.browser.id).or_default();
            for (host, score) in scores {
                let best = hosts.entry(host).or_insert(0.0);
                *best = best.max(score);
            }
        }
        let mut top = Vec::new();
        for browser in &BROWSERS {
            let Some(hosts) = by_browser.remove(browser.id) else {
                continue;
            };
            let mut sorted: Vec<(String, f64)> = hosts.into_iter().collect();
            sorted.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            top.extend(
                sorted
                    .into_iter()
                    .take(limit)
                    .map(|(host, _)| (browser, host)),
            );
        }
        top
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

    /// The site's icons stored by any browser; a site's icon is the same
    /// whichever browser fetched it.
    fn favicon_candidates(&self, host: &str) -> Vec<(i64, Vec<u8>)> {
        let mut candidates = Vec::new();
        for (profile, _, favicons) in self.databases() {
            if !favicons.is_file() {
                continue;
            }
            match site_icons(profile.browser.engine, &favicons, host) {
                Ok(icons) => candidates.extend(icons),
                Err(error) => log::debug!("Could not query {}: {error}", favicons.display()),
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
    fn browsers_by_app_id() {
        assert_eq!(
            browser("Google-chrome-canary").unwrap().id,
            "google-chrome-canary"
        );
        assert_eq!(browser("brave-browser").unwrap().id, "brave");
        assert_eq!(browser("org.mozilla.firefox").unwrap().id, "firefox");
        assert!(browser("foot").is_none());
        let chrome = browser("google-chrome").unwrap();
        let key = favicon_program(chrome, "example.org:8080");
        assert_eq!(key, "favicon:google-chrome/example.org:8080");
        assert_eq!(
            parse_favicon_program(&key),
            Some((chrome, "example.org:8080"))
        );
        assert_eq!(parse_favicon_program("favicon:example.org"), None);
        assert_eq!(page_title("Docs - Brave").unwrap(), "Docs");
        assert_eq!(browser_family("Brave Browser"), "chrome");
    }

    #[test]
    fn chromium_profiles_history_and_icons() {
        let home = tempfile::tempdir().unwrap();
        let profile = home.path().join(".config/google-chrome-canary/Profile 1");
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::create_dir_all(home.path().join(".config/google-chrome-canary/Crashpad")).unwrap();
        let history = rusqlite::Connection::open(profile.join("History")).unwrap();
        let now = chromium_now();
        let old = now - 200 * 86_400 * 1_000_000;
        history
            .execute_batch(&format!(
                "CREATE TABLE urls (id INTEGER PRIMARY KEY, url TEXT, title TEXT,
                   visit_count INTEGER, typed_count INTEGER, last_visit_time INTEGER,
                   hidden INTEGER);
                 INSERT INTO urls (url, visit_count, last_visit_time, hidden) VALUES
                   ('https://often.example/a', 5, {now}, 0),
                   ('https://often.example/b', 5, {now}, 0),
                   ('https://elsewhere.example/', 1, {old}, 0),
                   ('https://once.example/', 1, {now}, 0),
                   ('https://long-ago.example/', 50, {old}, 0),
                   ('https://hidden.example/', 99, {now}, 1),
                   ('chrome://settings/', 99, {now}, 0);
                 UPDATE urls SET title = 'Often B'
                   WHERE url IN ('https://often.example/b', 'https://elsewhere.example/');"
            ))
            .unwrap();
        let favicons = rusqlite::Connection::open(profile.join("Favicons")).unwrap();
        favicons
            .execute_batch(
                "CREATE TABLE favicons (id INTEGER PRIMARY KEY, url TEXT, icon_type INTEGER);
                 CREATE TABLE favicon_bitmaps (id INTEGER PRIMARY KEY, icon_id INTEGER,
                   last_updated INTEGER, image_data BLOB, width INTEGER, height INTEGER);
                 CREATE TABLE icon_mapping (id INTEGER PRIMARY KEY, page_url TEXT,
                   icon_id INTEGER);
                 INSERT INTO favicons VALUES (1, 'https://cdn.example/often.png', 1);
                 INSERT INTO favicon_bitmaps (icon_id, image_data, width) VALUES
                   (1, x'01', 16), (1, x'02', 32);
                 INSERT INTO icon_mapping (page_url, icon_id) VALUES
                   ('https://often.example/a', 1);",
            )
            .unwrap();

        assert_eq!(
            title_visit(Engine::Chromium, &profile.join("History"), "Often B")
                .unwrap()
                .map(|(_, url)| url)
                .as_deref(),
            Some("https://often.example/b")
        );
        assert_eq!(
            title_visit(Engine::Chromium, &profile.join("History"), "Nope").unwrap(),
            None
        );

        let profiles = browser_profiles(home.path());
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].browser.id, "google-chrome-canary");
        assert!(profiles[0].in_use());

        let scores = site_scores(Engine::Chromium, &profile.join("History")).unwrap();
        let mut ranked: Vec<_> = scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        let ranked: Vec<_> = ranked.into_iter().map(|(h, _)| h).collect();
        assert_eq!(
            ranked,
            [
                "often.example",
                "long-ago.example",
                "once.example",
                "elsewhere.example"
            ]
        );

        let mut icons =
            site_icons(Engine::Chromium, &profile.join("Favicons"), "often.example").unwrap();
        icons.sort();
        assert_eq!(icons, [(16, vec![1]), (32, vec![2])]);
        assert!(
            site_icons(Engine::Chromium, &profile.join("Favicons"), "once.example")
                .unwrap()
                .is_empty()
        );
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

        let badged = Favicons::new(dir.path())
            .badged_icon(&icon, &icon, "job")
            .unwrap();
        let image = raster::decode(&std::fs::read(badged).unwrap()).unwrap();
        // The gap around the badge is transparent; the badge itself is not.
        assert_eq!(image.get_pixel(100, 150)[3], 0);
        assert_eq!(image.get_pixel(160, 160)[3], 255);
        assert_eq!(image.get_pixel(20, 20)[3], 255);
    }
}
