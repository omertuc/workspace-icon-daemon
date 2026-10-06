//! XDG base directories.

use std::path::PathBuf;

pub const APP_NAME: &str = "workspace-icon-daemon";

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn base_dir(variable: &str, default: &str) -> PathBuf {
    match std::env::var_os(variable) {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => home().join(default),
    }
}

pub fn config_home() -> PathBuf {
    base_dir("XDG_CONFIG_HOME", ".config")
}

pub fn cache_home() -> PathBuf {
    base_dir("XDG_CACHE_HOME", ".cache")
}

pub fn data_home() -> PathBuf {
    base_dir("XDG_DATA_HOME", ".local/share")
}

/// De-duplicated XDG data directories in lookup order, user first.
pub fn data_dirs() -> Vec<PathBuf> {
    let system = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());
    let mut dirs = vec![data_home()];
    for item in system.split(':').filter(|item| !item.is_empty()) {
        let dir = PathBuf::from(item);
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

pub fn default_config_dir() -> PathBuf {
    config_home().join(APP_NAME)
}

pub fn default_cache_dir() -> PathBuf {
    cache_home().join(APP_NAME)
}
