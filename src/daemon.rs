//! Workspace and titlebar icons for i3 and Sway.
//!
//! All installed programs' icons are baked into a custom font. From the next
//! login on, the daemon uses it to set workspace names and window titles on
//! window events.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::ValueEnum;
use regex::Regex;

use crate::assets::placeholder_icon_path;
use crate::desktop::{self, corrected_name};
use crate::favicons::{self, FirefoxFavicons, favicon_program, line_icon, stacked_variants};
use crate::font_builder::{self, FontBuilder};
use crate::icon_map::{
    FAVICON_PREFIX, FAVICON_PUA_START, PLACEHOLDER_CODEPOINT, PUA_START, ProgramIconEntry,
    ProgramIconMap,
};
use crate::ipc::{Ipc, Node};
use crate::platform::{Compositor, FontInstaller, program_name};
use crate::xdg::APP_NAME;

pub const DEFAULT_FONT_FAMILY_NAME: &str = "WorkspaceIconDaemon";
/// Half-size top/bottom/middle copies of each icon, for stacking: slots
/// 0-1023 mirror the application range, the rest mirror favicons.
const STACK_TOP_START: u32 = 0x108000;
const STACK_BOTTOM_START: u32 = 0x10B000;
const STACK_MIDDLE_START: u32 = 0x10E000;
const STACK_SLOTS: u32 = 0x1FF0; // The middle range is the smallest.
/// Zero-width layout lines drawn over icons: under each tabbed icon, above
/// each stacked column, and between the halves of a vertical split's column.
const TAB_UNDERLINE_CODEPOINT: u32 = 0x10FFF0;
const STACK_OVERLINE_CODEPOINT: u32 = 0x10FFF1;
const SPLIT_LINE_CODEPOINT: u32 = 0x10FFF2;
const TAB_UNDERLINE_DROP: f64 = 0.12;
/// Background behind the focused window's icon in layout titles.
const FOCUS_HIGHLIGHT: &str = "#719cd666";
const NEUTRAL_COLOR: &str = "#888888";

fn layout_color(layout: &str) -> &'static str {
    match layout {
        "splith" => "#719cd6",
        "splitv" => "#81b29a",
        "stacked" => "#f4a261",
        "tabbed" => "#c58fff",
        _ => NEUTRAL_COLOR,
    }
}

/// Separators between the parts of a split; tabbed icons sit side by side
/// over a line and stacked ones on top of each other under a line.
fn layout_separator(layout: &str) -> &'static str {
    match layout {
        "splith" => "|",
        "splitv" => "—",
        "tabbed" => "",
        _ => " ",
    }
}

/// The icon font centres glyphs a little above where titles centre text,
/// which clips the top of a stacked pair; this lowers stacking glyphs (in ems).
const STACK_DROP: f64 = 0.08;
/// Bump when glyph layout changes without the set of icons changing, so the
/// installed font gets rebuilt. Stored as the font's version string.
pub const FONT_LAYOUT_VERSION: &str = "workspace-icon-daemon layout 5";
/// Title markup sizes relative to the title text (icons, layout symbols) and
/// to the compositor's title font (stacked icon pairs, which fill its line).
const DEFAULT_TITLE_FONT_SIZE: f64 = 10.0;
const ICON_SCALE: f64 = 1.4;
const STACK_SCALE: f64 = 1.3;
/// How many of the most visited sites get a favicon baked into the font up front.
const FAVICON_TOP_SITES: usize = 300;
/// Delay before rebuilding the font for newly seen sites, to batch them.
const FAVICON_REBUILD_DELAY: Duration = Duration::from_secs(120);
const FAVICON_BADGE_PROGRAMS: [&str; 3] = ["org.mozilla.firefox", "firefox", "firefox-esr"];
/// Terminals whose foreground job (e.g. nvim) gets its own icon, badged with
/// the terminal's.
const TERMINAL_PROGRAMS: [&str; 6] = [
    "Alacritty",
    "foot",
    "kitty",
    "org.wezfurlong.wezterm",
    "com.mitchellh.ghostty",
    "org.gnome.Ptyxis",
];
const JOB_PREFIX: &str = "job:";
/// Jobs added to the font up front, so they show from the next login on.
const PRESET_JOBS: [&str; 5] = ["nvim", "claude", "claude@1", "claude@2", "claude@3"];
/// Jobs that show a spinner as the first character of the window title while
/// working; their icon turns a quarter turn (one frame) each time it changes.
fn job_spinner(job: &str) -> &'static [char] {
    match job {
        "claude" => &['◐', '◓', '◑', '◒'],
        _ => &[],
    }
}
const JOB_FRAMES: u64 = 4;
pub const ANIMATION_FRAME: Duration = Duration::from_millis(500);
/// Jobs without an installed application icon, shown with a site's favicon.
fn job_favicon_site(job: &str) -> Option<&'static str> {
    match job {
        "claude" => Some("claude.ai"),
        _ => None,
    }
}

const IGNORED_PROGRAMS: [&str; 10] = [
    "fzf", "tmux", "screen", "vim", "nano", "htop", "btop", "less", "man", "ssh",
];
const SUPERSCRIPT_DIGITS: [char; 10] = ['⁰', '¹', '²', '³', '⁴', '⁵', '⁶', '⁷', '⁸', '⁹'];
const SUBSCRIPT_DIGITS: [char; 10] = ['₀', '₁', '₂', '₃', '₄', '₅', '₆', '₇', '₈', '₉'];

/// Fonts are written by one build at a time.
static BUILD_LOCK: Mutex<()> = Mutex::new(());
static CLOCK_START: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Codepoints of an icon's top, bottom and middle stacking glyphs.
pub fn stacked_codepoints(codepoint: u32) -> Option<(u32, u32, u32)> {
    let slot = if codepoint >= FAVICON_PUA_START {
        1024 + (codepoint - FAVICON_PUA_START) as i64
    } else {
        codepoint as i64 - PUA_START as i64
    };
    if !(0..STACK_SLOTS as i64).contains(&slot) {
        return None;
    }
    let slot = slot as u32;
    Some((
        STACK_TOP_START + slot,
        STACK_BOTTOM_START + slot,
        STACK_MIDDLE_START + slot,
    ))
}

