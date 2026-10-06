//! Create Private Use Area icon fonts from a CBDT/CBLC bitmap colour font.
//!
//! The output is written from scratch: the base font contributes its
//! metrics, its bitmap strike's size and (unless removed) its own glyphs,
//! and every image becomes a PNG glyph mapped to a PUA code point.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use read_fonts::tables::bitmap::BitmapLocation;
use read_fonts::types::{GlyphId, Tag};
use read_fonts::{FontRef, TableProvider};

use crate::raster;

pub const PUA_START: u32 = 0xE000;

const PNG_IMAGE_FORMAT: u16 = 17;

fn is_pua(codepoint: u32) -> bool {
    (0xE000..=0xF8FF).contains(&codepoint) || (0x100000..=0x10FFFD).contains(&codepoint)
}

/// Python-style rounding: halves go to the even neighbour.
fn round(value: f64) -> i64 {
    value.round_ties_even() as i64
}

fn clamp_i8(value: f64) -> i8 {
    round(value).clamp(-128, 127) as i8
}

fn clamp_u8(value: f64) -> u8 {
    round(value).clamp(0, 255) as u8
}

/// Settings for building an icon font.
#[derive(Debug, Clone)]
pub struct FontBuilder {
    pub family_name: String,
    pub pua_start: u32,
    /// Keep only .notdef and space from the base font.
    pub remove_original_symbols: bool,
    /// Code points in image order; allocated from pua_start when absent.
    pub codepoints: Option<Vec<u32>>,
    /// Substituted for images that cannot be decoded. Without one, decoding
    /// errors are returned.
    pub fallback_image: Option<PathBuf>,
    /// Per-image advance widths as fractions of the em; e.g. 0 makes the
    /// next glyph draw over this one.
    pub advance_fractions: Option<Vec<f64>>,
    /// Per-image downward shifts as fractions of the em.
    pub drop_fractions: Option<Vec<f64>>,
    /// The font's version string (name ID 5).
    pub version: Option<String>,
}

impl Default for FontBuilder {
    fn default() -> Self {
        Self {
            family_name: "MyCreatedIconFont".to_string(),
            pua_start: PUA_START,
            remove_original_symbols: false,
            codepoints: None,
            fallback_image: None,
            advance_fractions: None,
            drop_fractions: None,
            version: None,
        }
    }
}

/// A built font and the code points its images were mapped to.
pub struct BuiltFont {
    pub data: Vec<u8>,
    pub codepoints: Vec<u32>,
}

struct Glyph {
    advance: u16,
    /// A complete CBDT format 17 record: small metrics, length and PNG.
    bitmap: Option<Vec<u8>>,
}

/// The size of the base font's first bitmap strike.
pub fn strike_size(base_font: &[u8]) -> Result<u32> {
    let font = FontRef::new(base_font)?;
    let cblc = font
        .cblc()
        .context("Base font must be CBDT/CBLC (like NotoColorEmoji.ttf)")?;
    let size = cblc
        .bitmap_sizes()
        .first()
        .context("Base font has no bitmap strike")?;
    Ok(size.ppem_y() as u32)
}

/// Load and normalize images to target_px squares, substituting the fallback
/// image for any that cannot be read.
pub fn collect_images(
    paths: &[PathBuf],
    target_px: u32,
    fallback: Option<&Path>,
) -> Result<Vec<Vec<u8>>> {
    let results = crate::parallel_map(paths, |path| raster::collect_image(path, target_px));
    let mut fallback_data: Option<Vec<u8>> = None;
    paths
        .iter()
        .zip(results)
        .map(|(path, result)| match result {
            Ok(data) => Ok(data),
            Err(error) => {
                let Some(fallback) = fallback.filter(|f| *f != path.as_path()) else {
                    return Err(error);
                };
                log::warn!(
                    "Could not decode icon {} ({error:#}); using {}",
                    path.display(),
                    fallback.display()
                );
                if fallback_data.is_none() {
                    fallback_data = Some(raster::collect_image(fallback, target_px)?);
                }
                Ok(fallback_data.clone().unwrap())
            }
        })
        .collect()
}

