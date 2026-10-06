//! Desktop entries and application icon lookup.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::xdg;

/// Manual program name corrections for icon lookup.
pub fn corrected_name(name: &str) -> &str {
    match name {
        "thunderbird-esr" => "thunderbird_thunderbird",
        "firefox-esr" => "firefox_firefox",
        _ => name,
    }
}

const KNOWN_IMAGE_EXTENSIONS: [&str; 7] = ["svg", "png", "xpm", "ico", "gif", "jpg", "jpeg"];

/// Desktop-entry search roots in XDG precedence order.
pub fn desktop_application_paths() -> Vec<PathBuf> {
    let mut dirs = xdg::data_dirs();
    dirs.push(PathBuf::from("/var/lib/snapd/desktop"));
    dirs.into_iter()
        .map(|dir| dir.join("applications"))
        .collect()
}

/// Files below `dir` with the given extension, sorted, without following
/// symlinked directories.
pub fn files_with_extension(dir: &Path, extension: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == extension) && path.is_file() {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

fn stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Name variants with the separators (-, _, ., space) replaced, to handle
/// inconsistent naming conventions.
pub fn name_variants(name: &str) -> Vec<String> {
    let mut variants = vec![name.to_string()];
    for replacement in ["", "_", "-", "."] {
        let variant: String = name
            .chars()
            .map(|c| {
                if "-_. ".contains(c) {
                    replacement.to_string()
                } else {
                    c.to_string()
                }
            })
            .collect();
        if !variants.contains(&variant) {
            variants.push(variant);
        }
    }
    variants
}

/// The desktop entry for a window class, by progressively fuzzier matching.
pub fn find_desktop_file(class_name: &str) -> Option<PathBuf> {
    let bases: Vec<PathBuf> = desktop_application_paths()
        .into_iter()
        .filter(|base| base.is_dir())
        .collect();

    // 1. Exact match, case-sensitive.
    for base in &bases {
        let candidate = base.join(format!("{class_name}.desktop"));
        if candidate.exists() {
            return Some(candidate);
        }
    }
    let entries: Vec<(PathBuf, String)> = bases
        .iter()
        .flat_map(|base| files_with_extension(base, "desktop"))
        .map(|path| {
            let stem = stem(&path);
            (path, stem)
        })
        .collect();
    let lower = class_name.to_lowercase();
    // 2. Exact match, case-insensitive.
    // 3. Substring match, case-sensitive.
    // 4. Substring match, case-insensitive.
    let tiers: [&dyn Fn(&str) -> bool; 3] = [
        &|stem| stem.to_lowercase() == lower,
        &|stem| stem.contains(class_name),
        &|stem| stem.to_lowercase().contains(&lower),
    ];
    for matches in tiers {
        if let Some((path, _)) = entries.iter().find(|(_, stem)| matches(stem)) {
            return Some(path.clone());
        }
    }
    // 5. Separator-normalized variants.
    let variants: Vec<String> = name_variants(class_name)
        .iter()
        .map(|v| v.to_lowercase())
        .collect();
    for (path, stem) in &entries {
        let stem = stem.to_lowercase();
        if variants
            .iter()
            .any(|v| stem == *v || stem.starts_with(v) || stem.ends_with(v))
        {
            return Some(path.clone());
        }
    }
    None
}

/// A value from a desktop file's [Desktop Entry] section.
pub fn parse_desktop_value(path: &Path, key: &str) -> Option<String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            log::debug!("Failed to read desktop file {}: {error}", path.display());
            return None;
        }
    };
    let prefix = format!("{key}=");
    let mut in_desktop_entry = false;
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if line.starts_with('[') {
            in_desktop_entry = line == "[Desktop Entry]";
        } else if in_desktop_entry && let Some(value) = line.strip_prefix(&prefix) {
            let value = value.trim();
            return (!value.is_empty()).then(|| value.to_string());
        }
    }
    None
}