/// How repeated icons in a workspace are shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum UniqueIconsMode {
    /// Show all icons including duplicates.
    #[value(name = "nonunique")]
    Nonunique,
    /// One icon per program with its count in superscript.
    #[value(name = "numbers_superscript")]
    NumbersSuperscript,
    /// One icon per program with its count in subscript.
    #[value(name = "numbers_subscript")]
    NumbersSubscript,
    /// Only unique icons, without counts.
    #[value(name = "unique")]
    Unique,
}

pub struct WorkspaceInfo {
    pub num: i32,
    pub name: String,
    pub programs: Vec<String>,
}

fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#x27;"),
            _ => escaped.push(c),
        }
    }
    escaped
}

/// Format a number like Python's "{:g}": six significant digits, no
/// trailing zeros.
fn format_g(value: f64) -> String {
    if value == 0.0 {
        return "0".to_string();
    }
    let decimals = (5 - value.abs().log10().floor() as i32).max(0) as usize;
    let text = format!("{value:.decimals$}");
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        text
    }
}

fn glyph(codepoint: u32) -> String {
    format!("&#x{codepoint:X};")
}

/// Send a best-effort desktop notification.
pub fn notify(summary: &str, body: &str) {
    let result = std::process::Command::new("notify-send")
        .args(["--app-name", APP_NAME, summary, body])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if let Err(error) = result {
        log::warn!("Could not send desktop notification: {error}");
    }
}

/// Everything needed to build and install the font, detached from the
/// daemon so it can run without holding its lock.
pub struct FontJob {
    icons: Vec<(PathBuf, u32)>,
    base_font: &'static [u8],
    output: PathBuf,
    family: String,
    installer: FontInstaller,
}

impl FontJob {
    pub fn run(self) -> Result<()> {
        let _build = BUILD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        create_icon_font(&self.icons, self.base_font, &self.output, &self.family)?;
        self.installer.install(&self.output)?;
        Ok(())
    }
}

/// Build the icon font: the placeholder, every program's icon, the layout
/// lines and the half-size stacking copies of every icon.
pub fn create_icon_font(
    icons: &[(PathBuf, u32)],
    base_font: &[u8],
    output: &Path,
    family: &str,
) -> Result<()> {
    log::debug!("Creating icon font...");
    // Every generated font has a stable fallback glyph. It is used during
    // the session in which a newly discovered application's real glyph has
    // been built but cannot yet be loaded by the current renderers.
    let placeholder = placeholder_icon_path();
    let mut all_icons = vec![(placeholder.clone(), PLACEHOLDER_CODEPOINT)];
    all_icons.extend(icons.iter().cloned());
    let mut paths: Vec<PathBuf> = all_icons.iter().map(|(path, _)| path.clone()).collect();
    let mut codepoints: Vec<u32> = all_icons.iter().map(|(_, cp)| *cp).collect();
    let mut advances = vec![1.0; codepoints.len()];
    let mut drops = vec![0.0; codepoints.len()];

    let stacked_dir = output.parent().unwrap_or(Path::new(".")).join("stacked");
    for (position, layout, codepoint, drop) in [
        (
            "under",
            "tabbed",
            TAB_UNDERLINE_CODEPOINT,
            TAB_UNDERLINE_DROP,
        ),
        ("over", "stacked", STACK_OVERLINE_CODEPOINT, STACK_DROP),
        ("between", "splitv", SPLIT_LINE_CODEPOINT, STACK_DROP),
    ] {
        paths.push(line_icon(&stacked_dir, position, layout_color(layout))?);
        codepoints.push(codepoint);
        advances.push(0.0);
        drops.push(drop);
    }
    let variants = crate::parallel_map(&all_icons, |(icon, codepoint)| {
        let stacked = stacked_codepoints(*codepoint)?;
        let variants = stacked_variants(icon, &stacked_dir, &format!("{codepoint:X}"))?;
        Some((stacked, variants))
    });
    for ((top, bottom, middle), variants) in variants.into_iter().flatten() {
        paths.extend(variants);
        codepoints.extend([top, bottom, middle]);
        // The top half takes no space, so the bottom half draws under it.
        advances.extend([0.0, 0.5, 0.5]);
        drops.extend([STACK_DROP; 3]);
    }

    let builder = FontBuilder {
        family_name: family.to_string(),
        pua_start: PUA_START,
        remove_original_symbols: true,
        codepoints: Some(codepoints),
        fallback_image: Some(placeholder),
        advance_fractions: Some(advances),
        drop_fractions: Some(drops),
        version: Some(FONT_LAYOUT_VERSION.to_string()),
    };
    let built = builder.build(base_font, &paths)?;
    let directory = output.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(directory)?;
    let temporary = tempfile::NamedTempFile::new_in(directory)?;
    std::fs::write(temporary.path(), &built.data)?;
    temporary.persist(output)?;
    log::info!(
        "Wrote {} with {} glyphs",
        output.display(),
        built.codepoints.len()
    );
    Ok(())
}

pub struct Settings {
    pub compositor: Compositor,
    pub program_icon_map_path: PathBuf,
    pub base_font: &'static [u8],
    pub font_output_path: PathBuf,
    pub font_family_name: String,
    pub unique_icons_mode: UniqueIconsMode,
    pub use_placeholder_icon: bool,
    pub workspace_icons: bool,
    pub titlebar_icons: bool,
    /// Title text is drawn at this size (pt), so the compositor's title font
    /// can be set larger to make titlebars taller for the icons.
    pub title_text_size: Option<f64>,
    pub fonts_dir: PathBuf,
}

/// Manages application icons in i3 or Sway workspace names and titlebars.
pub struct Daemon {
    ipc: Box<dyn Ipc>,
    pub settings: Settings,
    pub program_icon_map: ProgramIconMap,
    pub font_installer: FontInstaller,
    titlebar_icon_codepoints: HashMap<i64, u32>,
    split_container_formats: HashMap<i64, String>,
    stacking_available: bool,
    animating: bool,
    compositor_font_size: Option<f64>,
    favicons: FirefoxFavicons,
    rebuild_requests: Option<Sender<()>>,
    pub notifier: fn(&str, &str),
    /// The glyphs in the font this session loaded. Installing a replacement
    /// font does not make its glyphs available to the current session.
    active_program_codepoints: HashMap<String, u32>,
    active_placeholder_available: bool,
}