impl FontBuilder {
    fn validate_codepoints(&self, count: usize) -> Result<()> {
        let Some(codepoints) = &self.codepoints else {
            return Ok(());
        };
        if codepoints.len() != count {
            bail!("Each image must have exactly one codepoint");
        }
        if !codepoints.iter().all(|&cp| is_pua(cp)) {
            bail!("Codepoints must be integers in U+E000..U+F8FF or U+100000..U+10FFFD");
        }
        if codepoints.iter().collect::<HashSet<_>>().len() != codepoints.len() {
            bail!("Codepoints must be unique");
        }
        Ok(())
    }

    /// Build a font from image files.
    pub fn build(&self, base_font: &[u8], image_paths: &[PathBuf]) -> Result<BuiltFont> {
        self.validate_codepoints(image_paths.len())?;
        let target = strike_size(base_font)?;
        let images = collect_images(image_paths, target, self.fallback_image.as_deref())?;
        if images.is_empty() {
            bail!("No valid images found");
        }
        let names: Vec<String> = image_paths
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        self.build_from_pngs(base_font, &images, &names)
    }

    /// Build a font from PNGs already at the base font's strike size.
    pub fn build_from_pngs(
        &self,
        base_font: &[u8],
        images: &[Vec<u8>],
        names: &[String],
    ) -> Result<BuiltFont> {
        self.validate_codepoints(images.len())?;
        let font = FontRef::new(base_font).context("Reading base font")?;
        let (cblc, cbdt) = match (font.cblc(), font.cbdt()) {
            (Ok(cblc), Ok(cbdt)) => (cblc, cbdt),
            _ => bail!("Base font must be CBDT/CBLC (like NotoColorEmoji.ttf)"),
        };
        let strike = cblc
            .bitmap_sizes()
            .first()
            .context("Base font has no bitmap strike")?;
        let (ppem_x, ppem_y) = (strike.ppem_x() as u32, strike.ppem_y() as u32);
        if ppem_x != ppem_y {
            bail!("Non-square CBDT strike not supported");
        }
        let ppem = ppem_y as f64;
        let upem = font.head()?.units_per_em() as f64;
        let os2 = font.os2()?;
        let hmtx = font.hmtx()?;
        let num_base_glyphs = font.maxp()?.num_glyphs() as u32;
        let base_advance = |gid: u32| {
            hmtx.advance(GlyphId::new(gid))
                .or_else(|| hmtx.h_metrics().last().map(|m| m.advance()))
                .unwrap_or(0)
        };

        let base_cmap: BTreeMap<u32, u32> = font
            .cmap()
            .ok()
            .and_then(|cmap| cmap.best_subtable())
            .map(|(_, _, subtable)| {
                subtable
                    .iter()
                    .map(|(cp, gid)| (cp, gid.to_u32()))
                    .collect()
            })
            .unwrap_or_default();
        let space = base_cmap.get(&0x20).copied();

        // Base glyphs carried over, keeping their glyph ids.
        let mut glyphs: Vec<Glyph> = Vec::new();
        let mut cmap: BTreeMap<u32, u32> = BTreeMap::new();
        let base_bitmap = |gid: u32| -> Option<Vec<u8>> {
            let location: BitmapLocation = strike
                .location(cblc.offset_data(), GlyphId::new(gid))
                .ok()?;
            if location.is_empty() || location.format != PNG_IMAGE_FORMAT {
                return None;
            }
            let bytes = cbdt.offset_data().as_bytes();
            bytes
                .get(location.data_offset..location.data_offset + location.data_size)
                .map(<[u8]>::to_vec)
        };
        if self.remove_original_symbols {
            glyphs.push(Glyph {
                advance: base_advance(0),
                bitmap: None,
            });
            if let Some(space) = space.filter(|&gid| gid != 0) {
                // The space takes no room, as in fonts made by earlier versions.
                glyphs.push(Glyph {
                    advance: 0,
                    bitmap: base_bitmap(space),
                });
                cmap.insert(0x20, 1);
            }
        } else {
            for gid in 0..num_base_glyphs {
                glyphs.push(Glyph {
                    advance: base_advance(gid),
                    bitmap: base_bitmap(gid),
                });
            }
            cmap.extend(base_cmap.iter());
        }

        let space_advance = space.map(|gid| glyphs.get(gid as usize).map_or(0, |g| g.advance));
        let reference_advance = match space_advance {
            Some(advance) if advance != 0 && !self.remove_original_symbols => advance as f64,
            _ => upem,
        };

        let codepoints: Vec<u32> = match &self.codepoints {
            Some(codepoints) => {
                if codepoints.iter().any(|cp| cmap.contains_key(cp)) {
                    bail!("Requested codepoint is already mapped in the font");
                }
                codepoints.clone()
            }
            None => {
                let available: Vec<u32> = (self.pua_start..=0xF8FF)
                    .filter(|cp| !cmap.contains_key(cp))
                    .take(images.len())
                    .collect();
                if available.len() < images.len() {
                    bail!("Not enough free Private Use Area code points");
                }
                available
            }
        };

        let ascender_px = round(os2.s_typo_ascender() as f64 * ppem / upem) as f64;
        let descender_px = round((os2.s_typo_descender() as f64).abs() * ppem / upem) as f64;
        let line_center = (ascender_px - descender_px) / 2.0;

        for (index, png) in images.iter().enumerate() {
            let name = names.get(index).map(String::as_str).unwrap_or("image");
            let size = raster::png_size(png)?;
            if size != (ppem_y, ppem_y) {
                bail!(
                    "Image {name} is {}x{} but expected {ppem_y}x{ppem_y}",
                    size.0,
                    size.1
                );
            }
            let advance_fraction = self
                .advance_fractions
                .as_ref()
                .and_then(|f| f.get(index))
                .copied()
                .unwrap_or(1.0);
            let drop_fraction = self
                .drop_fractions
                .as_ref()
                .and_then(|f| f.get(index))
                .copied()
                .unwrap_or(0.0);
            let advance = round(reference_advance * advance_fraction);
            let advance_px = round(advance as f64 * ppem / upem);
            let bearing_x = ((advance_px - ppem_y as i64) as f64 / 2.0)
                .round_ties_even()
                .max(0.0);
            let bearing_y = line_center + ppem / 2.0 - drop_fraction * ppem;

            let mut record = Vec::with_capacity(9 + png.len());
            record.push(ppem_y as u8); // height
            record.push(ppem_y as u8); // width
            record.push(clamp_i8(bearing_x) as u8);
            record.push(clamp_i8(bearing_y) as u8);
            record.push(clamp_u8(advance_px as f64));
            record.extend_from_slice(&(png.len() as u32).to_be_bytes());
            record.extend_from_slice(png);

            let codepoint = codepoints[index];
            cmap.insert(codepoint, glyphs.len() as u32);
            glyphs.push(Glyph {
                advance: advance.clamp(0, u16::MAX as i64) as u16,
                bitmap: Some(record),
            });
            log::debug!("[+] {name} -> U+{codepoint:04X}");
        }
        if glyphs.len() > u16::MAX as usize {
            bail!("Too many glyphs for one font");
        }

        let mut tables: BTreeMap<[u8; 4], Vec<u8>> = BTreeMap::new();
        let raw = |tag: &[u8; 4]| -> Result<Vec<u8>> {
            Ok(font
                .table_data(Tag::new(tag))
                .ok_or_else(|| anyhow!("Base font has no {} table", String::from_utf8_lossy(tag)))?
                .as_bytes()
                .to_vec())
        };

        let mut head = raw(b"head")?;
        head[8..12].fill(0); // checkSumAdjustment, set once the font is assembled
        tables.insert(*b"head", head);

        let mut hhea = raw(b"hhea")?;
        let advance_max = glyphs.iter().map(|g| g.advance).max().unwrap_or(0);
        hhea[10..12].copy_from_slice(&advance_max.to_be_bytes());
        hhea[34..36].copy_from_slice(&(glyphs.len() as u16).to_be_bytes());
        tables.insert(*b"hhea", hhea);

        let mut maxp = raw(b"maxp")?;
        maxp[4..6].copy_from_slice(&(glyphs.len() as u16).to_be_bytes());
        tables.insert(*b"maxp", maxp);

        let mut os2_data = raw(b"OS/2")?;
        if os2_data.len() >= 68 {
            let first = cmap.keys().next().copied().unwrap_or(0).min(0xFFFF) as u16;
            let last = cmap.keys().last().copied().unwrap_or(0).min(0xFFFF) as u16;
            os2_data[64..66].copy_from_slice(&first.to_be_bytes());
            os2_data[66..68].copy_from_slice(&last.to_be_bytes());
        }
        tables.insert(*b"OS/2", os2_data);

        let mut post = raw(b"post")?;
        post.truncate(32);
        post[0..4].copy_from_slice(&0x0003_0000u32.to_be_bytes());
        tables.insert(*b"post", post);

        let mut hmtx_data = Vec::with_capacity(glyphs.len() * 4);
        for glyph in &glyphs {
            hmtx_data.extend_from_slice(&glyph.advance.to_be_bytes());
            hmtx_data.extend_from_slice(&0i16.to_be_bytes());
        }
        tables.insert(*b"hmtx", hmtx_data);

        tables.insert(*b"cmap", build_cmap(&cmap));
        tables.insert(*b"name", self.build_name(&codepoints));

        let cblc_data = cblc.offset_data().as_bytes();
        let strike_record = cblc_data.get(8..56).context("Truncated CBLC table")?;
        let (cblc_out, cbdt_out) = build_bitmap_tables(&glyphs, strike_record)?;
        tables.insert(*b"CBLC", cblc_out);
        tables.insert(*b"CBDT", cbdt_out);

        if !self.remove_original_symbols
            && let Some(gsub) = font.table_data(Tag::new(b"GSUB"))
        {
            // Glyph ids of the base font are unchanged, so its substitutions
            // (e.g. emoji sequences) still apply.
            tables.insert(*b"GSUB", gsub.as_bytes().to_vec());
        }

        Ok(BuiltFont {
            data: assemble(tables),
            codepoints,
        })
    }

