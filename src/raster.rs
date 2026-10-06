//! Image decoding, SVG rasterization and the small set of image operations
//! the icon pipeline needs.

use std::io::Cursor;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result, anyhow, bail};
use image::imageops::{self, FilterType};
use image::{ImageFormat, Rgba, RgbaImage};
use resvg::{tiny_skia, usvg};

use crate::assets;

/// Fonts for SVG `<text>`: the system's, plus the bundled emoji font so
/// emoji favicons render in colour.
fn fontdb() -> Arc<usvg::fontdb::Database> {
    static DATABASE: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    DATABASE
        .get_or_init(|| {
            let mut database = usvg::fontdb::Database::new();
            database.load_system_fonts();
            database.load_font_data(assets::BASE_FONT.to_vec());
            // fontdb's generic families name fonts that may not be installed.
            if let Some(family) = fontconfig_family("sans-serif") {
                database.set_sans_serif_family(family);
            }
            if let Some(family) = fontconfig_family("serif") {
                database.set_serif_family(family);
            }
            if let Some(family) = fontconfig_family("monospace") {
                database.set_monospace_family(family);
            }
            Arc::new(database)
        })
        .clone()
}

fn fontconfig_family(generic: &str) -> Option<String> {
    let output = std::process::Command::new("fc-match")
        .args(["-f", "%{family[0]}", generic])
        .output()
        .ok()?;
    let family = String::from_utf8(output.stdout).ok()?;
    (output.status.success() && !family.is_empty()).then_some(family)
}

fn parse_svg(
    data: &[u8],
    resources_dir: Option<&Path>,
    style_sheet: Option<&str>,
) -> Result<usvg::Tree> {
    let options = usvg::Options {
        resources_dir: resources_dir.map(Path::to_path_buf),
        fontdb: fontdb(),
        font_family: "sans-serif".to_string(),
        style_sheet: style_sheet.map(str::to_string),
        ..Default::default()
    };
    Ok(usvg::Tree::from_data(data, &options)?)
}

fn pixmap_to_image(pixmap: &tiny_skia::Pixmap) -> RgbaImage {
    let mut image = RgbaImage::new(pixmap.width(), pixmap.height());
    for (target, source) in image.pixels_mut().zip(pixmap.pixels()) {
        let color = source.demultiply();
        *target = Rgba([color.red(), color.green(), color.blue(), color.alpha()]);
    }
    image
}

/// Rasterize an SVG to fit a width x height box, keeping its aspect ratio.
pub fn render_svg(
    data: &[u8],
    width: u32,
    height: u32,
    resources_dir: Option<&Path>,
) -> Result<RgbaImage> {
    render_svg_styled(data, width, height, resources_dir, None)
}

/// Rasterize an SVG with an extra style sheet applied.
pub fn render_svg_styled(
    data: &[u8],
    width: u32,
    height: u32,
    resources_dir: Option<&Path>,
    style_sheet: Option<&str>,
) -> Result<RgbaImage> {
    let tree = parse_svg(data, resources_dir, style_sheet)?;
    let size = tree.size();
    let scale = (width as f32 / size.width()).min(height as f32 / size.height());
    let transform = tiny_skia::Transform::from_translate(
        (width as f32 - size.width() * scale) / 2.0,
        (height as f32 - size.height() * scale) / 2.0,
    )
    .pre_scale(scale, scale);
    let mut pixmap = tiny_skia::Pixmap::new(width, height).context("Empty SVG raster")?;
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    Ok(pixmap_to_image(&pixmap))
}

pub fn render_svg_file(path: &Path, width: u32, height: u32) -> Result<RgbaImage> {
    let data = std::fs::read(path)?;
    render_svg(&data, width, height, path.parent())
        .with_context(|| format!("Rendering {}", path.display()))
}

/// Decode a raster image (PNG, ICO, JPEG, GIF, WebP, BMP). For an ICO this
/// is its largest image.
pub fn decode(data: &[u8]) -> Result<RgbaImage> {
    Ok(image::load_from_memory(data)?.to_rgba8())
}

pub fn encode_png(image: &RgbaImage) -> Vec<u8> {
    let mut buffer = Cursor::new(Vec::new());
    image
        .write_to(&mut buffer, ImageFormat::Png)
        .expect("PNG encoding to memory cannot fail");
    buffer.into_inner()
}

pub fn save_png(image: &RgbaImage, path: &Path) -> Result<()> {
    std::fs::write(path, encode_png(image)).with_context(|| format!("Writing {}", path.display()))
}

