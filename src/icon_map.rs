//! The persistent mapping from programs to icon files and code points.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::assets::{PLACEHOLDER_ICON_NAME, placeholder_icon_path};

pub const PUA_START: u32 = 0xEC00; // clear of Nerd Font / Font Awesome glyphs used by the bar
pub const PLACEHOLDER_CODEPOINT: u32 = PUA_START;
pub const PROGRAM_PUA_START: u32 = PUA_START + 1;
/// Site favicons live in Supplementary Private Use Area-B so that however
/// many accumulate, they never run into the application icons or other icon
/// fonts.
pub const FAVICON_PUA_START: u32 = 0x100000;
pub const FAVICON_PREFIX: &str = "favicon:";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgramIconEntry {
    /// None for programs known to have no icon.
    pub icon_path: Option<PathBuf>,
    #[serde(default = "no_codepoint")]
    pub unicode_id: i64,
}

fn no_codepoint() -> i64 {
    -1
}

impl ProgramIconEntry {
    /// The entry's code point, if it has an icon.
    pub fn codepoint(&self) -> Option<u32> {
        self.icon_path
            .as_ref()
            .and_then(|_| u32::try_from(self.unicode_id).ok())
    }
}

/// Programs, their icon paths and the code points assigned to them, which
/// stay stable across font rebuilds.
#[derive(Debug, Clone)]
pub struct ProgramIconMap {
    pub filepath: PathBuf,
    pub programs: BTreeMap<String, ProgramIconEntry>,
    pub next_unicode_id: u32,
    pub next_favicon_id: u32,
    /// Whether entries whose icons disappeared were dropped while loading.
    pub modified_at_load: bool,
}