impl Daemon {
    pub fn new(ipc: Box<dyn Ipc>, settings: Settings) -> Result<Self> {
        let program_icon_map = ProgramIconMap::load(&settings.program_icon_map_path)?;
        let cache_dir = settings
            .font_output_path
            .parent()
            .unwrap_or(Path::new("."))
            .to_path_buf();
        Ok(Self {
            ipc,
            program_icon_map,
            font_installer: FontInstaller {
                fonts_dir: settings.fonts_dir.clone(),
            },
            titlebar_icon_codepoints: HashMap::new(),
            split_container_formats: HashMap::new(),
            stacking_available: false,
            animating: false,
            compositor_font_size: None,
            favicons: FirefoxFavicons::new(&cache_dir),
            rebuild_requests: None,
            notifier: notify,
            active_program_codepoints: HashMap::new(),
            active_placeholder_available: false,
            settings,
        })
    }

    fn window_name(&self, window: &Node) -> Option<String> {
        program_name(window, self.settings.compositor).map(|name| corrected_name(name).to_string())
    }

    fn is_ignored(program: Option<&str>) -> bool {
        program.is_some_and(|p| IGNORED_PROGRAMS.contains(&p))
    }

    /// Windows sorted by visual position: top to bottom, then left to right.
    fn sort_windows_by_layout(windows: &mut [&Node]) {
        windows.sort_by_key(|w| (w.rect.y.div_euclid(10), w.rect.x));
    }

    pub fn programs_by_workspace(&self, tree: &Node) -> Vec<WorkspaceInfo> {
        tree.workspaces()
            .into_iter()
            .map(|workspace| {
                let mut windows = workspace.leaves();
                Self::sort_windows_by_layout(&mut windows);
                let programs = windows
                    .iter()
                    .filter_map(|w| self.window_name(w))
                    .filter(|p| !Self::is_ignored(Some(p)))
                    .collect();
                WorkspaceInfo {
                    num: workspace.num.unwrap_or(-1),
                    name: workspace.name().to_string(),
                    programs,
                }
            })
            .collect()
    }

    /// Add every application represented by an installed desktop entry.
    pub fn discover_installed_programs(&mut self) -> Result<bool> {
        // The first entry with an id wins, in XDG precedence order.
        let mut desktop_files: Vec<(String, PathBuf)> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for directory in desktop::desktop_application_paths() {
            for path in desktop::files_with_extension(&directory, "desktop") {
                let id = path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                if seen.insert(id.clone()) {
                    desktop_files.push((id, path));
                }
            }
        }

        let mut added_any = false;
        let mut icon_index: Option<HashMap<String, PathBuf>> = None;
        let placeholder = placeholder_icon_path();
        for (desktop_id, desktop_file) in &desktop_files {
            let mut identifiers = vec![desktop_id.clone(), corrected_name(desktop_id).to_string()];
            if let Some(class) = desktop::parse_desktop_value(desktop_file, "StartupWMClass") {
                identifiers.push(corrected_name(&class).to_string());
                identifiers.push(class);
            }
            let mut missing: Vec<String> = Vec::new();
            for identifier in identifiers {
                if !missing.contains(&identifier) && !self.program_icon_map.contains(&identifier) {
                    missing.push(identifier);
                }
            }
            if missing.is_empty() {
                continue;
            }
            let index = icon_index.get_or_insert_with(desktop::installed_icon_index);
            let icon_name = desktop::parse_desktop_value(desktop_file, "Icon");
            let mut icon_path = desktop::resolve_icon_from_index(icon_name.as_deref(), index);
            if icon_path.is_none() && self.settings.use_placeholder_icon {
                icon_path = Some(placeholder.clone());
            }
            for program in missing {
                let (added, _) = self
                    .program_icon_map
                    .add_program(&program, icon_path.as_deref())?;
                added_any |= added;
            }
        }
        if added_any {
            self.program_icon_map.save()?;
        }
        log::info!("Discovered {} installed applications", desktop_files.len());
        Ok(added_any)
    }

    fn add_missing_programs(&mut self, missing: &[String]) -> Result<bool> {
        let mut added_any = false;
        for program in missing {
            let mut icon_path = desktop::find_icon_for_program(program);
            if icon_path.is_none() {
                if self.settings.use_placeholder_icon {
                    icon_path = Some(placeholder_icon_path());
                    log::warn!("Could not find icon for program: {program}, using placeholder");
                } else {
                    log::warn!("Could not find icon for program: {program}, tracking without icon");
                }
            }
            let (added, _) = self
                .program_icon_map
                .add_program(program, icon_path.as_deref())?;
            added_any |= added;
        }
        Ok(added_any)
    }

    /// Discover programs represented by currently open windows.
    pub fn add_running_programs(&mut self) -> Result<bool> {
        let tree = self.ipc.get_tree()?;
        let programs: std::collections::BTreeSet<String> = self
            .programs_by_workspace(&tree)
            .into_iter()
            .flat_map(|w| w.programs)
            .collect();
        let missing: Vec<String> = programs
            .into_iter()
            .filter(|p| !self.program_icon_map.contains(p))
            .collect();
        if missing.is_empty() {
            return Ok(false);
        }
        let added = self.add_missing_programs(&missing)?;
        if added {
            self.program_icon_map.save()?;
        }
        Ok(added)
    }

    /// Check open windows for new programs, and install a font with their
    /// icons for the next session.
    pub fn process_new_programs(&mut self) -> Result<bool> {
        if !self.add_running_programs()? {
            log::debug!("No new programs detected; skipping font rebuild");
            return Ok(false);
        }
        self.publish_font_update(true)?;
        Ok(true)
    }

    pub fn font_job(&self) -> FontJob {
        FontJob {
            icons: self.program_icon_map.icons(),
            base_font: self.settings.base_font,
            output: self.settings.font_output_path.clone(),
            family: self.settings.font_family_name.clone(),
            installer: self.font_installer.clone(),
        }
    }

