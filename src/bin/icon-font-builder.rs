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
    /// Default: ./input_symbols
    #[arg(long, conflicts_with = "icon_paths")]
    input_folder: Option<PathBuf>,

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

    let image_paths = if !args.icon_paths.is_empty() {
        args.icon_paths
    } else {
        let folder = args
            .input_folder
            .unwrap_or_else(|| PathBuf::from("input_symbols"));
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&folder)
            .with_context(|| format!("Reading {}", folder.display()))?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_file()
                    && path.extension().is_some_and(|e| {
                        matches!(e.to_string_lossy().to_lowercase().as_str(), "png" | "svg")
                    })
            })
            .collect();
        paths.sort_by_key(|path| path.file_name().map(|n| n.to_os_string()));
        paths
    };
    let base_font = match &args.base_font {
        Some(path) => std::fs::read(path).with_context(|| format!("Reading {}", path.display()))?,
        None => assets::BASE_FONT.to_vec(),
    };
    let builder = FontBuilder {
        family_name: args.family_name,
        pua_start: args.pua_start,
        remove_original_symbols: args.remove_original_symbols,
        ..Default::default()
    };
    let built = builder.build(&base_font, &image_paths)?;
    if let Some(parent) = args.output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&args.output, &built.data)?;
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
