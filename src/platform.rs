//! Compositor detection and font installation.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;

use crate::ipc::{Node, Version};

/// Supported compositor/window-manager implementations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Compositor {
    Auto,
    I3,
    Sway,
}

impl Compositor {
    pub fn as_str(self) -> &'static str {
        match self {
            Compositor::Auto => "auto",
            Compositor::I3 => "i3",
            Compositor::Sway => "sway",
        }
    }
}

/// Detect i3 or Sway from the IPC version response.
pub fn detect_compositor(version: &Version) -> Compositor {
    let values = [&version.human_readable, &version.loaded_config_file_name];
    if values
        .iter()
        .any(|value| value.to_lowercase().contains("sway"))
    {
        Compositor::Sway
    } else {
        Compositor::I3
    }
}

/// A stable application identifier for an IPC window node.
pub fn program_name(window: &Node, compositor: Compositor) -> Option<&str> {
    let class = window.window_class().filter(|class| !class.is_empty());
    if compositor == Compositor::Sway {
        window
            .app_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .or(class)
    } else {
        class
    }
}

/// Install a generated font.
#[derive(Debug, Clone)]
pub struct FontInstaller {
    pub fonts_dir: PathBuf,
}

impl FontInstaller {
    /// Atomically install a font and refresh fontconfig's cache.
    ///
    /// Already-running renderers keep using the font they loaded at session
    /// start, so this never restarts a bar or compositor; the replacement
    /// becomes usable after the user's next login.
    pub fn install(&self, source: &Path) -> Result<PathBuf> {
        if !source.is_file() {
            bail!("Font file does not exist: {}", source.display());
        }
        fs::create_dir_all(&self.fonts_dir)
            .with_context(|| format!("creating {}", self.fonts_dir.display()))?;
        let name = source.file_name().context("font path has no file name")?;
        let destination = self.fonts_dir.join(name);
        // Do not truncate a font file while a renderer may have it mmap'ed.
        // Publish a fully written replacement as a new inode instead.
        let temporary = tempfile::Builder::new()
            .prefix(&format!(".{}.", name.to_string_lossy()))
            .tempfile_in(&self.fonts_dir)
            .with_context(|| {
                format!("creating a temporary file in {}", self.fonts_dir.display())
            })?;
        fs::copy(source, temporary.path())
            .with_context(|| format!("copying {}", source.display()))?;
        temporary
            .persist(&destination)
            .with_context(|| format!("replacing {}", destination.display()))?;
        refresh_font_cache(&self.fonts_dir).context("refreshing the font cache")?;
        log::info!("Installed icon font at {}", destination.display());
        Ok(destination)
    }
}

pub fn refresh_font_cache(fonts_dir: &Path) -> Result<()> {
    let status = Command::new("fc-cache")
        .arg("-f")
        .arg(fonts_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("running fc-cache")?;
    if !status.success() {
        bail!("fc-cache failed: {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(app_id: Option<&str>, class: Option<&str>) -> Node {
        serde_json::from_value(serde_json::json!({
            "app_id": app_id,
            "window_properties": {"class": class},
        }))
        .unwrap()
    }

    #[test]
    fn detects_compositor() {
        let sway = Version {
            human_readable: "sway version 1.10".into(),
            ..Default::default()
        };
        assert_eq!(detect_compositor(&sway), Compositor::Sway);
        let config = Version {
            loaded_config_file_name: "/home/u/.config/sway/config".into(),
            ..Default::default()
        };
        assert_eq!(detect_compositor(&config), Compositor::Sway);
        let i3 = Version {
            human_readable: "4.23".into(),
            ..Default::default()
        };
        assert_eq!(detect_compositor(&i3), Compositor::I3);
    }

    #[test]
    fn sway_prefers_app_id_and_supports_xwayland() {
        assert_eq!(
            program_name(&window(Some("foot"), None), Compositor::Sway),
            Some("foot")
        );
        assert_eq!(
            program_name(&window(None, Some("Firefox")), Compositor::Sway),
            Some("Firefox")
        );
        assert_eq!(
            program_name(&window(Some("foot"), Some("X")), Compositor::I3),
            Some("X")
        );
    }

    #[test]
    fn font_install_replaces_existing_file() {
        if Command::new("fc-cache").arg("--version").output().is_err() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Icons.ttf");
        fs::write(&source, b"new").unwrap();
        let fonts = dir.path().join("fonts");
        fs::create_dir_all(&fonts).unwrap();
        fs::write(fonts.join("Icons.ttf"), b"old").unwrap();
        let installed = FontInstaller {
            fonts_dir: fonts.clone(),
        }
        .install(&source)
        .unwrap();
        assert_eq!(fs::read(installed).unwrap(), b"new");
        let leftovers: Vec<_> = fs::read_dir(&fonts)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(leftovers.is_empty());
    }
}