impl ProgramIconMap {
    pub fn load(filepath: &Path) -> Result<Self> {
        let mut map = Self {
            filepath: filepath.to_path_buf(),
            programs: BTreeMap::new(),
            next_unicode_id: PROGRAM_PUA_START,
            next_favicon_id: FAVICON_PUA_START,
            modified_at_load: false,
        };
        if !filepath.exists() {
            log::debug!(
                "Program icon map not found at {}, starting fresh",
                filepath.display()
            );
            return Ok(map);
        }
        let text = std::fs::read_to_string(filepath)
            .with_context(|| format!("Reading {}", filepath.display()))?;
        let raw: Option<BTreeMap<String, serde_yaml_ng::Value>> = serde_yaml_ng::from_str(&text)
            .with_context(|| format!("Parsing {}", filepath.display()))?;

        let mut removed = Vec::new();
        let mut relocated = false;
        for (program, value) in raw.unwrap_or_default() {
            let has_icon_path = value
                .as_mapping()
                .is_some_and(|m| m.contains_key("icon_path"));
            let entry: ProgramIconEntry = match serde_yaml_ng::from_value(value) {
                Ok(entry) if has_icon_path => entry,
                _ => bail!("Invalid entry for {program} in {}", filepath.display()),
            };
            let mut entry = entry;
            if let Some(path) = &entry.icon_path
                && !path.exists()
            {
                // The bundled placeholder moved, e.g. from an older install.
                if path
                    .file_name()
                    .is_some_and(|name| name == PLACEHOLDER_ICON_NAME)
                {
                    entry.icon_path = Some(placeholder_icon_path());
                    relocated = true;
                } else {
                    log::debug!(
                        "Icon path for {program} does not exist: {}. Removing entry.",
                        path.display()
                    );
                    removed.push(program);
                    continue;
                }
            }
            map.programs.insert(program, entry);
        }

        let codepoints: Vec<u32> = map
            .programs
            .values()
            .filter_map(|e| e.codepoint())
            .collect();
        if let Some(max) = codepoints
            .iter()
            .filter(|&&cp| cp < FAVICON_PUA_START)
            .max()
        {
            map.next_unicode_id = PROGRAM_PUA_START.max(max + 1);
        }
        if let Some(max) = codepoints
            .iter()
            .filter(|&&cp| cp >= FAVICON_PUA_START)
            .max()
        {
            map.next_favicon_id = max + 1;
        }
        log::debug!(
            "Loaded {} programs from {}",
            map.programs.len(),
            filepath.display()
        );

        if !removed.is_empty() {
            log::debug!(
                "Removed {} programs with missing icon paths: {}",
                removed.len(),
                removed.join(", ")
            );
            map.modified_at_load = true;
        }
        if !removed.is_empty() || relocated {
            map.save()?;
        }
        Ok(map)
    }

    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.filepath.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_yaml_ng::to_string(&self.programs)?;
        std::fs::write(&self.filepath, text).with_context(|| {
            format!(
                "Failed to save program icon map {}",
                self.filepath.display()
            )
        })?;
        log::debug!("Saved program icon map to {}", self.filepath.display());
        Ok(())
    }

    /// Add a program, assigning it a code point if it has an icon. Returns
    /// whether it was new, and its code point.
    pub fn add_program(
        &mut self,
        program: &str,
        icon_path: Option<&Path>,
    ) -> Result<(bool, Option<u32>)> {
        if let Some(entry) = self.programs.get(program) {
            return Ok((false, entry.codepoint()));
        }
        let Some(icon_path) = icon_path else {
            self.programs.insert(
                program.to_string(),
                ProgramIconEntry {
                    icon_path: None,
                    unicode_id: -1,
                },
            );
            log::debug!("Added program: {program} -> (no icon, no Unicode ID)");
            return Ok((true, None));
        };
        if !icon_path.exists() {
            bail!("Icon path does not exist: {}", icon_path.display());
        }
        let counter = if program.starts_with(FAVICON_PREFIX) {
            &mut self.next_favicon_id
        } else {
            &mut self.next_unicode_id
        };
        let codepoint = *counter;
        *counter += 1;
        self.programs.insert(
            program.to_string(),
            ProgramIconEntry {
                icon_path: Some(icon_path.to_path_buf()),
                unicode_id: codepoint as i64,
            },
        );
        log::debug!(
            "Added program: {program} -> {} -> U+{codepoint:04X}",
            icon_path.display()
        );
        Ok((true, Some(codepoint)))
    }

    pub fn get_unicode_id(&self, program: &str) -> Option<u32> {
        self.programs.get(program)?.codepoint()
    }

    pub fn get_icon_path(&self, program: &str) -> Option<&Path> {
        self.programs.get(program)?.icon_path.as_deref()
    }

    pub fn contains(&self, program: &str) -> bool {
        self.programs.contains_key(program)
    }

    /// (icon path, code point) of every entry with an icon, by code point.
    pub fn icons(&self) -> Vec<(PathBuf, u32)> {
        let mut icons: Vec<(PathBuf, u32)> = self
            .programs
            .values()
            .filter_map(|e| Some((e.icon_path.clone()?, e.codepoint()?)))
            .collect();
        icons.sort_by_key(|(_, codepoint)| *codepoint);
        icons
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_removal_of_missing_icons() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("programs.yaml");
        let mut map = ProgramIconMap::load(&path).unwrap();
        for name in ["removed", "retained"] {
            let icon = dir.path().join(format!("{name}.png"));
            std::fs::write(&icon, b"x").unwrap();
            map.add_program(name, Some(&icon)).unwrap();
        }
        map.add_program("iconless", None).unwrap();
        map.add_program(
            "favicon:example.com",
            Some(&dir.path().join("retained.png")),
        )
        .unwrap();
        map.save().unwrap();

        let loaded = ProgramIconMap::load(&path).unwrap();
        assert!(!loaded.modified_at_load);
        assert_eq!(loaded.programs, map.programs);
        assert_eq!(loaded.get_unicode_id("iconless"), None);
        assert_eq!(
            loaded.get_unicode_id("favicon:example.com"),
            Some(FAVICON_PUA_START)
        );

        std::fs::remove_file(dir.path().join("removed.png")).unwrap();
        let mut restored = ProgramIconMap::load(&path).unwrap();
        assert!(restored.modified_at_load);
        assert!(!restored.contains("removed"));
        assert_eq!(restored.get_unicode_id("retained"), Some(0xEC02));
        let (added, codepoint) = restored
            .add_program("another", Some(&dir.path().join("retained.png")))
            .unwrap();
        assert!(added);
        assert_eq!(codepoint, Some(0xEC03));
        assert_eq!(restored.next_favicon_id, FAVICON_PUA_START + 1);
    }

    #[test]
    fn reads_maps_written_by_the_python_version() {
        let dir = tempfile::tempdir().unwrap();
        let icon = dir.path().join("a.svg");
        std::fs::write(&icon, b"<svg/>").unwrap();
        let path = dir.path().join("programs.yaml");
        std::fs::write(
            &path,
            format!(
                "Alacritty:\n  icon_path: {}\n  unicode_id: 60439\nfoo:\n  icon_path: null\n  unicode_id: -1\nold-placeholder:\n  icon_path: /gone/{PLACEHOLDER_ICON_NAME}\n  unicode_id: 60440\n",
                icon.display()
            ),
        )
        .unwrap();
        let map = ProgramIconMap::load(&path).unwrap();
        assert_eq!(map.get_unicode_id("Alacritty"), Some(60439));
        assert_eq!(map.get_unicode_id("foo"), None);
        assert_eq!(map.get_unicode_id("old-placeholder"), Some(60440));
        assert_eq!(
            map.get_icon_path("old-placeholder"),
            Some(placeholder_icon_path().as_path())
        );
        assert!(!map.modified_at_load);
        assert_eq!(map.next_unicode_id, 60441);
    }
}
