//! Workspace and titlebar icons for i3 and Sway.
//!
//! All installed programs' icons are baked into a custom font. From the next
//! login on, the daemon uses it to set workspace names and window titles on
//! window events.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::ValueEnum;
use regex::Regex;

use crate::assets::placeholder_icon_path;
use crate::desktop::{self, corrected_name};
use crate::favicons::{
    self, Browser, Favicons, favicon_program, line_icon, parse_favicon_program, stacked_variants,
};
use crate::font_builder::{self, FontBuilder};
use crate::icon_map::{
    FAVICON_PREFIX, FAVICON_PUA_END, PLACEHOLDER_CODEPOINT, PUA_START, ProgramIconEntry,
    ProgramIconMap,
};
use crate::ipc::{Ipc, Node};
use crate::platform::{Compositor, FontInstaller, program_name};
use crate::terminal;
use crate::xdg::APP_NAME;

pub const DEFAULT_FONT_FAMILY_NAME: &str = "WorkspaceIconDaemon";
/// Half-size top/bottom/middle copies of each icon, for stacking: slot n
/// mirrors code point `PUA_START + n`.
const STACK_TOP_START: u32 = FAVICON_PUA_END;
const STACK_BOTTOM_START: u32 = 0x0010_B000;
const STACK_MIDDLE_START: u32 = 0x0010_E000;
const STACK_SLOTS: u32 = 0x1FF0; // The middle range is the smallest.
/// Zero-width layout lines drawn over icons: under each tabbed icon, above
/// each stacked column, and between the halves of a vertical split's column.
const TAB_UNDERLINE_CODEPOINT: u32 = 0x0010_FFF0;
const STACK_OVERLINE_CODEPOINT: u32 = 0x0010_FFF1;
const SPLIT_LINE_CODEPOINT: u32 = 0x0010_FFF2;
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
pub const FONT_LAYOUT_VERSION: &str = "workspace-icon-daemon layout 6";
/// Title markup sizes relative to the title text (icons, layout symbols) and
/// to the compositor's title font (stacked icon pairs, which fill its line).
const DEFAULT_TITLE_FONT_SIZE: f64 = 10.0;
const ICON_SCALE: f64 = 1.4;
const STACK_SCALE: f64 = 1.3;
/// How many of the most visited sites get a favicon baked into the font up front.
const FAVICON_TOP_SITES: usize = 300;
/// Delay before rebuilding the font for newly seen sites, to batch them.
const FAVICON_REBUILD_DELAY: Duration = Duration::from_mins(2);
/// Terminals whose foreground job (e.g. nvim) gets its own icon, badged with
/// the terminal's.
const TERMINAL_PROGRAMS: [&str; 12] = [
    "Alacritty",
    "foot",
    "footclient",
    "kitty",
    "org.wezfurlong.wezterm",
    "com.mitchellh.ghostty",
    "org.gnome.Ptyxis",
    "org.gnome.Terminal",
    "Gnome-terminal",
    "org.kde.konsole",
    "konsole",
    "xfce4-terminal",
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
/// Title prefixes that mark a job's window though the title omits its name.
fn job_title_marks(job: &str) -> &'static [char] {
    match job {
        "claude" => &['✳', '◐', '◓', '◑', '◒'],
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

/// Spun in every titlebar while startup takes noticeably long.
const STARTUP_SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const STARTUP_SPINNER_DELAY: Duration = Duration::from_millis(300);
const STARTUP_SPINNER_FRAME: Duration = Duration::from_millis(100);

const IGNORED_PROGRAMS: [&str; 10] = [
    "fzf", "tmux", "screen", "vim", "nano", "htop", "btop", "less", "man", "ssh",
];
const SUPERSCRIPT_DIGITS: [char; 10] = ['⁰', '¹', '²', '³', '⁴', '⁵', '⁶', '⁷', '⁸', '⁹'];
const SUBSCRIPT_DIGITS: [char; 10] = ['₀', '₁', '₂', '₃', '₄', '₅', '₆', '₇', '₈', '₉'];

/// Whether the startup spinner may still draw. Checked under the lock before
/// each frame, so nothing is drawn once the desktop is being restored.
static STARTUP_SPINNING: Mutex<bool> = Mutex::new(false);
/// Fonts are written by one build at a time.
static BUILD_LOCK: Mutex<()> = Mutex::new(());
static CLOCK_START: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Codepoints of an icon's top, bottom and middle stacking glyphs.
pub fn stacked_codepoints(codepoint: u32) -> Option<(u32, u32, u32)> {
    let slot = codepoint
        .checked_sub(PUA_START)
        .filter(|slot| *slot < STACK_SLOTS)?;
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
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a finite f64's log10 is within ±324"
    )]
    let magnitude = value.abs().log10().floor() as i32;
    let decimals = usize::try_from((5 - magnitude).max(0)).unwrap_or_default();
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
        let _build = BUILD_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        create_icon_font(&self.icons, self.base_font, &self.output, &self.family)
            .context("creating icon font")?;
        self.installer
            .install(&self.output)
            .with_context(|| format!("installing {}", self.output.display()))?;
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
        paths.push(
            line_icon(&stacked_dir, position, layout_color(layout))
                .with_context(|| format!("creating {position} line icon"))?,
        );
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
    let built = builder.build(base_font, &paths).context("building font")?;
    let directory = output.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(directory)
        .with_context(|| format!("creating {}", directory.display()))?;
    let temporary = tempfile::NamedTempFile::new_in(directory)
        .with_context(|| format!("creating temporary file in {}", directory.display()))?;
    std::fs::write(temporary.path(), &built.data)
        .with_context(|| format!("writing {}", temporary.path().display()))?;
    temporary
        .persist(output)
        .with_context(|| format!("moving font to {}", output.display()))?;
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
    /// Each window's icon and title when its title format was last set.
    titlebar_icons: HashMap<i64, (u32, String)>,
    split_container_formats: HashMap<i64, String>,
    stacking_available: bool,
    animating: bool,
    compositor_font_size: Option<f64>,
    favicons: Favicons,
    rebuild_requests: Option<Sender<()>>,
    pub notifier: fn(&str, &str),
    /// The glyphs in the font this session loaded. Installing a replacement
    /// font does not make its glyphs available to the current session.
    active_program_codepoints: HashMap<String, u32>,
    active_placeholder_available: bool,
}