    fn build_name(&self, codepoints: &[u32]) -> Vec<u8> {
        let family = &self.family_name;
        let subfamily = "Regular";
        let sample: String = codepoints
            .iter()
            .take(64)
            .filter_map(|&cp| char::from_u32(cp))
            .map(String::from)
            .collect::<Vec<_>>()
            .join(" ");
        let sample = if sample.is_empty() {
            "Private Use Area".to_string()
        } else {
            sample
        };
        let mut names: Vec<(u16, String)> = vec![
            (1, family.clone()),
            (2, subfamily.to_string()),
            (3, format!("{family} {subfamily}; {}", std::process::id())),
            (4, format!("{family} {subfamily}")),
            (6, format!("{family}-{subfamily}")),
            (16, family.clone()),
            (17, subfamily.to_string()),
            (19, sample),
        ];
        if let Some(version) = &self.version {
            names.push((5, version.clone()));
        }
        names.sort_by_key(|(id, _)| *id);
        build_name_table(&names)
    }
}

/// A name table with each string for the Unicode and Windows platforms.
fn build_name_table(names: &[(u16, String)]) -> Vec<u8> {
    let platforms: [(u16, u16, u16); 2] = [(0, 4, 0), (3, 1, 0x409)];
    let mut strings: Vec<u8> = Vec::new();
    let mut offsets: HashMap<u16, (u16, u16)> = HashMap::new();
    for (id, value) in names {
        let encoded: Vec<u8> = value.encode_utf16().flat_map(u16::to_be_bytes).collect();
        offsets.insert(*id, (strings.len() as u16, encoded.len() as u16));
        strings.extend(encoded);
    }
    let count = (platforms.len() * names.len()) as u16;
    let mut table = Vec::new();
    table.extend_from_slice(&0u16.to_be_bytes());
    table.extend_from_slice(&count.to_be_bytes());
    table.extend_from_slice(&(6 + 12 * count).to_be_bytes());
    for (platform, encoding, language) in platforms {
        for (id, _) in names {
            let (offset, length) = offsets[id];
            for value in [platform, encoding, language, *id, length, offset] {
                table.extend_from_slice(&value.to_be_bytes());
            }
        }
    }
    table.extend(strings);
    table
}