/// Preferred application-icon directories in XDG precedence order.
pub fn preferred_icon_search_paths(extension: &str) -> Vec<PathBuf> {
    let relative: &[&str] = match extension {
        "svg" => &[
            "icons/hicolor/scalable/apps",
            "icons/Humanity/apps/16",
            "icons/Humanity/apps/22",
            "icons/Humanity/apps/24",
            "icons/Humanity/apps/32",
            "icons/Humanity/apps/48",
            "icons/Humanity/apps/64",
            "icons/Humanity/apps/128",
            "icons/Humanity/apps/192",
            "icons/HighContrast/scalable/apps",
            "pixmaps",
        ],
        // The bundled font's bitmap strike is 109px. Prefer the nearest
        // source that does not need upscaling, then larger sources, followed
        // by progressively smaller fallbacks.
        "png" => &[
            "icons/hicolor/128x128/apps",
            "icons/hicolor/192x192/apps",
            "icons/hicolor/256x256/apps",
            "icons/hicolor/512x512/apps",
            "icons/hicolor/96x96/apps",
            "icons/hicolor/72x72/apps",
            "icons/hicolor/64x64/apps",
            "icons/hicolor/48x48/apps",
            "icons/hicolor/36x36/apps",
            "icons/hicolor/32x32/apps",
            "icons/hicolor/24x24/apps",
            "icons/hicolor/22x22/apps",
            "icons/hicolor/16x16/apps",
            "pixmaps",
        ],
        _ => panic!("Unsupported icon extension: {extension}"),
    };
    xdg::data_dirs()
        .iter()
        .flat_map(|dir| relative.iter().map(move |r| dir.join(r)))
        .collect()
}

/// Icon roots in user/system precedence order.
pub fn icon_search_roots() -> Vec<PathBuf> {
    xdg::data_dirs()
        .iter()
        .flat_map(|dir| [dir.join("icons"), dir.join("pixmaps")])
        .collect()
}

/// Every installed icon by lower-case name, indexed once while keeping the
/// lookup precedence of [`resolve_icon_path`].
pub fn installed_icon_index() -> HashMap<String, PathBuf> {
    let mut icons: HashMap<String, PathBuf> = HashMap::new();
    // First the curated application directories for SVG, then PNG. In
    // particular, a colour hicolor PNG must beat an unrelated theme's
    // monochrome SVG.
    for extension in ["svg", "png"] {
        for dir in preferred_icon_search_paths(extension) {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut paths: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == extension) && p.is_file())
                .collect();
            paths.sort();
            for path in paths {
                icons.entry(stem(&path).to_lowercase()).or_insert(path);
            }
        }
    }
    // Only then the recursive fallbacks.
    for extension in ["svg", "png"] {
        for root in icon_search_roots() {
            for path in files_with_extension(&root, extension) {
                icons.entry(stem(&path).to_lowercase()).or_insert(path);
            }
        }
    }
    icons
}

/// The name to search for: icon names like "foo.xpm" are looked up as
/// "foo" so an SVG or PNG of the same name is found.
fn search_name(icon_name: &str) -> String {
    let path = Path::new(icon_name);
    match path.extension() {
        Some(e)
            if KNOWN_IMAGE_EXTENSIONS.contains(&e.to_string_lossy().to_lowercase().as_str()) =>
        {
            stem(path)
        }
        _ => icon_name.to_string(),
    }
}

/// Resolve a desktop entry's Icon= value with a precomputed index.
pub fn resolve_icon_from_index(
    icon_name: Option<&str>,
    index: &HashMap<String, PathBuf>,
) -> Option<PathBuf> {
    let icon_name = icon_name.filter(|name| !name.is_empty())?;
    let path = Path::new(icon_name);
    let extension = path.extension().map(|e| e.to_string_lossy().to_lowercase());
    if path.is_absolute()
        && path.is_file()
        && matches!(extension.as_deref(), Some("svg") | Some("png"))
    {
        return Some(path.to_path_buf());
    }
    index.get(&search_name(icon_name).to_lowercase()).cloned()
}