impl Daemon {
    pub fn new(ipc: Box<dyn Ipc>, settings: Settings) -> Result<Self> {
        let program_icon_map =
            ProgramIconMap::load(&settings.program_icon_map_path).with_context(|| {
                format!(
                    "loading program icon map {}",
                    settings.program_icon_map_path.display()
                )
            })?;
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
            titlebar_icons: HashMap::new(),
            split_container_formats: HashMap::new(),
            stacking_available: false,
            animating: false,
            compositor_font_size: None,
            favicons: Favicons::new(&cache_dir),
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
                    .add_program(&program, icon_path.as_deref())
                    .with_context(|| format!("adding program {program}"))?;
                added_any |= added;
            }
        }
        if added_any {
            self.program_icon_map
                .save()
                .context("saving program icon map")?;
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
                .add_program(program, icon_path.as_deref())
                .with_context(|| format!("adding program {program}"))?;
            added_any |= added;
        }
        Ok(added_any)
    }

    /// Discover programs represented by currently open windows.
    pub fn add_running_programs(&mut self) -> Result<bool> {
        let tree = self.ipc.get_tree().context("getting window tree")?;
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
        let added = self
            .add_missing_programs(&missing)
            .context("adding missing programs")?;
        if added {
            self.program_icon_map
                .save()
                .context("saving program icon map")?;
        }
        Ok(added)
    }

    /// Check open windows for new programs, and install a font with their
    /// icons for the next session.
    pub fn process_new_programs(&mut self) -> Result<bool> {
        if !self
            .add_running_programs()
            .context("adding running programs")?
        {
            log::debug!("No new programs detected; skipping font rebuild");
            return Ok(false);
        }
        self.publish_font_update(true)
            .context("publishing font update")?;
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
        self.font_job().run().context("running font job")?;
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
        let tree = self.ipc.get_tree().context("getting window tree")?;
        for workspace in tree.workspaces() {
            let mut windows = workspace.leaves();
            Self::sort_windows_by_layout(&mut windows);
            let mut icons: Vec<(String, bool)> = Vec::new();
            for window in windows {
                if Self::is_ignored(self.window_name(window).as_deref()) {
                    continue;
                }
                if let Some((codepoint, working)) = self.window_icon(window)
                    && let Some(c) = char::from_u32(codepoint)
                {
                    icons.push((c.to_string(), working));
                }
            }
            let processed = self.process_icons(icons);
            let new_name = construct_workspace_name(
                workspace.num.unwrap_or(-1),
                &processed,
                Some(&self.workspace_base_name(workspace.name())),
            );
            if new_name != workspace.name() {
                self.rename_workspace(workspace.name(), &new_name)
                    .with_context(|| {
                        format!("renaming workspace {:?} to {new_name:?}", workspace.name())
                    })?;
            }
        }
        Ok(())
    }

    /// Animation frame of a working job's icon, or None when it is idle.
    ///
    /// Frames come from the clock rather than from the title's spinner,
    /// which e.g. Claude Code stops updating while its terminal is unfocused.
    fn spinner_frame(&mut self, window: &Node, job: &str) -> Option<u64> {
        let first = window.name().chars().next();
        if !first.is_some_and(|c| job_spinner(job).contains(&c)) {
            return None;
        }
        self.animating = true;
        let frame = CLOCK_START.elapsed().as_millis() / ANIMATION_FRAME.as_millis()
            % u128::from(JOB_FRAMES);
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the frame is below JOB_FRAMES"
        )]
        let frame = frame as u64;
        Some(frame)
    }

    /// Redraw icons for the next animation frame while any job is working.
    pub fn animation_tick(&mut self) {
        if !self.animating {
            return;
        }
        self.animating = false; // Set again by any still-working job.
        if let Err(error) = self
            .update_workspace_names()
            .and_then(|()| self.update_window_titles())
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
            .add_program(&program, icon_path.as_deref())
            .with_context(|| format!("adding job {program}"))?;
        self.program_icon_map
            .save()
            .context("saving program icon map")?;
        Ok(icon_path.is_some())
    }

    pub fn add_preset_jobs(&mut self) -> Result<bool> {
        // Badge with the terminal in use, or else the first one installed.
        let tree = self.ipc.get_tree().context("getting window tree")?;
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
            added |= self
                .add_job(job, terminal)
                .with_context(|| format!("adding preset job {job}"))?;
        }
        Ok(added)
    }

    /// The window's site favicon if it is a browser on a known site, the
    /// icon of its foreground job if it is a terminal, otherwise its
    /// application icon.
    fn window_unicode_id(&mut self, window: &Node) -> Option<u32> {
        self.window_icon(window).map(|(codepoint, _)| codepoint)
    }

    /// The window's icon, and whether it is a working job's animated icon.
    fn window_icon(&mut self, window: &Node) -> Option<(u32, bool)> {
        let program = self.window_name(window)?;
        if TERMINAL_PROGRAMS.contains(&program.as_str())
            && let Some(mut job) =
                terminal::foreground_job(window.pid?, window.name(), job_title_marks)
        {
            let frame = self.spinner_frame(window, &job);
            // Frames missing from the loaded font fall back to the icon at rest.
            if let Some(frame) = frame
                && frame != 0
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
                return Some((codepoint, frame.is_some()));
            }
            match self.add_job(&job, &program) {
                Ok(true) => self.schedule_font_rebuild(),
                Ok(false) => {}
                Err(error) => log::warn!("Could not add job {job}: {error:#}"),
            }
        }
        if let Some(browser) = favicons::browser(&program)
            && let Some(host) = self
                .favicons
                .host_for_window(&program, window.name.as_deref())
        {
            let favicon = favicon_program(browser, &host);
            if let Some(&codepoint) = self.active_program_codepoints.get(&favicon) {
                return Some((codepoint, false));
            }
            if !self.program_icon_map.contains(&favicon) {
                self.add_favicon_later(browser, &host);
            }
        }
        self.active_unicode_id(&program)
            .map(|codepoint| (codepoint, false))
    }

    /// Icon overlaid on favicons to show which browser a window is.
    fn favicon_badge(&self, browser: &Browser) -> Option<PathBuf> {
        browser
            .app_ids
            .iter()
            .find_map(|p| self.program_icon_map.get_icon_path(p))
            .map(Path::to_path_buf)
            .or_else(|| {
                browser
                    .app_ids
                    .iter()
                    .find_map(|p| desktop::find_icon_for_program(p))
            })
    }

    /// Add or refresh a site's favicon; returns whether its glyph changed.
    fn add_favicon(&mut self, browser: &Browser, host: &str) -> Result<bool> {
        let program = favicon_program(browser, host);
        let entry = self.program_icon_map.programs.get(&program).cloned();
        if entry.as_ref().is_some_and(|e| e.icon_path.is_none()) {
            return Ok(false); // Known to have no favicon.
        }
        let badge = self.favicon_badge(browser);
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
            .add_program(&program, icon_path.as_deref())
            .with_context(|| format!("adding favicon {program}"))?;
        Ok(icon_path.is_some())
    }

    /// Add favicons of the most visited sites, and refresh known ones.
    pub fn add_top_favicons(&mut self) -> Result<bool> {
        if !self.settings.titlebar_icons || !self.favicons.available() {
            return Ok(false);
        }
        // Favicons from before they were kept per browser are dropped.
        let before = self.program_icon_map.programs.len();
        self.program_icon_map.programs.retain(|program, _| {
            !program.starts_with(FAVICON_PREFIX) || parse_favicon_program(program).is_some()
        });
        let mut changed = before - self.program_icon_map.programs.len();
        let mut sites = self.favicons.top_hosts(FAVICON_TOP_SITES);
        for program in self.program_icon_map.programs.keys() {
            if let Some((browser, host)) = parse_favicon_program(program)
                && !sites.iter().any(|(b, h)| *b == browser && h == host)
            {
                sites.push((browser, host.to_string()));
            }
        }
        for (browser, host) in sites {
            changed += usize::from(
                self.add_favicon(browser, &host)
                    .with_context(|| format!("adding favicon for {host} in {}", browser.id))?,
            );
        }
        self.program_icon_map
            .save()
            .context("saving program icon map")?;
        log::info!("Added or updated {changed} site favicons");
        Ok(changed > 0)
    }

    /// Add a newly seen site's favicon to the font for the next session.
    ///
    /// Deliberately silent: until then the browser icon is shown, and the
    /// favicon simply appears after some later login.
    fn add_favicon_later(&mut self, browser: &Browser, host: &str) {
        let added = self.add_favicon(browser, host);
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
        let tree = self.ipc.get_tree().context("getting window tree")?;
        let mut visible: HashSet<i64> = HashSet::new();
        for window in tree.leaves() {
            let Some(codepoint) = self.window_unicode_id(window) else {
                continue;
            };
            visible.insert(window.id);
            // A title change reruns the compositor's for_window rules, which
            // may replace the title format (e.g. `for_window [class=".*"]
            // title_format "%title"` also matches Wayland windows), so the
            // format is set again even when the icon is unchanged.
            let applied = (codepoint, window.name().to_string());
            if self.titlebar_icons.get(&window.id) == Some(&applied) {
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
            self.titlebar_icons.insert(window.id, applied);
            self.ipc
                .command(&format!(
                    "[con_id={}] title_format \"{title_format}\"",
                    window.id
                ))
                .with_context(|| format!("setting title format of window {}", window.id))?;
        }
        self.titlebar_icons.retain(|id, _| visible.contains(id));
        self.update_split_container_titles(&family)
    }

    /// The non-window containers nested inside workspaces.
    fn split_containers(&mut self) -> Result<Vec<Node>> {
        let tree = self.ipc.get_tree().context("getting window tree")?;
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
        if let [only] = con.nodes.as_slice() {
            return self.container_representation(only, family, nested, underline);
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
        for (pair, children) in stacked.chunks(2).zip(con.nodes.chunks(2)) {
            let mut column = match pair {
                [(top, _, _), (_, bottom, _)] => {
                    format!("{over}{}{between}{}", glyph(*top), glyph(*bottom))
                }
                [.., (_, _, middle)] => format!("{over}{}", glyph(*middle)),
                [] => String::new(),
            };
            if children.iter().any(|c| c.focused) {
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
        static FONT_LINE: LazyLock<Option<Regex>> =
            LazyLock::new(|| Regex::new(r"(?m)^font\s+(.*?)\s*$").ok());
        static FONT_SIZE: LazyLock<Option<Regex>> =
            LazyLock::new(|| Regex::new(r"([\d.]+)\s*(px)?$").ok());
        if let Some(size) = self.compositor_font_size {
            return size;
        }
        let mut size = DEFAULT_TITLE_FONT_SIZE;
        let config = self.ipc.get_config().unwrap_or_default();
        if let Some(font_line) = FONT_LINE.as_ref()
            && let Some(font_size) = FONT_SIZE.as_ref()
            && let Some(line) = font_line.captures(&config)
            && let Some(font) = line.get(1)
            && let Some(found) = font_size.captures(font.as_str())
            && let Some(points) = found.get(1)
            && let Ok(points) = points.as_str().parse::<f64>()
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
        for con in self
            .split_containers()
            .context("getting split containers")?
        {
            let title_format = self.container_representation(&con, family, false, false);
            if self.split_container_formats.get(&con.id) != Some(&title_format) {
                self.ipc
                    .command(&format!(
                        "[con_id={}] title_format \"{title_format}\"",
                        con.id
                    ))
                    .with_context(|| format!("setting title format of container {}", con.id))?;
            }
            formats.insert(con.id, title_format);
        }
        self.split_container_formats = formats;
        Ok(())
    }

    /// Merge repeated icons. Working jobs' animated icons are kept apart
    /// from the same icons at rest, which they match once per turn.
    fn process_icons(&self, icons: Vec<(String, bool)>) -> Vec<String> {
        let mut unique: Vec<(String, bool)> = Vec::new();
        for icon in &icons {
            if !unique.contains(icon) {
                unique.push(icon.clone());
            }
        }
        let digits = match self.settings.unique_icons_mode {
            UniqueIconsMode::Nonunique => return icons.into_iter().map(|(icon, _)| icon).collect(),
            UniqueIconsMode::Unique => return unique.into_iter().map(|(icon, _)| icon).collect(),
            UniqueIconsMode::NumbersSuperscript => &SUPERSCRIPT_DIGITS,
            UniqueIconsMode::NumbersSubscript => &SUBSCRIPT_DIGITS,
        };
        unique
            .into_iter()
            .map(|icon| {
                let count = icons.iter().filter(|i| **i == icon).count();
                let (icon, _) = icon;
                if count > 1 {
                    let suffix: String = count
                        .to_string()
                        .chars()
                        .filter_map(|d| digits.get(d.to_digit(10)? as usize))
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
            self.update_workspace_names()
                .context("updating workspace names")?;
            self.update_window_titles()
                .context("updating window titles")?;
        }
        Ok(())
    }

    /// Restore workspace names and title formats.
    pub fn reset_desktop_state(&mut self) -> Result<()> {
        self.reset_plan()
            .apply(self.ipc.as_mut())
            .context("restoring workspace names and titles")?;
        self.titlebar_icons.clear();
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
            .context("font output path has no file name")?;
        let destination = self.font_installer.fonts_dir.join(name);
        let session_font = self
            .session_font(&destination)
            .context("finding this session's font")?;
        let active_font_available = self.snapshot_active_font(&session_font);
        let map_was_repaired = self.program_icon_map.modified_at_load;
        let installed_added = self
            .discover_installed_programs()
            .context("discovering installed programs")?;
        let running_added = self
            .add_running_programs()
            .context("adding running programs")?;
        let mut favicons_changed = self
            .add_top_favicons()
            .context("adding top site favicons")?;
        favicons_changed =
            self.add_preset_jobs().context("adding preset jobs")? || favicons_changed;

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
            self.publish_font_update(false)
                .context("publishing initial font")?;
            (self.notifier)(
                "WorkspaceIconDaemon: Icon font installed",
                "Log out and back in again to show application icons",
            );
            return Ok(false);
        }
        if installed_added || running_added || map_was_repaired {
            self.publish_font_update(true)
                .context("publishing font update")?;
        } else if installed_is_outdated
            || favicons_changed
            || font_builder::font_version(&destination).as_deref() != Some(FONT_LAYOUT_VERSION)
        {
            // Only favicons changed; they show up quietly after a later login.
            self.publish_font_update(false)
                .context("publishing font update")?;
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
        static SESSION: LazyLock<Option<Regex>> =
            LazyLock::new(|| Regex::new(r"\.(\d+)\.sock$").ok());
        let session_pattern = SESSION
            .as_ref()
            .context("session socket pattern is invalid")?;
        let socket = ["SWAYSOCK", "I3SOCK"]
            .iter()
            .find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty()))
            .unwrap_or_default();
        let Some(session) = session_pattern
            .captures(&socket)
            .and_then(|captures| captures.get(1))
        else {
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
        let session_font = session_dir.join(format!("{}.ttf", session.as_str()));
        if !session_font.is_file() {
            std::fs::create_dir_all(&session_dir)
                .with_context(|| format!("creating {}", session_dir.display()))?;
            let stale_fonts = std::fs::read_dir(&session_dir)
                .with_context(|| format!("reading {}", session_dir.display()))?;
            for stale in stale_fonts.flatten() {
                if stale.path().extension().is_some_and(|e| e == "ttf") {
                    let _ = std::fs::remove_file(stale.path());
                }
            }
            std::fs::copy(installed_font, &session_font).with_context(|| {
                format!(
                    "copying {} to {}",
                    installed_font.display(),
                    session_font.display()
                )
            })?;
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
    let suffix = name.get(base.len()..).unwrap_or_default();
    if !suffix.chars().any(|c| icon_chars.contains(&c)) {
        return name.to_string(); // No icons of ours; leave e.g. trailing spaces alone.
    }
    base.to_string()
}

/// Spins in every titlebar until dropped, then restores plain titles.
///
/// It has its own connection, since startup holds the daemon throughout.
struct StartupSpinner {
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl StartupSpinner {
    fn start(mut ipc: Box<dyn Ipc>, title: String) -> Self {
        *STARTUP_SPINNING
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = true;
        let (stop, stopped) = mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            // Quick startups finish without a flash of spinners.
            if stopped.recv_timeout(STARTUP_SPINNER_DELAY) != Err(RecvTimeoutError::Timeout) {
                return;
            }
            let draw = |ipc: &mut dyn Ipc, format: &str| {
                let spinning = STARTUP_SPINNING
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if *spinning {
                    // Fails harmlessly when there are no windows.
                    let _ = ipc.command(&format!("[all] title_format \"{format}\""));
                }
                *spinning
            };
            for frame in STARTUP_SPINNER.iter().cycle() {
                if !draw(ipc.as_mut(), &format!("{frame} {title}")) {
                    return;
                }
                if stopped.recv_timeout(STARTUP_SPINNER_FRAME) != Err(RecvTimeoutError::Timeout) {
                    break;
                }
            }
            draw(ipc.as_mut(), "%title");
        });
        Self {
            stop: Some(stop),
            thread: Some(thread),
        }
    }
}

impl Drop for StartupSpinner {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        *STARTUP_SPINNING
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = false;
    }
}

/// Restores default workspace names and window titles.
pub struct ResetPlan {
    icon_chars: HashSet<char>,
    workspace_icons: bool,
    titlebar_icons: bool,
}

impl ResetPlan {
    pub fn apply(&self, ipc: &mut dyn Ipc) -> Result<()> {
        *STARTUP_SPINNING
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = false;
        let tree = ipc.get_tree().context("getting window tree")?;
        if self.titlebar_icons {
            for window in tree.leaves() {
                ipc.command(&format!("[con_id={}] title_format \"%title\"", window.id))
                    .with_context(|| format!("resetting title format of window {}", window.id))?;
            }
            for workspace in tree.workspaces() {
                for con in workspace.descendants() {
                    if con.kind == "con" && !con.nodes.is_empty() {
                        ipc.command(&format!("[con_id={}] title_format \"%title\"", con.id))
                            .with_context(|| {
                                format!("resetting title format of container {}", con.id)
                            })?;
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
                    ipc.command(&format!("rename workspace \"{old}\" to \"{new}\""))
                        .with_context(|| {
                            format!("renaming workspace {:?} to {new_name:?}", workspace.name())
                        })?;
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
    static NUMBERED: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"^\d+:?$").ok());
    let icons = icons.concat();
    let is_numbered = |base: &str| NUMBERED.as_ref().is_some_and(|r| r.is_match(base.trim()));
    match base_name {
        Some(base) if !is_numbered(base) => {
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
    daemon.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Run the daemon until the compositor goes away. `spinner_ipc` shows a
/// spinner in titlebars while startup is busy.
pub fn run(
    daemon: &Arc<Mutex<Daemon>>,
    spinner_ipc: Option<Box<dyn Ipc>>,
    connect_events: impl FnOnce() -> Result<crate::ipc::EventStream>,
) -> Result<()> {
    use crate::ipc::Event;
    log::info!("Starting workspace icon daemon...");
    let (requests, received) = mpsc::channel::<()>();
    lock(daemon).rebuild_requests = Some(requests);
    {
        let mut guard = lock(daemon);
        let spinner = spinner_ipc
            .filter(|_| guard.settings.titlebar_icons)
            .map(|ipc| StartupSpinner::start(ipc, guard.title_text("%title")));
        let ready = guard
            .ensure_startup_font()
            .context("preparing the icon font")?;
        drop(spinner);
        if !ready {
            return Ok(());
        }
        guard
            .update_workspace_names()
            .context("updating workspace names")?;
        guard
            .update_window_titles()
            .context("updating window titles")?;
    }
    let mut events = connect_events().context("connecting to compositor events")?;
    let rebuilder = Arc::clone(daemon);
    std::thread::spawn(move || {
        while received.recv().is_ok() {
            // Wait until requests stop arriving, then build once.
            loop {
                match received.recv_timeout(FAVICON_REBUILD_DELAY) {
                    Ok(()) => {}
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
    let animator = Arc::clone(daemon);
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(ANIMATION_FRAME);
            lock(&animator).animation_tick();
        }
    });

    log::info!("Daemon is running. Press Ctrl+C to exit.");
    while let Some(event) = events.next_event().context("reading compositor event")? {
        let mut guard = lock(daemon);
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
