//! The persistent mapping from programs to icon files and code points.

use std::collections::BTreeMap;
use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::assets::{PLACEHOLDER_ICON_NAME, placeholder_icon_path};

/// Every icon lives in Supplementary Private Use Area-B. Icon fonts such as
/// Nerd Fonts fill the BMP Private Use Area and Private Use Area-A, and
/// fontconfig falls back to any installed font that covers a missing glyph,
/// so icons there show up in other applications in place of those glyphs.
pub const PUA_START: u32 = 0x0010_0000;
pub const PLACEHOLDER_CODEPOINT: u32 = PUA_START;
pub const PROGRAM_PUA_START: u32 = PUA_START + 1;
/// Site favicons follow the application icons.
pub const FAVICON_PUA_START: u32 = PUA_START + 0x400;
/// The end of the favicon range, where the stacking glyphs begin.
pub const FAVICON_PUA_END: u32 = 0x0010_8000;
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
    /// Whether entries whose icons disappeared were dropped, or entries were
    /// renumbered, while loading.
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
            .with_context(|| format!("reading {}", filepath.display()))?;
        let raw: Option<BTreeMap<String, serde_yaml_ng::Value>> = serde_yaml_ng::from_str(&text)
            .with_context(|| format!("parsing {}", filepath.display()))?;

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

        // Entries outside their range, e.g. from versions that used the BMP
        // Private Use Area, are renumbered after the rest, in their order.
        let codepoints: Vec<(String, u32)> = map
            .programs
            .iter()
            .filter_map(|(program, entry)| Some((program.clone(), entry.codepoint()?)))
            .collect();
        let mut misplaced = Vec::new();
        for (program, codepoint) in codepoints {
            let (counter, range) = map.counter(&program);
            if range.contains(&codepoint) {
                *counter = (*counter).max(codepoint + 1);
            } else {
                misplaced.push((codepoint, program));
            }
        }
        misplaced.sort();
        let renumbered = !misplaced.is_empty();
        for (old, program) in misplaced {
            if let Some(codepoint) = map.allocate(&program) {
                log::debug!("Renumbered {program}: U+{old:04X} -> U+{codepoint:04X}");
                if let Some(entry) = map.programs.get_mut(&program) {
                    entry.unicode_id = i64::from(codepoint);
                }
            } else {
                log::warn!("No code point left for {program}. Removing entry.");
                map.programs.remove(&program);
            }
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
        }
        if !removed.is_empty() || renumbered {
            map.modified_at_load = true;
        }
        if !removed.is_empty() || relocated || renumbered {
            map.save()
                .context("saving the cleaned-up program icon map")?;
        }
        Ok(map)
    }

    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.filepath.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let text =
            serde_yaml_ng::to_string(&self.programs).context("serializing the program icon map")?;
        std::fs::write(&self.filepath, text)
            .with_context(|| format!("writing {}", self.filepath.display()))?;
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
        if let Some(icon_path) = icon_path
            && !icon_path.exists()
        {
            bail!("Icon path does not exist: {}", icon_path.display());
        }
        let icon = icon_path.and_then(|path| {
            let codepoint = self.allocate(program);
            if codepoint.is_none() {
                log::warn!("No code point left for {program}, tracking without icon");
            }
            Some((path, codepoint?))
        });
        let Some((icon_path, codepoint)) = icon else {
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
        self.programs.insert(
            program.to_string(),
            ProgramIconEntry {
                icon_path: Some(icon_path.to_path_buf()),
                unicode_id: i64::from(codepoint),
            },
        );
        log::debug!(
            "Added program: {program} -> {} -> U+{codepoint:04X}",
            icon_path.display()
        );
        Ok((true, Some(codepoint)))
    }

    /// The next-code-point counter for a program and the range it draws from.
    fn counter(&mut self, program: &str) -> (&mut u32, Range<u32>) {
        if program.starts_with(FAVICON_PREFIX) {
            (
                &mut self.next_favicon_id,
                FAVICON_PUA_START..FAVICON_PUA_END,
            )
        } else {
            (
                &mut self.next_unicode_id,
                PROGRAM_PUA_START..FAVICON_PUA_START,
            )
        }
    }

    /// Take the next free code point for a program, if its range has one.
    fn allocate(&mut self, program: &str) -> Option<u32> {
        let (counter, range) = self.counter(program);
        let codepoint = *counter;
        range.contains(&codepoint).then(|| {
            *counter += 1;
            codepoint
        })
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
        assert_eq!(
            restored.get_unicode_id("retained"),
            Some(PROGRAM_PUA_START + 1)
        );
        let (added, codepoint) = restored
            .add_program("another", Some(&dir.path().join("retained.png")))
            .unwrap();
        assert!(added);
        assert_eq!(codepoint, Some(PROGRAM_PUA_START + 2));
        assert_eq!(restored.next_favicon_id, FAVICON_PUA_START + 1);
    }

    #[test]
    fn renumbers_codepoints_outside_their_range() {
        let dir = tempfile::tempdir().unwrap();
        let icon = dir.path().join("a.svg");
        std::fs::write(&icon, b"<svg/>").unwrap();
        let path = dir.path().join("programs.yaml");
        let icon = icon.display();
        std::fs::write(
            &path,
            format!(
                "kept:\n  icon_path: {icon}\n  unicode_id: {PROGRAM_PUA_START}\n\
                 old-b:\n  icon_path: {icon}\n  unicode_id: 60426\n\
                 old-a:\n  icon_path: {icon}\n  unicode_id: 60427\n\
                 favicon:old.example:\n  icon_path: {icon}\n  unicode_id: 1048578\n\
                 favicon:kept.example:\n  icon_path: {icon}\n  unicode_id: {FAVICON_PUA_START}\n"
            ),
        )
        .unwrap();
        let mut map = ProgramIconMap::load(&path).unwrap();
        assert!(map.modified_at_load);
        assert_eq!(map.get_unicode_id("kept"), Some(PROGRAM_PUA_START));
        // Renumbered entries keep their relative order.
        assert_eq!(map.get_unicode_id("old-b"), Some(PROGRAM_PUA_START + 1));
        assert_eq!(map.get_unicode_id("old-a"), Some(PROGRAM_PUA_START + 2));
        assert_eq!(
            map.get_unicode_id("favicon:old.example"),
            Some(FAVICON_PUA_START + 1)
        );
        assert_eq!(
            map.add_program("new", Some(Path::new(&icon.to_string())))
                .unwrap()
                .1,
            Some(PROGRAM_PUA_START + 3)
        );
        // The renumbering was saved.
        let reloaded = ProgramIconMap::load(&path).unwrap();
        assert!(!reloaded.modified_at_load);
        assert_eq!(
            reloaded.get_unicode_id("old-a"),
            Some(PROGRAM_PUA_START + 2)
        );
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
        assert_eq!(map.get_unicode_id("Alacritty"), Some(PROGRAM_PUA_START));
        assert_eq!(map.get_unicode_id("foo"), None);
        assert_eq!(
            map.get_unicode_id("old-placeholder"),
            Some(PROGRAM_PUA_START + 1)
        );
        assert_eq!(
            map.get_icon_path("old-placeholder"),
            Some(placeholder_icon_path().as_path())
        );
        assert!(map.modified_at_load);
        assert_eq!(map.next_unicode_id, PROGRAM_PUA_START + 2);
    }
}