/// A cmap with one format 12 subtable covering every mapping.
fn build_cmap(mapping: &BTreeMap<u32, u32>) -> Vec<u8> {
    let mut groups: Vec<(u32, u32, u32)> = Vec::new();
    for (&codepoint, &gid) in mapping {
        if let Some(last) = groups.last_mut()
            && last.1 + 1 == codepoint
            && last.2 + (codepoint - last.0) == gid
        {
            last.1 = codepoint;
            continue;
        }
        groups.push((codepoint, codepoint, gid));
    }
    let mut table = Vec::new();
    table.extend_from_slice(&0u16.to_be_bytes()); // version
    table.extend_from_slice(&1u16.to_be_bytes()); // numTables
    table.extend_from_slice(&3u16.to_be_bytes()); // Windows
    table.extend_from_slice(&10u16.to_be_bytes()); // Unicode full repertoire
    table.extend_from_slice(&12u32.to_be_bytes()); // subtable offset
    table.extend_from_slice(&12u16.to_be_bytes());
    table.extend_from_slice(&0u16.to_be_bytes());
    table.extend_from_slice(&(16 + 12 * groups.len() as u32).to_be_bytes());
    table.extend_from_slice(&0u32.to_be_bytes()); // language
    table.extend_from_slice(&(groups.len() as u32).to_be_bytes());
    for (start, end, gid) in groups {
        for value in [start, end, gid] {
            table.extend_from_slice(&value.to_be_bytes());
        }
    }
    table
}