/// Resolve an icon name or path to an icon file.
pub fn resolve_icon_path(icon_name: &str) -> Option<PathBuf> {
    let path = Path::new(icon_name);
    if path.is_absolute() && path.exists() {
        return Some(path.to_path_buf());
    }
    let name = search_name(icon_name);
    for extension in ["svg", "png"] {
        for dir in preferred_icon_search_paths(extension) {
            let candidate = dir.join(format!("{name}.{extension}"));
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    for extension in ["svg", "png"] {
        let target = format!("{name}.{extension}");
        for root in icon_search_roots() {
            if let Some(found) = files_with_extension(&root, extension)
                .into_iter()
                .find(|p| p.file_name().is_some_and(|f| f.to_string_lossy() == target))
            {
                log::debug!(
                    "Found {} via global search: {}",
                    extension.to_uppercase(),
                    found.display()
                );
                return Some(found);
            }
        }
    }
    None
}

/// The icon file for a program, via its desktop entry.
pub fn find_icon_for_program(program: &str) -> Option<PathBuf> {
    let Some(desktop_file) = find_desktop_file(program) else {
        log::warn!("No .desktop file found for program: {program}");
        return None;
    };
    let Some(icon_name) = parse_desktop_value(&desktop_file, "Icon") else {
        log::debug!(
            "Found desktop file for {program} but no Icon= entry: {}",
            desktop_file.display()
        );
        return None;
    };
    log::debug!(
        "Program {program}: desktop {} -> Icon={icon_name}",
        desktop_file.display()
    );
    let icon = resolve_icon_path(&icon_name);
    match &icon {
        Some(path) => log::debug!("Resolved icon for {program}: {}", path.display()),
        None => log::debug!("Could not resolve icon '{icon_name}' for program {program}"),
    }
    icon
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Tests that change XDG variables run one at a time.
    pub static ENV_LOCK: Mutex<()> = Mutex::new(());

    pub fn with_xdg<T>(data_home: &Path, data_dirs: &[&Path], test: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let joined: Vec<String> = data_dirs.iter().map(|d| d.display().to_string()).collect();
        let saved = (
            std::env::var_os("XDG_DATA_HOME"),
            std::env::var_os("XDG_DATA_DIRS"),
        );
        // SAFETY: tests touching the environment hold ENV_LOCK.
        unsafe {
            std::env::set_var("XDG_DATA_HOME", data_home);
            std::env::set_var("XDG_DATA_DIRS", joined.join(":"));
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(test));
        for (name, value) in [("XDG_DATA_HOME", saved.0), ("XDG_DATA_DIRS", saved.1)] {
            // SAFETY: as above.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
        result.unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    }

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"x").unwrap();
    }

    #[test]
    fn bulk_index_preserves_application_icon_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let system = dir.path().join("system");
        let user = dir.path().join("user");
        touch(&system.join("icons/Papirus/16x16/apps/app.svg"));
        touch(&system.join("icons/hicolor/128x128/apps/app.png"));
        touch(&system.join("icons/hicolor/scalable/apps/other.svg"));
        touch(&user.join("icons/hicolor/scalable/apps/other.svg"));
        with_xdg(&user, &[&system], || {
            let index = installed_icon_index();
            assert_eq!(
                index["app"],
                system.join("icons/hicolor/128x128/apps/app.png")
            );
            assert_eq!(
                index["other"],
                user.join("icons/hicolor/scalable/apps/other.svg")
            );
            assert_eq!(
                resolve_icon_from_index(Some("App.xpm"), &index),
                Some(system.join("icons/hicolor/128x128/apps/app.png"))
            );
            assert_eq!(
                resolve_icon_path("app"),
                Some(system.join("icons/hicolor/128x128/apps/app.png"))
            );
        });
    }

    #[test]
    fn png_precedence_prefers_128_over_larger_and_smaller_icons() {
        let dir = tempfile::tempdir().unwrap();
        for size in ["16x16", "512x512", "128x128", "256x256"] {
            touch(
                &dir.path()
                    .join(format!("icons/hicolor/{size}/apps/app.png")),
            );
        }
        with_xdg(dir.path(), &[], || {
            assert_eq!(
                resolve_icon_path("app"),
                Some(dir.path().join("icons/hicolor/128x128/apps/app.png"))
            );
        });
    }

    #[test]
    fn finds_desktop_files_fuzzily() {
        let dir = tempfile::tempdir().unwrap();
        let apps = dir.path().join("applications");
        for name in [
            "org.mozilla.firefox",
            "Alacritty",
            "com.slack.Slack",
            "foo-bar",
        ] {
            touch(&apps.join(format!("{name}.desktop")));
        }
        with_xdg(dir.path(), &[], || {
            let found = |class: &str| find_desktop_file(class).map(|p| stem(&p));
            assert_eq!(found("Alacritty").as_deref(), Some("Alacritty"));
            assert_eq!(found("alacritty").as_deref(), Some("Alacritty"));
            assert_eq!(found("firefox").as_deref(), Some("org.mozilla.firefox"));
            assert_eq!(found("Slack").as_deref(), Some("com.slack.Slack"));
            assert_eq!(found("foo_bar").as_deref(), Some("foo-bar"));
            assert_eq!(found("nothing"), None);
        });
    }

    #[test]
    fn desktop_values_come_from_the_desktop_entry_section() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.desktop");
        std::fs::write(
            &path,
            "[Desktop Action new]\nIcon=wrong\n\n[Desktop Entry]\nName=A\nIcon= right \nStartupWMClass=AClass\n",
        )
        .unwrap();
        assert_eq!(parse_desktop_value(&path, "Icon").as_deref(), Some("right"));
        assert_eq!(
            parse_desktop_value(&path, "StartupWMClass").as_deref(),
            Some("AClass")
        );
        assert_eq!(parse_desktop_value(&path, "Exec"), None);
    }
}