    /// Build and install a font which will become active next session.
    pub fn publish_font_update(&mut self, new_application: bool) -> Result<()> {
        self.font_job().run()?;
        if new_application {
            (self.notifier)(
                "WorkspaceIconDaemon: New application icon installed",
                "Log out and back in again for the new application icon to be correctly shown",
            );
        }
        Ok(())
    }

    /// A glyph guaranteed to exist in the font loaded this session.
    fn active_unicode_id(&self, program: &str) -> Option<u32> {
        if let Some(&codepoint) = self.active_program_codepoints.get(program) {
            return Some(codepoint);
        }
        (self.settings.use_placeholder_icon && self.active_placeholder_available)
            .then_some(PLACEHOLDER_CODEPOINT)
    }

    fn rename_workspace(&mut self, old: &str, new: &str) -> Result<()> {
        let old = old.replace('"', "\\\"");
        let new = new.replace('"', "\\\"");
        self.ipc
            .command(&format!("rename workspace \"{old}\" to \"{new}\""))
    }

    /// Update all workspace names with their windows' icons.
    pub fn update_workspace_names(&mut self) -> Result<()> {
        if !self.settings.workspace_icons {
            return Ok(());
        }
        let tree = self.ipc.get_tree()?;
        for workspace in tree.workspaces() {
            let mut windows = workspace.leaves();
            Self::sort_windows_by_layout(&mut windows);
            let mut icons: Vec<String> = Vec::new();
            for window in windows {
                if Self::is_ignored(self.window_name(window).as_deref()) {
                    continue;
                }
                if let Some(codepoint) = self.window_unicode_id(window)
                    && let Some(c) = char::from_u32(codepoint)
                {
                    icons.push(c.to_string());
                }
            }
            let processed = self.process_icons(icons);
            let new_name = construct_workspace_name(
                workspace.num.unwrap_or(-1),
                &processed,
                Some(&self.workspace_base_name(workspace.name())),
            );
            if new_name != workspace.name() {
                self.rename_workspace(workspace.name(), &new_name)?;
            }
        }
        Ok(())
    }

