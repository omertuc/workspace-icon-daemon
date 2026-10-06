//! Create a Private Use Area icon font from PNG and SVG images by extending
//! a CBDT/CBLC bitmap colour font such as Noto Color Emoji.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use workspace_icon_daemon::assets;
use workspace_icon_daemon::font_builder::{FontBuilder, PUA_START};

fn parse_codepoint(value: &str) -> Result<u32, String> {
    let parsed = match value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        Some(hex) => u32::from_str_radix(hex, 16),
        None => value.parse(),
    };
    parsed.map_err(|error| error.to_string())
}

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Create a Private Use Area icon font from PNG and SVG images by extending a CBDT/CBLC bitmap colour font"
)]
struct Args {
    /// Directory containing icon files (PNGs/SVGs), sorted by file name.
    #[arg(long, conflicts_with = "icon_paths", default_value = "input_symbols")]
    input_folder: PathBuf,

    /// Icon files, mapped to code points in the order given
    #[arg(long, num_args = 1.., value_name = "PATH")]
    icon_paths: Vec<PathBuf>,

    /// Output font file path
    #[arg(long, default_value = "./MyCreatedIconFont.ttf")]
    output: PathBuf,

    /// Base CBDT/CBLC font (default: the bundled Noto Color Emoji)
    #[arg(long)]
    base_font: Option<PathBuf>,

    /// Font family name
    #[arg(long, default_value = "MyCreatedIconFont")]
    family_name: String,

    /// First code point in the Private Use Area
    #[arg(long, value_name = "CODEPOINT", value_parser = parse_codepoint, default_value = "0xE000")]
    pua_start: u32,

    /// Remove the base font's own glyphs, keeping only .notdef, space and
    /// the new icons
    #[arg(long)]
    remove_original_symbols: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .format_timestamp(None)
        .init();

    let image_paths = if args.icon_paths.is_empty() {
        let folder = args.input_folder;
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&folder)
            .with_context(|| format!("reading {}", folder.display()))?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_file()
                    && path.extension().is_some_and(|e| {
                        matches!(e.to_string_lossy().to_lowercase().as_str(), "png" | "svg")
                    })
            })
            .collect();
        paths.sort_by_key(|path| path.file_name().map(std::ffi::OsStr::to_os_string));
        paths
    } else {
        args.icon_paths
    };
    let base_font = match &args.base_font {
        Some(path) => std::fs::read(path).with_context(|| format!("reading {}", path.display()))?,
        None => assets::BASE_FONT.to_vec(),
    };
    let builder = FontBuilder {
        family_name: args.family_name,
        pua_start: args.pua_start,
        remove_original_symbols: args.remove_original_symbols,
        ..Default::default()
    };
    let built = builder
        .build(&base_font, &image_paths)
        .context("building font")?;
    if let Some(parent) = args.output.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&args.output, &built.data)
        .with_context(|| format!("writing {}", args.output.display()))?;
    for (path, codepoint) in image_paths.iter().zip(&built.codepoints) {
        log::info!("[+] {} -> U+{codepoint:04X}", path.display());
    }
    log::info!(
        "Wrote {} with {} icons starting at U+{:04X}",
        args.output.display(),
        built.codepoints.len(),
        built.codepoints.first().copied().unwrap_or(PUA_START)
    );
    Ok(())
}
