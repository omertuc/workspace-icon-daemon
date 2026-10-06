//! workspace icon daemon for i3 and Sway: dynamically create icon fonts and
//! update workspace names and window titles.

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use clap::Parser;
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use workspace_icon_daemon::daemon::{
    self, DEFAULT_FONT_FAMILY_NAME, Daemon, Settings, UniqueIconsMode,
};
use workspace_icon_daemon::icon_map::ProgramIconMap;
use workspace_icon_daemon::ipc::Connection;
use workspace_icon_daemon::platform::{self, Compositor, detect_compositor};
use workspace_icon_daemon::{assets, pidfile, xdg};

#[derive(Parser, Debug)]
#[command(
    version,
    about = "workspace icon daemon for i3 and Sway - dynamically create icon fonts and update workspace names"
)]
struct Args {
    /// Compositor/window manager to use
    #[arg(long, value_enum, default_value_t = Compositor::Auto)]
    compositor: Compositor,

    /// Path to the program icon map YAML file
    #[arg(long, default_value_os_t = xdg::default_config_dir().join("program_icon_map.yaml"))]
    program_icon_map: PathBuf,

    /// Base CBDT/CBLC font file (default: the bundled Noto Color Emoji)
    #[arg(long)]
    base_font: Option<PathBuf>,

    /// Path where the custom font is saved
    #[arg(long, default_value_os_t = xdg::default_cache_dir().join("WorkspaceIconDaemon.ttf"))]
    font_output: PathBuf,

    /// Name of the custom font family
    #[arg(long, default_value = DEFAULT_FONT_FAMILY_NAME)]
    font_family_name: String,

    /// How repeated programs in a workspace are shown
    #[arg(long, value_enum, default_value_t = UniqueIconsMode::NumbersSubscript)]
    unique_icons: UniqueIconsMode,

    /// Don't use a placeholder icon for programs where no icon is found;
    /// such programs then don't appear in workspace names
    #[arg(long)]
    no_placeholder_icon: bool,

    /// Add application icons to workspace names (default)
    #[arg(long, overrides_with = "no_workspace_icons")]
    workspace_icons: bool,

    /// Don't add application icons to workspace names
    #[arg(long, overrides_with = "workspace_icons")]
    no_workspace_icons: bool,

    /// Add application icons to window titlebars (default). Pango markup must
    /// be enabled for the compositor title font
    #[arg(long, overrides_with = "no_titlebar_icons")]
    titlebar_icons: bool,

    /// Don't add application icons to window titlebars
    #[arg(long, overrides_with = "titlebar_icons")]
    no_titlebar_icons: bool,

    /// Draw title text at this size. Set the compositor's title font larger
    /// than this to make titlebars taller, giving icons more room
    #[arg(long, value_name = "PT")]
    title_text_size: Option<f64>,

    /// Stop a running daemon, restore default workspace/titlebar names,
    /// remove generated state and exit
    #[arg(long, conflicts_with = "reset_and_rebuild")]
    reset: bool,

    /// Perform --reset, then discover installed applications and install
    /// their font for the next login
    #[arg(long)]
    reset_and_rebuild: bool,

    /// Enable verbose logging
    #[arg(short, long)]
    verbose: bool,
}

fn init_logging(verbose: bool) {
    let level = if verbose {
        log::LevelFilter::Debug
    } else {
        log::LevelFilter::Info
    };
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Warn)
        .filter_module("workspace_icon_daemon", level)
        .parse_default_env()
        .format(|buf, record| {
            writeln!(
                buf,
                "{} - {} - {}",
                buf.timestamp_seconds(),
                record.level(),
                record.args()
            )
        })
        .init();
}