    /// Name of the job in the foreground of a terminal window's shell.
    fn terminal_job(window: &Node) -> Option<String> {
        let pid = window.pid?;
        let mut tasks: Vec<u64> = std::fs::read_dir(format!("/proc/{pid}/task"))
            .ok()?
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.parse().ok())
            .collect();
        tasks.sort();
        let mut children = tasks.iter().flat_map(|task| {
            std::fs::read_to_string(format!("/proc/{pid}/task/{task}/children"))
                .unwrap_or_default()
                .split_whitespace()
                .filter_map(|c| c.parse::<i64>().ok())
                .collect::<Vec<_>>()
        });
        let shell = children.next()?;
        let stat = std::fs::read_to_string(format!("/proc/{shell}/stat")).ok()?;
        // comm may contain spaces and parentheses; fields resume after the
        // last ')': state, ppid, pgrp, session, tty_nr, tpgid.
        let foreground: i64 = stat
            .get(stat.rfind(')')? + 2..)?
            .split_whitespace()
            .nth(5)?
            .parse()
            .ok()?;
        if foreground == shell || foreground == -1 {
            return None;
        }
        let comm = std::fs::read_to_string(format!("/proc/{foreground}/comm")).ok()?;
        let comm = comm.trim();
        (!comm.is_empty()).then(|| comm.to_string())
    }

    /// Animation frame of a working job's icon, or 0 when it is idle.
    ///
    /// Frames come from the clock rather than from the title's spinner,
    /// which e.g. Claude Code stops updating while its terminal is unfocused.
    fn spinner_frame(&mut self, window: &Node, job: &str) -> u64 {
        let first = window.name().chars().next();
        if !first.is_some_and(|c| job_spinner(job).contains(&c)) {
            return 0;
        }
        self.animating = true;
        (CLOCK_START.elapsed().as_millis() / ANIMATION_FRAME.as_millis()) as u64 % JOB_FRAMES
    }

    /// Redraw icons for the next animation frame while any job is working.
    pub fn animation_tick(&mut self) {
        if !self.animating {
            return;
        }
        self.animating = false; // Set again by any still-working job.
        if let Err(error) = self
            .update_workspace_names()
            .and_then(|_| self.update_window_titles())
        {
            log::debug!("Animation update failed: {error:#}");
        }
    }

    /// A job's icon badged with its terminal's icon. `job@N` is animation
    /// frame N: the icon turned by N quarter turns.
    fn job_icon(&mut self, job: &str, terminal: &str) -> Option<PathBuf> {
        let (job, frame) = job.split_once('@').unwrap_or((job, ""));
        let badge = self.program_icon_map.get_icon_path(terminal)?.to_path_buf();
        if let Some(site) = job_favicon_site(job) {
            let frame: u32 = frame.parse().unwrap_or(0);
            return self.favicons.export_icon(site, Some(&badge), 90 * frame);
        }
        let icon = desktop::find_icon_for_program(job)?;
        self.favicons.badged_icon(&icon, &badge, job)
    }

    /// Add a terminal job's icon; returns whether a new glyph is needed.
    fn add_job(&mut self, job: &str, terminal: &str) -> Result<bool> {
        let program = format!("{JOB_PREFIX}{job}");
        if self.program_icon_map.contains(&program) {
            return Ok(false);
        }
        let icon_path = self.job_icon(job, terminal);
        self.program_icon_map
            .add_program(&program, icon_path.as_deref())?;
        self.program_icon_map.save()?;
        Ok(icon_path.is_some())
    }

    pub fn add_preset_jobs(&mut self) -> Result<bool> {
        // Badge with the terminal in use, or else the first one installed.
        let tree = self.ipc.get_tree()?;
        let running: HashSet<String> = tree
            .leaves()
            .iter()
            .filter_map(|w| self.window_name(w))
            .collect();
        let mut terminals = TERMINAL_PROGRAMS.to_vec();
        terminals.sort_by_key(|t| !running.contains(*t));
        let Some(terminal) = terminals
            .into_iter()
            .find(|t| self.program_icon_map.get_icon_path(t).is_some())
        else {
            return Ok(false);
        };
        let mut added = false;
        for job in PRESET_JOBS {
            added |= self.add_job(job, terminal)?;
        }
        Ok(added)
    }

    /// The window's site favicon if it is a browser on a known site, the
    /// icon of its foreground job if it is a terminal, otherwise its
    /// application icon.
    fn window_unicode_id(&mut self, window: &Node) -> Option<u32> {
        let program = self.window_name(window)?;
        if TERMINAL_PROGRAMS.contains(&program.as_str())
            && let Some(mut job) = Self::terminal_job(window)
        {
            let frame = self.spinner_frame(window, &job);
            // Frames missing from the loaded font fall back to the icon at rest.
            if frame != 0
                && self
                    .active_program_codepoints
                    .contains_key(&format!("{JOB_PREFIX}{job}@{frame}"))
            {
                job = format!("{job}@{frame}");
            }
            if let Some(&codepoint) = self
                .active_program_codepoints
                .get(&format!("{JOB_PREFIX}{job}"))
            {
                return Some(codepoint);
            }
            match self.add_job(&job, &program) {
                Ok(true) => self.schedule_font_rebuild(),
                Ok(false) => {}
                Err(error) => log::warn!("Could not add job {job}: {error:#}"),
            }
        }
        if favicons::is_browser(Some(&program))
            && let Some(host) = self
                .favicons
                .host_for_window(&program, window.name.as_deref())
        {
            let favicon = favicon_program(&host);
            if let Some(&codepoint) = self.active_program_codepoints.get(&favicon) {
                return Some(codepoint);
            }
            if !self.program_icon_map.contains(&favicon) {
                self.add_favicon_later(&host);
            }
        }
        self.active_unicode_id(&program)
    }

    /// Icon overlaid on favicons to show which browser a window is.
    fn favicon_badge(&self) -> Option<PathBuf> {
        FAVICON_BADGE_PROGRAMS
            .iter()
            .find_map(|p| self.program_icon_map.get_icon_path(p))
            .map(Path::to_path_buf)
    }

    /// Add or refresh a site's favicon; returns whether its glyph changed.
    fn add_favicon(&mut self, host: &str) -> Result<bool> {
        let program = favicon_program(host);
        let entry = self.program_icon_map.programs.get(&program).cloned();
        if entry.as_ref().is_some_and(|e| e.icon_path.is_none()) {
            return Ok(false); // Known to have no favicon.
        }
        let badge = self.favicon_badge();
        let icon_path = self.favicons.export_icon(host, badge.as_deref(), 0);
        if let Some(entry) = entry {
            if icon_path.is_none() || icon_path == entry.icon_path {
                return Ok(false);
            }
            self.program_icon_map.programs.insert(
                program,
                ProgramIconEntry {
                    icon_path,
                    unicode_id: entry.unicode_id,
                },
            );
            return Ok(true);
        }
        // Sites without a favicon are remembered so they are not looked up again.
        self.program_icon_map
            .add_program(&program, icon_path.as_deref())?;
        Ok(icon_path.is_some())
    }

    /// Add favicons of the most visited sites, and refresh known ones.
    pub fn add_top_favicons(&mut self) -> Result<bool> {
        if !self.settings.titlebar_icons || !self.favicons.available() {
            return Ok(false);
        }
        let mut hosts = self.favicons.top_hosts(FAVICON_TOP_SITES);
        for program in self.program_icon_map.programs.keys() {
            if let Some(host) = program.strip_prefix(FAVICON_PREFIX)
                && !hosts.iter().any(|h| h == host)
            {
                hosts.push(host.to_string());
            }
        }
        let mut changed = 0;
        for host in hosts {
            changed += self.add_favicon(&host)? as usize;
        }
        self.program_icon_map.save()?;
        log::info!("Added or updated {changed} site favicons");
        Ok(changed > 0)
    }

    /// Add a newly seen site's favicon to the font for the next session.
    ///
    /// Deliberately silent: until then the browser icon is shown, and the
    /// favicon simply appears after some later login.
    fn add_favicon_later(&mut self, host: &str) {
        let added = self.add_favicon(host);
        if let Err(error) = self.program_icon_map.save() {
            log::warn!("{error:#}");
        }
        match added {
            Ok(true) => self.schedule_font_rebuild(),
            Ok(false) => {}
            Err(error) => log::warn!("Could not add favicon for {host}: {error:#}"),
        }
    }

    /// Quietly rebuild the font for the next session, batching additions.
    fn schedule_font_rebuild(&mut self) {
        if let Some(requests) = &self.rebuild_requests {
            let _ = requests.send(());
        }
    }

    fn icon_markup(&self, codepoint: u32, family: &str, underline: bool, focused: bool) -> String {
        let line = if underline {
            glyph(TAB_UNDERLINE_CODEPOINT)
        } else {
            String::new()
        };
        let highlight = if focused {
            format!(" background='{FOCUS_HIGHLIGHT}'")
        } else {
            String::new()
        };
        format!(
            "<span font_family='{family}' size='{}'{highlight}>{line}{}</span>",
            self.icon_size(),
            glyph(codepoint)
        )
    }

    /// Apply each window's icon to its title.
    ///
    /// The generated font contains only icon glyphs, so the font family is
    /// scoped to the icon span; the ordinary %title text keeps the
    /// compositor's configured title font.
    pub fn update_window_titles(&mut self) -> Result<()> {
        if !self.settings.titlebar_icons {
            return Ok(());
        }
        self.title_font_size();
        let family = escape(&self.settings.font_family_name);
        let tree = self.ipc.get_tree()?;
        let mut visible: HashSet<i64> = HashSet::new();
        for window in tree.leaves() {
            let Some(codepoint) = self.window_unicode_id(window) else {
                continue;
            };
            visible.insert(window.id);
            if self.titlebar_icon_codepoints.get(&window.id) == Some(&codepoint) {
                continue;
            }
            // A leading zero-width space keeps the line metrics from the
            // title font; otherwise Sway ignores the icon span's size.
            let title_format = format!(
                "&#x200B;<span font_family='{family}' size='{}'>{}</span> {}",
                self.icon_size(),
                glyph(codepoint),
                self.title_text("%title")
            );
            // Record this before sending the command: changing title_format
            // may itself cause a window::title event on some compositor versions.
            self.titlebar_icon_codepoints.insert(window.id, codepoint);
            self.ipc.command(&format!(
                "[con_id={}] title_format \"{title_format}\"",
                window.id
            ))?;
        }
        self.titlebar_icon_codepoints
            .retain(|id, _| visible.contains(id));
        self.update_split_container_titles(&family)
    }

    /// The non-window containers nested inside workspaces.
    fn split_containers(&mut self) -> Result<Vec<Node>> {
        let tree = self.ipc.get_tree()?;
        Ok(tree
            .workspaces()
            .into_iter()
            .flat_map(|w| w.descendants())
            .filter(|con| con.kind == "con" && !con.nodes.is_empty())
            .cloned()
            .collect())
    }

    /// Render a container's layout with its windows' icons: `A|B` side by
    /// side, `A—B` top and bottom, tabbed icons over a line and stacked icons
    /// on top of each other under a line. Nested groups get brackets.
    fn container_representation(
        &mut self,
        con: &Node,
        family: &str,
        nested: bool,
        underline: bool,
    ) -> String {
        if con.nodes.is_empty() {
            return match self.window_unicode_id(con) {
                Some(codepoint) => self.icon_markup(codepoint, family, underline, con.focused),
                None => escape(self.window_name(con).as_deref().unwrap_or("?")),
            };
        }
        if con.nodes.len() == 1 {
            return self.container_representation(&con.nodes[0], family, nested, underline);
        }
        if let Some(stacked) = self.stacked_icons(con, family) {
            return stacked;
        }
        let mut separator = layout_separator(&con.layout).to_string();
        if !separator.trim().is_empty() {
            separator = format!(
                "<span foreground='{}' weight='bold' size='{}'>{separator}</span>",
                layout_color(&con.layout),
                self.text_size()
            );
        }
        let underline = con.layout == "tabbed" && self.stacking_available;
        let parts: Vec<String> = con
            .nodes
            .iter()
            .map(|child| self.container_representation(child, family, true, underline))
            .collect();
        let body = parts.join(&separator);
        if !nested {
            return body;
        }
        let bracket = format!(
            "<span foreground='{NEUTRAL_COLOR}' size='{}'>",
            self.text_size()
        );
        format!("{bracket}[</span>{body}{bracket}]</span>")
    }

    /// Icons of a vertical split's or stacked layout's windows on top of
    /// each other, or None if that can't be shown: the layout holds
    /// containers, a vertical split has more than two windows (columns would
    /// read as a horizontal split), or the loaded font can't stack.
    fn stacked_icons(&mut self, con: &Node, family: &str) -> Option<String> {
        if !matches!(con.layout.as_str(), "splitv" | "stacked") || !self.stacking_available {
            return None;
        }
        if con.nodes.iter().any(|child| !child.nodes.is_empty()) {
            return None;
        }
        if con.layout == "splitv" && con.nodes.len() != 2 {
            return None;
        }
        let mut stacked = Vec::new();
        for child in &con.nodes {
            stacked.push(stacked_codepoints(self.window_unicode_id(child)?)?);
        }
        let over = if con.layout == "stacked" {
            glyph(STACK_OVERLINE_CODEPOINT)
        } else {
            String::new()
        };
        let between = if con.layout == "splitv" {
            glyph(SPLIT_LINE_CODEPOINT)
        } else {
            String::new()
        };
        let mut columns = String::new();
        for index in (0..stacked.len()).step_by(2) {
            let mut column = if index + 1 < stacked.len() {
                let (top, bottom) = (stacked[index].0, stacked[index + 1].1);
                format!("{over}{}{between}{}", glyph(top), glyph(bottom))
            } else {
                format!("{over}{}", glyph(stacked[index].2))
            };
            if con.nodes[index..(index + 2).min(con.nodes.len())]
                .iter()
                .any(|c| c.focused)
            {
                column = format!("<span background='{FOCUS_HIGHLIGHT}'>{column}</span>");
            }
            columns.push_str(&column);
        }
        Some(format!(
            "<span font_family='{family}' size='{}'>{columns}</span>",
            self.stack_size()
        ))
    }

    /// Size in pt of the compositor's title font, from its config.
    fn title_font_size(&mut self) -> f64 {
        if let Some(size) = self.compositor_font_size {
            return size;
        }
        static FONT_LINE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"(?m)^font\s+(.*?)\s*$").unwrap());
        static FONT_SIZE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"([\d.]+)\s*(px)?$").unwrap());
        let mut size = DEFAULT_TITLE_FONT_SIZE;
        let config = self.ipc.get_config().unwrap_or_default();
        if let Some(line) = FONT_LINE.captures(&config)
            && let Some(found) = FONT_SIZE.captures(&line[1])
            && let Ok(points) = found[1].parse::<f64>()
        {
            size = if found.get(2).is_some() {
                points * 0.75
            } else {
                points
            };
        }
        self.compositor_font_size = Some(size);
        size
    }

    fn base_text_size(&self) -> f64 {
        self.settings
            .title_text_size
            .or(self.compositor_font_size)
            .unwrap_or(DEFAULT_TITLE_FONT_SIZE)
    }

    fn text_size(&self) -> String {
        format!("{}pt", format_g(self.base_text_size()))
    }

    fn title_text(&self, text: &str) -> String {
        match self.settings.title_text_size {
            None => text.to_string(),
            Some(_) => format!("<span size='{}'>{text}</span>", self.text_size()),
        }
    }

    fn icon_size(&self) -> String {
        format!("{}pt", format_g(self.base_text_size() * ICON_SCALE))
    }

    fn stack_size(&self) -> String {
        // Stacked pairs fill the line the compositor's (possibly larger)
        // title font makes room for.
        let font = self.compositor_font_size.unwrap_or(DEFAULT_TITLE_FONT_SIZE);
        format!("{}pt", format_g(font * STACK_SCALE))
    }

    /// Replace Sway's `H[app app]` split container titles with icons.
    fn update_split_container_titles(&mut self, family: &str) -> Result<()> {
        let mut formats: HashMap<i64, String> = HashMap::new();
        for con in self.split_containers()? {
            let title_format = self.container_representation(&con, family, false, false);
            if self.split_container_formats.get(&con.id) != Some(&title_format) {
                self.ipc.command(&format!(
                    "[con_id={}] title_format \"{title_format}\"",
                    con.id
                ))?;
            }
            formats.insert(con.id, title_format);
        }
        self.split_container_formats = formats;
        Ok(())
    }

    fn process_icons(&self, icons: Vec<String>) -> Vec<String> {
        let mut unique: Vec<String> = Vec::new();
        for icon in &icons {
            if !unique.contains(icon) {
                unique.push(icon.clone());
            }
        }
        let digits = match self.settings.unique_icons_mode {
            UniqueIconsMode::Nonunique => return icons,
            UniqueIconsMode::Unique => return unique,
            UniqueIconsMode::NumbersSuperscript => &SUPERSCRIPT_DIGITS,
            UniqueIconsMode::NumbersSubscript => &SUBSCRIPT_DIGITS,
        };
        unique
            .into_iter()
            .map(|icon| {
                let count = icons.iter().filter(|i| **i == icon).count();
                if count > 1 {
                    let suffix: String = count
                        .to_string()
                        .chars()
                        .map(|d| digits[d.to_digit(10).unwrap() as usize])
                        .collect();
                    format!("{icon}{suffix}")
                } else {
                    icon
                }
            })
            .collect()
    }

    /// Every glyph this daemon may have put into a workspace name.
    fn icon_chars(&self) -> HashSet<char> {
        let mut chars: HashSet<char> = self
            .program_icon_map
            .programs
            .values()
            .filter_map(|e| char::from_u32(e.codepoint()?))
            .collect();
        chars.extend(char::from_u32(PLACEHOLDER_CODEPOINT));
        chars
    }

    fn workspace_base_name(&self, name: &str) -> String {
        workspace_base_name(name, &self.icon_chars())
    }

    /// What restoring the desktop needs, so it can be done without the
    /// daemon, e.g. while it is busy. The glyphs shown in a session are
    /// fixed at startup, so this stays valid.
    pub fn reset_plan(&self) -> ResetPlan {
        ResetPlan {
            icon_chars: self.icon_chars(),
            workspace_icons: self.settings.workspace_icons,
            titlebar_icons: self.settings.titlebar_icons,
        }
    }

    /// React to a window event, or a workspace event (no container).
    pub fn on_window_event(&mut self, change: &str, container: Option<&Node>) -> Result<()> {
        if matches!(change, "new" | "title")
            && favicons::is_browser(container.and_then(|c| self.window_name(c)).as_deref())
        {
            self.favicons.forget_window_titles();
        }
        if matches!(change, "new" | "close" | "move" | "title") {
            if let Err(error) = self.process_new_programs() {
                log::warn!("Could not add new programs: {error:#}");
            }
            self.update_workspace_names()?;
            self.update_window_titles()?;
        }
        Ok(())
    }

    /// Restore workspace names and title formats.
    pub fn reset_desktop_state(&mut self) -> Result<()> {
        self.reset_plan().apply(self.ipc.as_mut())?;
        self.titlebar_icon_codepoints.clear();
        self.split_container_formats.clear();
        Ok(())
    }

    /// Prepare the next-session font and report whether monitoring may start.
    ///
    /// The installed font is snapshotted before discovery; that snapshot is
    /// the only mapping used for the lifetime of this process.
    pub fn ensure_startup_font(&mut self) -> Result<bool> {
        let name = self
            .settings
            .font_output_path
            .file_name()
            .context("Font output has no file name")?;
        let destination = self.font_installer.fonts_dir.join(name);
        let session_font = self.session_font(&destination)?;
        let active_font_available = self.snapshot_active_font(&session_font);
        let map_was_repaired = self.program_icon_map.modified_at_load;
        let installed_added = self.discover_installed_programs()?;
        let running_added = self.add_running_programs()?;
        let mut favicons_changed = self.add_top_favicons()?;
        favicons_changed = self.add_preset_jobs()? || favicons_changed;

        let expected: HashSet<u32> = self
            .program_icon_map
            .programs
            .values()
            .filter_map(ProgramIconEntry::codepoint)
            .collect();
        let active: HashSet<u32> = self.active_program_codepoints.values().copied().collect();
        let installed_is_outdated = expected != active;

        if !active_font_available {
            log::info!("No usable preinstalled icon font; creating one for next login");
            self.publish_font_update(false)?;
            (self.notifier)(
                "WorkspaceIconDaemon: Icon font installed",
                "Log out and back in again to show application icons",
            );
            return Ok(false);
        }
        if installed_added || running_added || map_was_repaired {
            self.publish_font_update(true)?;
        } else if installed_is_outdated
            || favicons_changed
            || font_builder::font_version(&destination).as_deref() != Some(FONT_LAYOUT_VERSION)
        {
            // Only favicons changed; they show up quietly after a later login.
            self.publish_font_update(false)?;
        }
        self.program_icon_map.modified_at_load = false;
        Ok(true)
    }

    /// The font as it was when this compositor session started.
    ///
    /// The compositor keeps the font it loaded at login, while the installed
    /// file is replaced by rebuilds, so the first daemon start of a session
    /// keeps a copy for later restarts (e.g. on config reloads) to consult.
    fn session_font(&self, installed_font: &Path) -> Result<PathBuf> {
        static SESSION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\.(\d+)\.sock$").unwrap());
        let socket = ["SWAYSOCK", "I3SOCK"]
            .iter()
            .find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty()))
            .unwrap_or_default();
        let Some(session) = SESSION.captures(&socket) else {
            return Ok(installed_font.to_path_buf());
        };
        if !installed_font.is_file() {
            return Ok(installed_font.to_path_buf());
        }
        let session_dir = self
            .settings
            .font_output_path
            .parent()
            .unwrap_or(Path::new("."))
            .join("sessions");
        let session_font = session_dir.join(format!("{}.ttf", &session[1]));
        if !session_font.is_file() {
            std::fs::create_dir_all(&session_dir)?;
            for stale in std::fs::read_dir(&session_dir)?.flatten() {
                if stale.path().extension().is_some_and(|e| e == "ttf") {
                    let _ = std::fs::remove_file(stale.path());
                }
            }
            std::fs::copy(installed_font, &session_font)?;
        }
        Ok(session_font)
    }

    /// Capture the mappings actually present in the installed font.
    fn snapshot_active_font(&mut self, installed_font: &Path) -> bool {
        self.active_program_codepoints.clear();
        self.active_placeholder_available = false;
        self.stacking_available = false;
        if !installed_font.is_file() {
            return false;
        }
        let info = match font_builder::read_font_info(installed_font) {
            Ok(info) => info,
            Err(error) => {
                log::warn!("Cannot use installed icon font: {error:#}");
                return false;
            }
        };
        if info.family.as_deref() != Some(self.settings.font_family_name.as_str())
            || !info.bitmap_codepoints.contains(&PLACEHOLDER_CODEPOINT)
        {
            return false;
        }
        self.active_placeholder_available = true;
        // Stacking and layout-line glyphs move between layouts.
        self.stacking_available = info.version.as_deref() == Some(FONT_LAYOUT_VERSION);
        for (program, entry) in &self.program_icon_map.programs {
            if let Some(codepoint) = entry.codepoint()
                && info.bitmap_codepoints.contains(&codepoint)
            {
                self.active_program_codepoints
                    .insert(program.clone(), codepoint);
            }
        }
        true
    }
}