/// CBLC and CBDT tables with one strike and one index subtable (format 1)
/// spanning all glyphs that have bitmaps. `strike_record` is the base
/// font's BitmapSize record, whose line metrics and depth are kept.
fn build_bitmap_tables(glyphs: &[Glyph], strike_record: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
    let first = glyphs
        .iter()
        .position(|g| g.bitmap.is_some())
        .context("No bitmap glyphs")?;
    let last = glyphs.iter().rposition(|g| g.bitmap.is_some()).unwrap();

    let mut cbdt = vec![0, 3, 0, 0]; // version 3.0
    let image_data_offset = cbdt.len() as u32;
    let mut offsets = vec![0u32];
    for glyph in &glyphs[first..=last] {
        if let Some(bitmap) = &glyph.bitmap {
            cbdt.extend_from_slice(bitmap);
        }
        offsets.push(cbdt.len() as u32 - image_data_offset);
    }

    let mut subtable = Vec::new();
    subtable.extend_from_slice(&1u16.to_be_bytes()); // indexFormat
    subtable.extend_from_slice(&PNG_IMAGE_FORMAT.to_be_bytes());
    subtable.extend_from_slice(&image_data_offset.to_be_bytes());
    for offset in offsets {
        subtable.extend_from_slice(&offset.to_be_bytes());
    }
    let mut index = Vec::new();
    index.extend_from_slice(&(first as u16).to_be_bytes());
    index.extend_from_slice(&(last as u16).to_be_bytes());
    index.extend_from_slice(&8u32.to_be_bytes()); // offset from the array start
    index.extend(subtable);

    let mut cblc = Vec::new();
    cblc.extend_from_slice(&[0, 3, 0, 0]); // version 3.0
    cblc.extend_from_slice(&1u32.to_be_bytes()); // numSizes
    let mut record = strike_record.to_vec();
    record[0..4].copy_from_slice(&56u32.to_be_bytes()); // indexSubTableArrayOffset
    record[4..8].copy_from_slice(&(index.len() as u32).to_be_bytes());
    record[8..12].copy_from_slice(&1u32.to_be_bytes()); // numberOfIndexSubTables
    record[40..42].copy_from_slice(&(first as u16).to_be_bytes());
    record[42..44].copy_from_slice(&(last as u16).to_be_bytes());
    cblc.extend(record);
    cblc.extend(index);
    Ok((cblc, cbdt))
}

