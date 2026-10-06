//! Resources bundled into the binary.

use std::path::PathBuf;
use std::sync::OnceLock;

use crate::xdg;

/// The bitmap colour font generated fonts are built on.
pub static BASE_FONT: &[u8] = include_bytes!("../assets/NotoColorEmoji.ttf");
pub static PLACEHOLDER_ICON: &[u8] = include_bytes!("../assets/placeholder_icon.svg");
pub const PLACEHOLDER_ICON_NAME: &str = "placeholder_icon.svg";

/// The placeholder icon as a file, since icon map entries refer to icons by
/// path.
pub fn placeholder_icon_path() -> PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    static WARNED: OnceLock<()> = OnceLock::new();
    let path = PATH.get_or_init(|| {
        xdg::data_home()
            .join(xdg::APP_NAME)
            .join(PLACEHOLDER_ICON_NAME)
    });
    let current = std::fs::read(path).ok();
    if current.as_deref() != Some(PLACEHOLDER_ICON) {
        let written = path
            .parent()
            .map(std::fs::create_dir_all)
            .transpose()
            .and_then(|_| std::fs::write(path, PLACEHOLDER_ICON));
        if let Err(error) = written {
            WARNED.get_or_init(|| log::warn!("Could not write {}: {error}", path.display()));
        }
    }
    path.clone()
}