/// The workspace name without the icon suffix this daemon appended.
///
/// Only a trailing run of our own glyphs (with their count digits and
/// spaces) is removed, so whatever the user named the workspace survives.
fn workspace_base_name(name: &str, icon_chars: &HashSet<char>) -> String {
    let is_suffix = |c: char| {
        icon_chars.contains(&c)
            || SUBSCRIPT_DIGITS.contains(&c)
            || SUPERSCRIPT_DIGITS.contains(&c)
            || c == ' '
    };
    let base = name.trim_end_matches(is_suffix);
    if !name[base.len()..].chars().any(|c| icon_chars.contains(&c)) {
        return name.to_string(); // No icons of ours; leave e.g. trailing spaces alone.
    }
    base.to_string()
}

/// Restores default workspace names and window titles.
pub struct ResetPlan {
    icon_chars: HashSet<char>,
    workspace_icons: bool,
    titlebar_icons: bool,
}

impl ResetPlan {
    pub fn apply(&self, ipc: &mut dyn Ipc) -> Result<()> {
        let tree = ipc.get_tree()?;
        if self.titlebar_icons {
            for window in tree.leaves() {
                ipc.command(&format!("[con_id={}] title_format \"%title\"", window.id))?;
            }
            for workspace in tree.workspaces() {
                for con in workspace.descendants() {
                    if con.kind == "con" && !con.nodes.is_empty() {
                        ipc.command(&format!("[con_id={}] title_format \"%title\"", con.id))?;
                    }
                }
            }
        }
        if self.workspace_icons {
            for workspace in tree.workspaces() {
                let base = workspace_base_name(workspace.name(), &self.icon_chars);
                let new_name =
                    construct_workspace_name(workspace.num.unwrap_or(-1), &[], Some(&base));
                if new_name != workspace.name() {
                    let old = workspace.name().replace('"', "\\\"");
                    let new = new_name.replace('"', "\\\"");
                    ipc.command(&format!("rename workspace \"{old}\" to \"{new}\""))?;
                }
            }
        }
        Ok(())
    }
}