fn remove_generated_state(paths: &[&PathBuf]) {
    for path in paths {
        if path.exists() {
            match std::fs::remove_file(path) {
                Ok(()) => log::info!("Removed {}", path.display()),
                Err(error) => log::warn!("Could not remove {}: {error}", path.display()),
            }
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    init_logging(args.verbose);

    let mut connection = Connection::connect()?;
    let compositor = match args.compositor {
        Compositor::Auto => detect_compositor(&connection.get_version()?),
        requested => requested,
    };
    log::info!("Using {}", compositor.as_str());

    let base_font: &'static [u8] = match &args.base_font {
        Some(path) => Box::leak(
            std::fs::read(path)
                .with_context(|| format!("Reading {}", path.display()))?
                .into_boxed_slice(),
        ),
        None => assets::BASE_FONT,
    };
    let fonts_dir = xdg::data_home().join("fonts");
    let reset = args.reset || args.reset_and_rebuild;
    let settings = Settings {
        compositor,
        program_icon_map_path: args.program_icon_map.clone(),
        base_font,
        font_output_path: args.font_output.clone(),
        font_family_name: args.font_family_name.clone(),
        unique_icons_mode: args.unique_icons,
        use_placeholder_icon: !args.no_placeholder_icon,
        // A reset restores everything, whichever icons were enabled.
        workspace_icons: reset || !args.no_workspace_icons,
        titlebar_icons: reset || !args.no_titlebar_icons,
        title_text_size: args.title_text_size,
        fonts_dir: fonts_dir.clone(),
    };
    let mut daemon = Daemon::new(Box::new(connection), settings)?;

    let cache_dir = args
        .font_output
        .parent()
        .map(PathBuf::from)
        .unwrap_or_default();
    let pid_path = cache_dir.join("daemon.pid");
    let installed_font = fonts_dir.join(
        args.font_output
            .file_name()
            .context("Font output has no file name")?,
    );

    if reset {
        pidfile::stop_running_daemon(&pid_path);
        // Also done here: it covers a stale or missing PID file and makes the
        // operation idempotent.
        daemon.reset_desktop_state()?;
        remove_generated_state(&[
            &args.program_icon_map,
            &args.font_output,
            &installed_font,
            &pid_path,
        ]);
        let _ = platform::refresh_font_cache(&fonts_dir);
        if args.reset_and_rebuild {
            daemon.program_icon_map = ProgramIconMap::load(&args.program_icon_map)?;
            daemon.discover_installed_programs()?;
            daemon.add_running_programs()?;
            daemon.publish_font_update(false)?;
            daemon::notify(
                "WorkspaceIconDaemon: Icon font rebuilt",
                "Log out and back in again to show application icons",
            );
        }
        return Ok(());
    }

    // exec_always starts this command again whenever the compositor config
    // is reloaded, so replace any running daemon rather than have two
    // manage the same workspaces and titles.
    pidfile::stop_running_daemon(&pid_path);
    pidfile::write(&pid_path)?;

    let reset_plan = daemon.reset_plan();
    let daemon = Arc::new(Mutex::new(daemon));
    let mut signals = Signals::new([SIGINT, SIGTERM])?;
    let on_signal = Arc::clone(&daemon);
    let signal_pid_path = pid_path.clone();
    std::thread::spawn(move || {
        if let Some(signal) = signals.forever().next() {
            log::info!("Received signal {signal}, exiting gracefully...");
            // Exit promptly even while the daemon is busy (e.g. building a
            // font): a replacement daemon is waiting for this one to go.
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(300);
            let guard = loop {
                match on_signal.try_lock() {
                    Ok(guard) => break Some(guard),
                    Err(std::sync::TryLockError::Poisoned(e)) => break Some(e.into_inner()),
                    Err(std::sync::TryLockError::WouldBlock)
                        if std::time::Instant::now() < deadline =>
                    {
                        std::thread::sleep(std::time::Duration::from_millis(10))
                    }
                    Err(std::sync::TryLockError::WouldBlock) => break None,
                }
            };
            let result = match guard {
                Some(mut daemon) => daemon.reset_desktop_state(),
                None => Connection::connect().and_then(|mut c| reset_plan.apply(&mut c)),
            };
            if let Err(error) = result {
                log::warn!("Could not restore workspace names: {error:#}");
            }
            pidfile::remove_own(&signal_pid_path);
            std::process::exit(0);
        }
    });

    let result = daemon::run(daemon, || {
        Connection::connect()?.subscribe(&["window", "workspace", "binding", "shutdown"])
    });
    pidfile::remove_own(&pid_path);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Args {
        Args::try_parse_from(std::iter::once("workspace-icon-daemon").chain(args.iter().copied()))
            .unwrap()
    }

    #[test]
    fn icon_outputs_default_to_enabled_and_can_be_disabled_independently() {
        let args = parse(&[]);
        assert!(!args.no_workspace_icons && !args.no_titlebar_icons);
        let args = parse(&["--no-titlebar-icons"]);
        assert!(!args.no_workspace_icons && args.no_titlebar_icons);
        let args = parse(&["--no-workspace-icons", "--no-titlebar-icons"]);
        assert!(args.no_workspace_icons && args.no_titlebar_icons);
        // The last of a flag and its negation wins.
        let args = parse(&["--no-titlebar-icons", "--titlebar-icons"]);
        assert!(!args.no_titlebar_icons);
    }

    #[test]
    fn compositor_and_modes() {
        let args = parse(&[
            "--compositor",
            "sway",
            "--unique-icons",
            "numbers_superscript",
        ]);
        assert_eq!(args.compositor, Compositor::Sway);
        assert_eq!(args.unique_icons, UniqueIconsMode::NumbersSuperscript);
        assert_eq!(parse(&[]).unique_icons, UniqueIconsMode::NumbersSubscript);
    }

    #[test]
    fn reset_flags_are_exclusive() {
        assert!(parse(&["--reset"]).reset);
        assert!(parse(&["--reset-and-rebuild"]).reset_and_rebuild);
        assert!(Args::try_parse_from(["x", "--reset", "--reset-and-rebuild"]).is_err());
    }
}