fn checksum(data: &[u8]) -> u32 {
    data.chunks(4).fold(0u32, |sum, chunk| {
        let mut word = [0u8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        sum.wrapping_add(u32::from_be_bytes(word))
    })
}

/// Lay out tables (sorted by tag) into an sfnt file.
fn assemble(tables: BTreeMap<[u8; 4], Vec<u8>>) -> Vec<u8> {
    let count = tables.len() as u16;
    let entry_selector = 15 - count.leading_zeros() as u16;
    let search_range = (1u16 << entry_selector) * 16;
    let mut font = Vec::new();
    font.extend_from_slice(&0x0001_0000u32.to_be_bytes());
    for value in [
        count,
        search_range,
        entry_selector,
        count * 16 - search_range,
    ] {
        font.extend_from_slice(&value.to_be_bytes());
    }
    let mut offset = 12 + 16 * tables.len();
    let mut head_offset = 0;
    for (tag, data) in &tables {
        if tag == b"head" {
            head_offset = offset;
        }
        font.extend_from_slice(tag);
        font.extend_from_slice(&checksum(data).to_be_bytes());
        font.extend_from_slice(&(offset as u32).to_be_bytes());
        font.extend_from_slice(&(data.len() as u32).to_be_bytes());
        offset += data.len().next_multiple_of(4);
    }
    for data in tables.values() {
        font.extend_from_slice(data);
        font.resize(font.len().next_multiple_of(4), 0);
    }
    let adjustment = 0xB1B0_AFBAu32.wrapping_sub(checksum(&font));
    font[head_offset + 8..head_offset + 12].copy_from_slice(&adjustment.to_be_bytes());
    font
}

/// What an installed font provides, as far as the daemon is concerned.
#[derive(Debug, Default)]
pub struct FontInfo {
    pub family: Option<String>,
    pub version: Option<String>,
    /// Code points whose glyphs have a bitmap in the first strike.
    pub bitmap_codepoints: HashSet<u32>,
}

fn name_string(font: &FontRef, name_id: u16) -> Option<String> {
    let name = font.name().ok()?;
    let records = name.name_record();
    let english = records.iter().find(|r| {
        r.name_id().to_u16() == name_id && r.platform_id() == 3 && r.language_id() == 0x409
    });
    let record = english.or_else(|| records.iter().find(|r| r.name_id().to_u16() == name_id))?;
    Some(record.string(name.string_data()).ok()?.chars().collect())
}

pub fn read_font_info(path: &Path) -> Result<FontInfo> {
    let data = std::fs::read(path)?;
    let font = FontRef::new(&data)?;
    let mut info = FontInfo {
        family: name_string(&font, 1),
        version: name_string(&font, 5),
        ..Default::default()
    };
    let cblc = font.cblc()?;
    let strike = cblc
        .bitmap_sizes()
        .first()
        .context("Font has no bitmap strike")?;
    let Some((_, _, cmap)) = font.cmap()?.best_subtable() else {
        return Ok(info);
    };
    for (codepoint, gid) in cmap.iter() {
        if strike
            .location(cblc.offset_data(), gid)
            .is_ok_and(|l| !l.is_empty())
        {
            info.bitmap_codepoints.insert(codepoint);
        }
    }
    Ok(info)
}

/// The version string (name ID 5) of a font file, if it can be read.
pub fn font_version(path: &Path) -> Option<String> {
    let data = std::fs::read(path).ok()?;
    name_string(&FontRef::new(&data).ok()?, 5)
}

/// The PNG of a code point's glyph in a CBDT font, e.g. an emoji.
pub fn glyph_png(font_data: &[u8], codepoint: u32) -> Option<Vec<u8>> {
    let font = FontRef::new(font_data).ok()?;
    let gid = font.cmap().ok()?.map_codepoint(codepoint)?;
    let cblc = font.cblc().ok()?;
    let location = cblc
        .bitmap_sizes()
        .first()?
        .location(cblc.offset_data(), gid)
        .ok()?;
    let start = location.data_offset + 9;
    let end = location.data_offset + location.data_size;
    (location.format == PNG_IMAGE_FORMAT && start <= end)
        .then(|| {
            font.cbdt()
                .ok()?
                .offset_data()
                .as_bytes()
                .get(start..end)
                .map(<[u8]>::to_vec)
        })
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::BASE_FONT;
    use image::{Rgba, RgbaImage};

    fn write_png(path: &Path, size: u32, color: [u8; 4]) {
        raster::save_png(&RgbaImage::from_pixel(size, size, Rgba(color)), path).unwrap();
    }

    #[test]
    fn default_allocation_remains_sequential() {
        let dir = tempfile::tempdir().unwrap();
        let paths: Vec<PathBuf> = (0..3)
            .map(|i| {
                let path = dir.path().join(format!("{i}.png"));
                write_png(&path, 32, [i * 80, 0, 0, 255]);
                path
            })
            .collect();
        let builder = FontBuilder {
            remove_original_symbols: true,
            ..Default::default()
        };
        let built = builder.build(BASE_FONT, &paths).unwrap();
        assert_eq!(built.codepoints, [0xE000, 0xE001, 0xE002]);
        let output = dir.path().join("out.ttf");
        std::fs::write(&output, &built.data).unwrap();
        let info = read_font_info(&output).unwrap();
        assert_eq!(info.family.as_deref(), Some("MyCreatedIconFont"));
        assert_eq!(
            info.bitmap_codepoints,
            HashSet::from([0xE000, 0xE001, 0xE002])
        );
    }

    #[test]
    fn rejects_invalid_explicit_codepoints() {
        let paths = vec![PathBuf::from("a.png"), PathBuf::from("b.png")];
        for codepoints in [vec![0xE000], vec![0x41, 0xE001], vec![0xE000, 0xE000]] {
            let builder = FontBuilder {
                codepoints: Some(codepoints),
                ..Default::default()
            };
            assert!(builder.build(BASE_FONT, &paths).is_err());
        }
    }

    #[test]
    fn invalid_image_is_replaced_without_losing_its_codepoint() {
        let dir = tempfile::tempdir().unwrap();
        let broken = dir.path().join("broken.svg");
        std::fs::write(&broken, "not an svg").unwrap();
        let fallback = dir.path().join("fallback.png");
        write_png(&fallback, 109, [0, 0, 255, 255]);
        let builder = FontBuilder {
            remove_original_symbols: true,
            codepoints: Some(vec![0xE005]),
            fallback_image: Some(fallback.clone()),
            ..Default::default()
        };
        let built = builder.build(BASE_FONT, &[broken]).unwrap();
        assert_eq!(
            glyph_png(&built.data, 0xE005).unwrap(),
            std::fs::read(&fallback).unwrap()
        );
    }

    #[test]
    fn keeps_original_glyphs_unless_removed() {
        let dir = tempfile::tempdir().unwrap();
        let icon = dir.path().join("icon.png");
        write_png(&icon, 109, [0, 255, 0, 255]);
        let kept = FontBuilder::default()
            .build(BASE_FONT, std::slice::from_ref(&icon))
            .unwrap();
        assert!(glyph_png(&kept.data, 0x1F600).is_some());
        assert!(glyph_png(&kept.data, kept.codepoints[0]).is_some());
        let removed = FontBuilder {
            remove_original_symbols: true,
            ..Default::default()
        }
        .build(BASE_FONT, &[icon])
        .unwrap();
        assert!(glyph_png(&removed.data, 0x1F600).is_none());
    }

    #[test]
    fn glyph_metrics_match_earlier_fonts() {
        let dir = tempfile::tempdir().unwrap();
        let icon = dir.path().join("icon.png");
        write_png(&icon, 109, [0, 255, 0, 255]);
        let builder = FontBuilder {
            remove_original_symbols: true,
            codepoints: Some(vec![0xEC00, 0x10B000]),
            advance_fractions: Some(vec![1.0, 0.5]),
            drop_fractions: Some(vec![0.0, 0.08]),
            ..Default::default()
        };
        let built = builder.build(BASE_FONT, &[icon.clone(), icon]).unwrap();
        let font = FontRef::new(&built.data).unwrap();
        let cblc = font.cblc().unwrap();
        let cbdt = font.cbdt().unwrap();
        let strike = &cblc.bitmap_sizes()[0];
        let metrics = |cp: u32| {
            let gid = font.cmap().unwrap().map_codepoint(cp).unwrap();
            let location = strike.location(cblc.offset_data(), gid).unwrap();
            let data = cbdt.data(&location).unwrap();
            let read_fonts::tables::bitmap::BitmapMetrics::Small(m) = data.metrics else {
                panic!("expected small metrics")
            };
            (m.height, m.width, m.bearing_x(), m.bearing_y(), m.advance)
        };
        assert_eq!(metrics(0xEC00), (109, 109, 0, 92, 109));
        assert_eq!(metrics(0x10B000), (109, 109, 0, 83, 54));
        let hmtx = font.hmtx().unwrap();
        assert_eq!(hmtx.advance(GlyphId::new(2)), Some(2048));
        assert_eq!(hmtx.advance(GlyphId::new(3)), Some(1024));
    }
}