/// A workspace name from its base name and icons: "NUM: ICONS" for a bare
/// numbered workspace, otherwise "BASE ICONS", or the base name alone when
/// there are no icons.
pub fn construct_workspace_name(num: i32, icons: &[String], base_name: Option<&str>) -> String {
    static NUMBERED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d+:?$").unwrap());
    let icons = icons.concat();
    match base_name {
        Some(base) if !NUMBERED.is_match(base.trim()) => {
            if icons.is_empty() {
                base.to_string()
            } else {
                format!("{base} {icons}")
            }
        }
        _ => {
            let base = if num >= 0 {
                num.to_string()
            } else {
                base_name.unwrap_or("").trim_end_matches(':').to_string()
            };
            if icons.is_empty() {
                base
            } else {
                format!("{base}: {icons}")
            }
        }
    }
}

pub fn lock(daemon: &Mutex<Daemon>) -> std::sync::MutexGuard<'_, Daemon> {
    daemon.lock().unwrap_or_else(|e| e.into_inner())
}

/// Run the daemon until the compositor goes away.
pub fn run(
    daemon: Arc<Mutex<Daemon>>,
    connect_events: impl FnOnce() -> Result<crate::ipc::EventStream>,
) -> Result<()> {
    use crate::ipc::Event;
    log::info!("Starting workspace icon daemon...");
    let (requests, received) = mpsc::channel::<()>();
    lock(&daemon).rebuild_requests = Some(requests);
    {
        let mut guard = lock(&daemon);
        if !guard.ensure_startup_font()? {
            return Ok(());
        }
        guard.update_workspace_names()?;
        guard.update_window_titles()?;
    }
    let mut events = connect_events()?;
    let rebuilder = Arc::clone(&daemon);
    std::thread::spawn(move || {
        while received.recv().is_ok() {
            // Wait until requests stop arriving, then build once.
            loop {
                match received.recv_timeout(FAVICON_REBUILD_DELAY) {
                    Ok(()) => continue,
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            let job = lock(&rebuilder).font_job();
            if let Err(error) = job.run() {
                log::warn!("Font rebuild failed: {error:#}");
            }
        }
    });
    let animator = Arc::clone(&daemon);
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(ANIMATION_FRAME);
            lock(&animator).animation_tick();
        }
    });

    log::info!("Daemon is running. Press Ctrl+C to exit.");
    while let Some(event) = events.next_event()? {
        let mut guard = lock(&daemon);
        let result = match &event {
            Event::Window { change, container } => match change.as_str() {
                "focus" => guard.update_window_titles(),
                change => guard.on_window_event(change, Some(container)),
            },
            // Re-add icons right after the user renames a workspace. Our own
            // renames trigger this too, but then the name is already up to date.
            Event::Workspace { change } if change == "rename" => guard.update_workspace_names(),
            Event::Workspace { change } if change == "move" => guard.on_window_event(change, None),
            Event::Workspace { .. } => Ok(()),
            // Splits and layout changes emit no window event, only a binding event.
            Event::Binding => guard.update_window_titles(),
            Event::Shutdown => break,
        };
        if let Err(error) = result {
            log::warn!("Could not handle {event:?}: {error:#}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