/// The (width, height) of PNG data, read from its header.
pub fn png_size(data: &[u8]) -> Result<(u32, u32)> {
    const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
    let index = data
        .windows(SIGNATURE.len())
        .position(|window| window == SIGNATURE)
        .ok_or_else(|| anyhow!("Not a PNG"))?;
    let start = index + 8;
    if data.len() < start + 24 {
        bail!("Truncated PNG");
    }
    let width = u32::from_be_bytes(data[start + 8..start + 12].try_into().unwrap());
    let height = u32::from_be_bytes(data[start + 12..start + 16].try_into().unwrap());
    Ok((width, height))
}

fn extension(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

pub fn is_svg(path: &Path) -> bool {
    extension(path) == "svg"
}

/// A PNG or SVG file as PNG data of target_px x target_px pixels. PNGs of
/// the right size are passed through untouched.
pub fn collect_image(path: &Path, target_px: u32) -> Result<Vec<u8>> {
    if !path.is_file() {
        bail!("Not a file: {}", path.display());
    }
    match extension(path).as_str() {
        "png" => {
            let data = std::fs::read(path)?;
            let (width, height) = png_size(&data)?;
            if (width, height) == (target_px, target_px) {
                return Ok(data);
            }
            log::debug!(
                "{}: PNG is {width}x{height}; rescaling to {target_px}x{target_px}",
                path.display()
            );
            let image = decode(&data)?;
            let resized = imageops::resize(&image, target_px, target_px, FilterType::Lanczos3);
            Ok(encode_png(&resized))
        }
        "svg" => Ok(encode_png(&render_svg_file(path, target_px, target_px)?)),
        _ => bail!("Not a PNG or SVG file: {}", path.display()),
    }
}

/// The smallest rectangle (x, y, width, height) holding all visible pixels.
pub fn alpha_bbox(image: &RgbaImage) -> Option<(u32, u32, u32, u32)> {
    let (mut left, mut top, mut right, mut bottom) = (u32::MAX, u32::MAX, 0, 0);
    for (x, y, pixel) in image.enumerate_pixels() {
        if pixel[3] != 0 {
            left = left.min(x);
            top = top.min(y);
            right = right.max(x);
            bottom = bottom.max(y);
        }
    }
    (left != u32::MAX).then(|| (left, top, right - left + 1, bottom - top + 1))
}

/// Grow the visible area of an alpha mask by `radius` pixels in every
/// direction (a square max filter).
pub fn dilate(mask: &[u8], width: usize, height: usize, radius: usize) -> Vec<u8> {
    let pass = |source: &[u8], step: usize, length: usize, lines: usize, line_step: usize| {
        let mut target = vec![0u8; source.len()];
        for line in 0..lines {
            let base = line * line_step;
            for i in 0..length {
                let low = i.saturating_sub(radius);
                let high = (i + radius).min(length - 1);
                target[base + i * step] = (low..=high)
                    .map(|j| source[base + j * step])
                    .max()
                    .unwrap_or(0);
            }
        }
        target
    };
    let horizontal = pass(mask, 1, width, height, width);
    pass(&horizontal, width, height, width, 1)
}

/// Parse a "#rrggbb" or "#rrggbbaa" colour.
pub fn parse_color(color: &str) -> Rgba<u8> {
    let hex = color.trim_start_matches('#');
    let channel =
        |i: usize| u8::from_str_radix(hex.get(i..i + 2).unwrap_or("ff"), 16).unwrap_or(255);
    Rgba([
        channel(0),
        channel(2),
        channel(4),
        if hex.len() >= 8 { channel(6) } else { 255 },
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dilation_grows_a_point_into_a_square() {
        let mut mask = vec![0u8; 25];
        mask[12] = 200;
        let grown = dilate(&mask, 5, 5, 1);
        let lit: Vec<usize> = (0..25).filter(|&i| grown[i] == 200).collect();
        assert_eq!(lit, [6, 7, 8, 11, 12, 13, 16, 17, 18]);
    }

    #[test]
    fn svg_renders_centered_at_requested_size() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10"><rect width="20" height="10" fill="#f00"/></svg>"##;
        let image = render_svg(svg, 40, 40, None).unwrap();
        assert_eq!(image.dimensions(), (40, 40));
        assert_eq!(alpha_bbox(&image), Some((0, 10, 40, 20)));
        assert_eq!(image.get_pixel(20, 20), &Rgba([255, 0, 0, 255]));
    }

    #[test]
    fn parses_colors() {
        assert_eq!(parse_color("#719cd666"), Rgba([0x71, 0x9c, 0xd6, 0x66]));
        assert_eq!(parse_color("#c58fff"), Rgba([0xc5, 0x8f, 0xff, 255]));
    }
}
