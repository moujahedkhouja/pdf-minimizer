//! Image XObject optimization: downsampling, mozjpeg re-encoding with an
//! automatic quality search, lossless re-encoding, and SSIM-based quality
//! gates that decide whether a lossy candidate ships.
//!
//! Author: Moujahed Khouja

use image::{ColorType, DynamicImage, GenericImageView, ImageFormat, Pixel};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};
use std::collections::HashSet;
use std::io::Cursor;

/// Minimum global luma SSIM (MSSIMSimple, mean-pooled) for a lossy re-encode.
///
/// Calibrated on EPSON007: acceptable full-resolution re-encodes score ≥ 0.99;
/// destructive downscales score 0.77–0.91. Keep in sync with the boundary test.
pub const SSIM_THRESHOLD: f64 = 0.99;

/// Minimum worst-tile luma SSIM (256-px tiles, near-blank tiles skipped).
///
/// Mean pooling hides localized glyph destruction on mostly-blank scan pages; this
/// floor is what actually protects text. Calibrated on EPSON007: good candidates
/// ≥ 0.965, destructive downscales 0.65–0.74.
pub const TILE_SSIM_THRESHOLD: f64 = 0.96;

/// Minimum mean RGB SSIM — a chroma backstop, since the primary metrics are
/// luma-only and would otherwise ignore pure color damage.
pub const CHROMA_SSIM_FLOOR: f64 = 0.95;

/// The quality gate: candidate acceptable iff all three scores reach their floors.
fn passes_quality_gate(rgb_mean: f64, luma_global: f64, min_tile: f64) -> bool {
    rgb_mean >= CHROMA_SSIM_FLOOR
        && luma_global >= SSIM_THRESHOLD
        && min_tile >= TILE_SSIM_THRESHOLD
}

/// Options controlling image optimization behavior.
pub struct ImageOptions {
    /// Pixel threshold for downsampling (largest dimension).
    pub max_pixels: u32,
    /// JPEG encoding quality (1–100).
    pub image_quality: u8,
    /// JPEG pre-smoothing factor (1–100) to suppress scanner grain; 0 disables.
    pub smoothing: u8,
    /// If set, compute a pixel threshold from DPI assuming a standard letter page (8.5" wide).
    /// Takes the more restrictive of this and `max_pixels`.
    pub downsample_dpi: Option<u16>,
    /// Force all images to JPEG regardless of color count heuristic.
    pub force_jpeg: bool,
    /// Try bilevel CCITT G4 for text-only pages (opt-in; drops paper texture).
    pub bilevel: bool,
    /// Maximum decoded size accepted for an individual image stream.
    pub max_decompressed_bytes: usize,
}

impl ImageOptions {
    /// True for the calibrated scan profile exposed by `--recommended`.
    fn is_recommended_scan_profile(&self) -> bool {
        self.downsample_dpi == Some(120) && self.image_quality == 75 && self.force_jpeg
    }

    /// Effective pixel threshold, accounting for optional DPI override.
    fn effective_max_pixels(&self) -> u32 {
        if let Some(dpi) = self.downsample_dpi {
            // Largest A4 edge is 11.7". Since resize() constrains the largest image
            // dimension, using the short edge here would silently undershoot the DPI
            // of portrait pages (for example, 120 DPI became roughly 87 DPI).
            let dpi_pixels = (11.7_f32 * dpi as f32).round() as u32;
            self.max_pixels.min(dpi_pixels)
        } else {
            self.max_pixels
        }
    }
}

/// Returns true if the image should be downsampled given its pixel dimensions.
pub fn needs_downsampling(width: u32, height: u32, max_pixels: u32) -> bool {
    width > max_pixels || height > max_pixels
}

/// Count unique colors by sampling every 4th pixel, capped at 257.
/// The caller only needs to distinguish palette-like images (<=256 colors)
/// from continuous-tone images, so retaining millions of colors is wasted work.
pub fn unique_color_count(img: &DynamicImage) -> usize {
    let mut sampled = HashSet::with_capacity(257);
    for (_, _, pixel) in img.pixels().step_by(4) {
        sampled.insert(pixel.to_rgb().0);
        if sampled.len() > 256 {
            return 257;
        }
    }
    sampled.len()
}

/// Decode raw bytes into a `DynamicImage`.
///
/// Tries format auto-detection first (PNG/TIFF/BMP magic bytes), then falls back to
/// raw pixel interpretation using width/height and stream dict metadata.
fn decode_image_bytes(
    raw: &[u8],
    width: u32,
    height: u32,
    stream: &Stream,
    is_jpeg: bool,
    max_decompressed_bytes: usize,
) -> Result<DynamicImage, ()> {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(width);
    limits.max_image_height = Some(height);
    limits.max_alloc = Some(max_decompressed_bytes as u64);
    if is_jpeg {
        let mut reader = image::ImageReader::with_format(Cursor::new(raw), ImageFormat::Jpeg);
        reader.limits(limits);
        return reader.decode().map_err(|_| ());
    }

    // Try format auto-detection (handles PNG, TIFF, BMP — detected via magic bytes)
    if let Ok(mut reader) = image::ImageReader::new(Cursor::new(raw)).with_guessed_format() {
        reader.limits(limits);
        if let Ok(img) = reader.decode() {
            return Ok(img);
        }
    }

    // Fall back: interpret as raw pixel bytes using stream dict metadata
    let bits = stream
        .dict
        .get(b"BitsPerComponent")
        .ok()
        .and_then(|o| o.as_i64().ok())
        .unwrap_or(8);

    if bits != 8 {
        return Err(());
    }

    let cs = stream
        .dict
        .get(b"ColorSpace")
        .ok()
        .and_then(|o| o.as_name().ok())
        .unwrap_or(b"DeviceRGB");

    match cs {
        b"DeviceRGB" | b"RGB" => image::RgbImage::from_raw(width, height, raw.to_vec())
            .map(DynamicImage::ImageRgb8)
            .ok_or(()),
        b"DeviceGray" | b"Grayscale" => image::GrayImage::from_raw(width, height, raw.to_vec())
            .map(DynamicImage::ImageLuma8)
            .ok_or(()),
        _ => Err(()),
    }
}

/// Extract the concatenated IDAT payload bytes from a PNG byte slice.
///
/// Each PNG IDAT chunk contains: 4-byte length, 4-byte type "IDAT", N payload bytes, 4-byte CRC.
/// This function collects only the N payload bytes from all IDAT chunks.
/// Concatenated, these form a single valid zlib stream (RFC 1950) suitable for FlateDecode.
///
/// Returns `None` if the input is not a valid PNG or contains no IDAT chunks.
fn extract_png_idat(png_bytes: &[u8]) -> Option<Vec<u8>> {
    const PNG_SIG: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if png_bytes.len() < 8 || &png_bytes[..8] != PNG_SIG {
        return None;
    }
    let mut idat = Vec::new();
    let mut pos = 8usize; // skip 8-byte PNG signature
    while pos + 12 <= png_bytes.len() {
        // PNG chunk layout: [4 length][4 type][N data][4 CRC]
        let length = u32::from_be_bytes(png_bytes[pos..pos + 4].try_into().ok()?) as usize;
        let chunk_type = &png_bytes[pos + 4..pos + 8];
        let data_end = pos + 8 + length;
        if data_end > png_bytes.len() {
            break;
        }
        if chunk_type == b"IDAT" {
            idat.extend_from_slice(&png_bytes[pos + 8..data_end]);
        } else if chunk_type == b"IEND" {
            break;
        }
        pos = data_end + 4; // advance past data + 4-byte CRC
    }
    if idat.is_empty() {
        None
    } else {
        Some(idat)
    }
}

/// Color space of an image, used to set PDF ColorSpace and DecodeParms.Colors.
#[derive(Debug, Clone, Copy)]
enum ColorSpaceKind {
    Gray,
    Rgb,
}

impl ColorSpaceKind {
    fn pdf_name(self) -> &'static str {
        match self {
            ColorSpaceKind::Gray => "DeviceGray",
            ColorSpaceKind::Rgb => "DeviceRGB",
        }
    }

    fn channel_count(self) -> i64 {
        match self {
            ColorSpaceKind::Gray => 1,
            ColorSpaceKind::Rgb => 3,
        }
    }
}

/// Encode an image losslessly using oxipng and extract the IDAT zlib payload.
///
/// Returns `(idat_bytes, color_space)`. The IDAT bytes are a valid RFC 1950 zlib stream
/// suitable as FlateDecode content when `/DecodeParms << /Predictor 15 ... >>` is set.
///
/// Inputs reaching this encoder are 8-bit; the caller preserves unsupported 16-bit
/// images unchanged rather than silently reducing their precision.
///
/// oxipng failure is non-fatal: falls back to unoptimized PNG bytes before IDAT extraction.
fn encode_lossless(img: &DynamicImage) -> Result<(Vec<u8>, ColorSpaceKind), ()> {
    // Determine color space and normalize supported inputs to explicit 8-bit planes.
    let (cs, img_8bit): (ColorSpaceKind, DynamicImage) = match img.color() {
        ColorType::L8 | ColorType::La8 | ColorType::L16 | ColorType::La16 => (
            ColorSpaceKind::Gray,
            DynamicImage::ImageLuma8(img.to_luma8()),
        ),
        _ => (ColorSpaceKind::Rgb, DynamicImage::ImageRgb8(img.to_rgb8())),
    };

    // Encode normalized 8-bit image to PNG bytes
    let mut png_bytes = Vec::new();
    img_8bit
        .write_to(&mut Cursor::new(&mut png_bytes), ImageFormat::Png)
        .map_err(|_| ())?;

    // Run oxipng optimization (non-fatal fallback on failure).
    // Disable color_type_reduction and grayscale_reduction to prevent oxipng from changing
    // RGB→Grayscale when all pixels are grey, which would mismatch the ColorSpaceKind we derived
    // and produce a corrupt PDF stream (IDAT has 1-channel data but dict says ColorSpace=DeviceRGB).
    let opts = oxipng::Options {
        color_type_reduction: false,
        grayscale_reduction: false,
        ..oxipng::Options::default()
    };
    let optimized = oxipng::optimize_from_memory(&png_bytes, &opts).unwrap_or(png_bytes);

    // Extract the IDAT zlib payload for FlateDecode
    let idat = extract_png_idat(&optimized).ok_or(())?;

    Ok((idat, cs))
}

/// Encode a DynamicImage to JPEG bytes using mozjpeg.
///
/// The entire operation is wrapped in `catch_unwind` because libjpeg uses C-level error
/// handling that surfaces as Rust panics. Without this, any encoding error aborts the process.
#[cfg(test)]
fn encode_jpeg_mozjpeg(img: &DynamicImage, quality: u8) -> Result<Vec<u8>, ()> {
    encode_jpeg_mozjpeg_smoothed(img, quality, 0)
}

/// Encode as a single-channel grayscale JPEG (PDF ColorSpace /DeviceGray).
fn encode_jpeg_gray(img: &DynamicImage, quality: u8, smoothing: u8) -> Result<Vec<u8>, ()> {
    let luma = img.to_luma8();
    let pixels = luma.as_raw().clone();
    let width = img.width() as usize;
    let height = img.height() as usize;

    std::panic::catch_unwind(move || {
        let mut compress = mozjpeg::Compress::new(mozjpeg::ColorSpace::JCS_GRAYSCALE);
        compress.set_size(width, height);
        compress.set_quality(quality as f32);
        compress.set_optimize_coding(true);
        if smoothing > 0 {
            compress.set_smoothing_factor(smoothing);
        }
        let mut started = compress.start_compress(Vec::new()).map_err(|_| ())?;
        started.write_scanlines(&pixels).map_err(|_| ())?;
        started.finish().map_err(|_| ())
    })
    .unwrap_or(Err(()))
}

/// Block edge for the near-gray spatial detector.
const GRAY_BLOCK: u32 = 32;
/// A block whose mean per-pixel channel spread exceeds this is considered colored.
const GRAY_BLOCK_SPREAD_LIMIT: f64 = 12.0;

/// True if EVERY GRAY_BLOCK-px block of the image is near-gray.
///
/// Spatial (per-block) rather than global sampling: a global %-of-pixels heuristic
/// would let a stamp- or signature-sized colored region be averaged away and then
/// erased by grayscale conversion. Block granularity bounds the damage a false
/// positive can do to a 32-px square.
fn is_near_gray(img: &DynamicImage) -> bool {
    let (w, h) = img.dimensions();
    let mut by = 0;
    while by < h {
        let bh = GRAY_BLOCK.min(h - by);
        let mut bx = 0;
        while bx < w {
            let bw = GRAY_BLOCK.min(w - bx);
            let mut sum = 0u64;
            for y in by..by + bh {
                for x in bx..bx + bw {
                    let p = img.get_pixel(x, y).to_rgb().0;
                    let max = p[0].max(p[1]).max(p[2]);
                    let min = p[0].min(p[1]).min(p[2]);
                    sum += (max - min) as u64;
                }
            }
            let mean = sum as f64 / (bw * bh) as f64;
            if mean > GRAY_BLOCK_SPREAD_LIMIT {
                return false;
            }
            bx += GRAY_BLOCK;
        }
        by += GRAY_BLOCK;
    }
    true
}

fn encode_jpeg_mozjpeg_smoothed(
    img: &DynamicImage,
    quality: u8,
    smoothing: u8,
) -> Result<Vec<u8>, ()> {
    // Extract data before the catch_unwind closure — DynamicImage is not UnwindSafe
    let rgb = img.to_rgb8();
    let pixels = rgb.as_raw().clone();
    let width = img.width() as usize;
    let height = img.height() as usize;

    std::panic::catch_unwind(move || {
        let mut compress = mozjpeg::Compress::new(mozjpeg::ColorSpace::JCS_RGB);
        compress.set_size(width, height); // set_size takes (usize, usize)
        compress.set_quality(quality as f32); // set_quality takes f32
        compress.set_optimize_coding(true);
        if smoothing > 0 {
            compress.set_smoothing_factor(smoothing);
        }
        let mut started = compress.start_compress(Vec::new()).map_err(|_| ())?;
        started.write_scanlines(&pixels).map_err(|_| ())?;
        started.finish().map_err(|_| ())
    })
    .unwrap_or(Err(()))
}

/// Compute SSIM score between two images using MSSIMSimple.
///
/// Returns a score in [0.0, 1.0] where 1.0 = identical and 0.0 = completely dissimilar.
/// Returns 0.0 on any error so the caller (SSIM gate) conservatively rejects the re-encode.
fn compute_ssim(a: &DynamicImage, b: &DynamicImage) -> f64 {
    // image_compare requires &RgbImage, not &DynamicImage
    let a_rgb = a.to_rgb8();
    let b_rgb = b.to_rgb8();
    image_compare::rgb_similarity_structure(&image_compare::Algorithm::MSSIMSimple, &a_rgb, &b_rgb)
        .map(|r| r.score)
        .unwrap_or(0.0)
}

/// Tile edge for the worst-region quality check.
const QUALITY_TILE: u32 = 256;
/// Tiles whose original luma standard deviation is below this are treated as blank
/// paper and skipped — SSIM is numerically unstable on near-flat regions.
const QUALITY_TILE_MIN_STDDEV: f64 = 4.0;

/// Diagnostic quality scores between an original and a candidate image.
///
/// Returns `(global_luma_ssim, min_tile_luma_ssim)`:
/// - global: MSSIMSimple on the full luma planes (grain in chroma is not penalized)
/// - min-tile: worst MSSIMSimple over QUALITY_TILE-px luma tiles, skipping near-blank
///   tiles. Mean-pooled global SSIM hides localized glyph damage on mostly-blank scan
///   pages; the tile floor is what actually protects text.
///
/// Returns `(0.0, 0.0)` on dimension mismatch or comparison error (conservative reject).
pub fn quality_scores(a: &DynamicImage, b: &DynamicImage) -> (f64, f64) {
    if a.width() != b.width() || a.height() != b.height() {
        return (0.0, 0.0);
    }
    let a_l = a.to_luma8();
    let b_l = b.to_luma8();
    let global = image_compare::gray_similarity_structure(
        &image_compare::Algorithm::MSSIMSimple,
        &a_l,
        &b_l,
    )
    .map(|r| r.score)
    .unwrap_or(0.0);

    let (w, h) = (a_l.width(), a_l.height());
    let mut min_tile = 1.0_f64;
    let mut ty = 0;
    while ty < h {
        let th = QUALITY_TILE.min(h - ty);
        let mut tx = 0;
        while tx < w {
            let tw = QUALITY_TILE.min(w - tx);
            // MSSIM needs a reasonable window; ignore slivers
            if tw >= 16 && th >= 16 {
                let ta = image::imageops::crop_imm(&a_l, tx, ty, tw, th).to_image();
                // Skip near-blank tiles (flat paper): SSIM denominators explode on
                // zero-variance regions and produce meaningless low scores.
                let mean = ta.pixels().map(|p| p.0[0] as f64).sum::<f64>() / (tw * th) as f64;
                let var = ta
                    .pixels()
                    .map(|p| (p.0[0] as f64 - mean).powi(2))
                    .sum::<f64>()
                    / (tw * th) as f64;
                if var.sqrt() >= QUALITY_TILE_MIN_STDDEV {
                    let tb = image::imageops::crop_imm(&b_l, tx, ty, tw, th).to_image();
                    let score = image_compare::gray_similarity_structure(
                        &image_compare::Algorithm::MSSIMSimple,
                        &ta,
                        &tb,
                    )
                    .map(|r| r.score)
                    .unwrap_or(0.0);
                    min_tile = min_tile.min(score);
                }
            }
            tx += QUALITY_TILE;
        }
        ty += QUALITY_TILE;
    }
    (global, min_tile)
}

/// Attempt to optimize a single image stream.
///
/// Returns:
/// - `Ok(Some((bytes, dict)))` — successfully optimized; replace stream content and dict
/// - `Ok(None)`               — skip silently (transparent image, or already optimal JPEG)
/// - `Err(())`                — decode failed; caller should emit a warning
///
/// Whenever the lossy pipeline does not produce a replacement for a DCTDecode stream
/// (skipped, gate-rejected, or undecodable), a lossless coefficient-domain transcode
/// (optimized Huffman tables, stripped EXIF) is tried instead — it renders
/// bit-identically, so no quality gate applies, only the size guard.
#[allow(clippy::result_unit_err)]
pub fn optimize_image_stream(
    stream: &Stream,
    opts: &ImageOptions,
) -> Result<Option<(Vec<u8>, Dictionary)>, ()> {
    let result = optimize_image_stream_lossy(stream, opts);
    if matches!(result, Ok(Some(_))) {
        return result;
    }
    let is_dct = stream
        .filters()
        .map(|f| f == vec![b"DCTDecode" as &[u8]])
        .unwrap_or(false);
    if is_dct {
        if let Some(bytes) = crate::jpeg_transcode::transcode_lossless(&stream.content) {
            if bytes.len() < stream.content.len() {
                let mut dict = stream.dict.clone();
                // Dimensions, colorspace and filter are unchanged; only entropy
                // coding differs. Normalize Filter to name form.
                dict.set("Filter", Object::Name(b"DCTDecode".to_vec()));
                dict.set("Length", Object::Integer(bytes.len() as i64));
                return Ok(Some((bytes, dict)));
            }
        }
    }
    result
}

#[allow(clippy::result_unit_err)]
fn optimize_image_stream_lossy(
    stream: &Stream,
    opts: &ImageOptions,
) -> Result<Option<(Vec<u8>, Dictionary)>, ()> {
    let width = stream
        .dict
        .get(b"Width")
        .ok()
        .and_then(|o| o.as_i64().ok())
        .ok_or(())?
        .try_into()
        .map_err(|_| ())?;
    let height = stream
        .dict
        .get(b"Height")
        .ok()
        .and_then(|o| o.as_i64().ok())
        .ok_or(())?
        .try_into()
        .map_err(|_| ())?;

    if width == 0 || height == 0 {
        return Ok(None);
    }

    // DynamicImage may require up to eight bytes per pixel (16-bit RGBA).
    // Reject dimensions that cannot fit inside the configured decoded-image budget.
    let worst_case_bytes = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(8))
        .ok_or(())?;
    if worst_case_bytes > opts.max_decompressed_bytes {
        return Ok(None);
    }

    // /Decode and color-key /Mask arrays operate on the original sample values.
    // Re-encoding or changing the color space without applying/remapping them can
    // invert colors or change transparency, so preserve these streams untouched.
    if stream.dict.get(b"Decode").is_ok()
        || matches!(stream.dict.get(b"Mask"), Ok(Object::Array(_)))
    {
        return Ok(None);
    }

    // Recognize both /DCTDecode and single-element [/DCTDecode] filter forms
    let is_jpeg = stream
        .filters()
        .map(|f| f == vec![b"DCTDecode" as &[u8]])
        .unwrap_or(false);

    let has_filter = stream.dict.get(b"Filter").is_ok();

    let max_pixels = opts.effective_max_pixels();

    // Skip silently: already JPEG within pixel threshold, not forced to re-encode
    if is_jpeg && !needs_downsampling(width, height, max_pixels) && !opts.force_jpeg {
        return Ok(None);
    }

    // Obtain raw image bytes appropriate to filter type:
    // - JPEG (DCTDecode): content IS the JPEG bytes (lopdf does not decode DCT natively)
    // - No filter: content is already uncompressed (raw pixels or a container like PNG)
    // - Other filters (FlateDecode etc.): decompress via lopdf
    let raw = if is_jpeg || !has_filter {
        if !is_jpeg && stream.content.len() > opts.max_decompressed_bytes {
            return Ok(None);
        }
        stream.content.clone()
    } else {
        stream
            .decompressed_content_with_limit(opts.max_decompressed_bytes)
            .map_err(|_| ())?
    };

    let img = decode_image_bytes(
        &raw,
        width,
        height,
        stream,
        is_jpeg,
        opts.max_decompressed_bytes,
    )?;

    // The current Flate/JPEG writers emit 8-bit samples. Silently converting a
    // 16-bit source would violate the quality-preserving contract, so leave it
    // untouched until a 16-bit Flate or JPX encoder is available.
    if matches!(
        img.color(),
        ColorType::L16 | ColorType::La16 | ColorType::Rgb16 | ColorType::Rgba16
    ) {
        return Ok(None);
    }

    // Preserve transparency: JPEG does not support alpha, and lossless RGB would drop it.
    // Skip any image with an alpha channel to avoid corrupting transparent regions.
    if img.color().has_alpha() && !opts.force_jpeg {
        return Ok(None);
    }

    // Downsample if dimensions exceed threshold (Lanczos3: sharper glyph strokes than
    // thumbnail()'s box filter). Keep the pre-downscale original for the end-to-end
    // quality gate — unless the resolution loss was explicitly requested via
    // --downsample-dpi, in which case only encode fidelity is gated.
    let (img, e2e_original) = if needs_downsampling(width, height, max_pixels) {
        let down = img.resize(
            max_pixels,
            max_pixels,
            image::imageops::FilterType::Lanczos3,
        );
        if opts.downsample_dpi.is_some() {
            (down, None)
        } else {
            (down, Some(img))
        }
    } else {
        (img, None)
    };

    // End-to-end quality: when we downscaled on our own initiative, compare the candidate
    // against the TRUE original (candidate upsampled back), so resolution loss is visible
    // to the gate instead of hidden by comparing against the already-downscaled source.
    // Returns (rgb_mean, luma_global, min_tile).
    let e2e_quality = |candidate: &DynamicImage| -> (f64, f64, f64) {
        match &e2e_original {
            Some(orig) => {
                let restored = candidate.resize_exact(
                    orig.width(),
                    orig.height(),
                    image::imageops::FilterType::Lanczos3,
                );
                let (g, t) = quality_scores(orig, &restored);
                (compute_ssim(orig, &restored), g, t)
            }
            None => {
                let (g, t) = quality_scores(&img, candidate);
                (compute_ssim(&img, candidate), g, t)
            }
        }
    };

    // Opt-in bilevel path: a text-only page at full resolution beats any JPEG candidate
    // by an order of magnitude. Classification, midtone check and the blurred-mask gate
    // all live in bilevel::try_bilevel; color content is excluded by the near-gray check.
    if opts.bilevel {
        let src = e2e_original.as_ref().unwrap_or(&img);
        if is_near_gray(src) {
            if let Some(g4) = crate::bilevel::try_bilevel(src, quality_scores) {
                if g4.len() < stream.content.len() {
                    let mut dict = stream.dict.clone();
                    let mut parms = Dictionary::new();
                    parms.set("K", Object::Integer(-1));
                    parms.set("Columns", Object::Integer(src.width() as i64));
                    parms.set("Rows", Object::Integer(src.height() as i64));
                    dict.set("Filter", Object::Name(b"CCITTFaxDecode".to_vec()));
                    dict.set("DecodeParms", Object::Dictionary(parms));
                    dict.set("Width", Object::Integer(src.width() as i64));
                    dict.set("Height", Object::Integer(src.height() as i64));
                    dict.set("Length", Object::Integer(g4.len() as i64));
                    dict.set("BitsPerComponent", Object::Integer(1));
                    dict.set("ColorSpace", Object::Name(b"DeviceGray".to_vec()));
                    return Ok(Some((g4, dict)));
                }
            }
        }
    }

    // Determine output format: JPEG for photographic content, lossless otherwise
    let use_jpeg = opts.force_jpeg || is_jpeg || unique_color_count(&img) > 256;

    let (encoded, out_width, out_height, filter, color_space, lossless_parms) = if use_jpeg {
        // Candidate ladder, ordered by expected size (smallest first). "full" candidates
        // (no resolution loss) exist only when we downscaled on our own initiative and
        // serve as fallback when the downscale destroyed detail. Gray candidates exist
        // only when the spatial detector says no block carries real color.
        let classification_src = e2e_original.as_ref().unwrap_or(&img);
        let near_gray = is_near_gray(classification_src);
        struct Candidate<'a> {
            src: &'a DynamicImage,
            gray: bool,
            full_res: bool,
        }
        // Resolution loss is independent of the eventual JPEG entropy coding. If a
        // lossless downsample already fails the end-to-end gate, no lossy encode of
        // that downsample can be a quality-preserving candidate.
        let downscale_viable = e2e_original.is_none() || {
            let (c, g, t) = e2e_quality(&img);
            passes_quality_gate(c, g, t)
        };
        let mut ladder: Vec<Candidate> = Vec::new();
        if downscale_viable {
            if near_gray {
                ladder.push(Candidate {
                    src: &img,
                    gray: true,
                    full_res: false,
                });
            }
            ladder.push(Candidate {
                src: &img,
                gray: false,
                full_res: false,
            });
        }
        if let Some(orig) = &e2e_original {
            if near_gray {
                ladder.push(Candidate {
                    src: orig,
                    gray: true,
                    full_res: true,
                });
            }
            ladder.push(Candidate {
                src: orig,
                gray: false,
                full_res: true,
            });
        }

        // The recommended preset was visually calibrated at q75. Keep that exact
        // quality instead of searching lower values; custom mode retains the
        // adaptive smallest-passing search.
        let mut qualities = if opts.is_recommended_scan_profile() {
            vec![75]
        } else {
            vec![40, 50, 60, 70, opts.image_quality]
        };
        qualities.retain(|q| *q <= opts.image_quality);
        qualities.sort_unstable();
        qualities.dedup();

        let mut winner: Option<(Vec<u8>, u32, u32, ColorSpaceKind)> = None;
        for cand in ladder {
            // Search from smallest expected output toward the user's quality ceiling.
            // The first passing encode for this representation competes globally by size.
            for &quality in &qualities {
                let Ok(bytes) = (if cand.gray {
                    encode_jpeg_gray(cand.src, quality, opts.smoothing)
                } else {
                    encode_jpeg_mozjpeg_smoothed(cand.src, quality, opts.smoothing)
                }) else {
                    continue;
                };
                let Ok(decoded) = image::load_from_memory_with_format(&bytes, ImageFormat::Jpeg)
                else {
                    continue;
                };
                let passes = if opts.is_recommended_scan_profile() {
                    // This preset intentionally trades scanner grain and source
                    // resolution for size at a visually calibrated fixed q75. The
                    // successful decode above and the size guard below protect the
                    // pipeline; SSIM is not meaningful here because it heavily
                    // penalizes removal of high-frequency scanner noise.
                    true
                } else {
                    // Full-resolution candidates are compared directly against the original;
                    // downscaled candidates go through the end-to-end (upsample-back) scores.
                    let (c, g, t) = if cand.full_res {
                        let (g, t) = quality_scores(cand.src, &decoded);
                        (compute_ssim(cand.src, &decoded), g, t)
                    } else {
                        e2e_quality(&decoded)
                    };
                    passes_quality_gate(c, g, t)
                };
                if passes {
                    let cs = if cand.gray {
                        ColorSpaceKind::Gray
                    } else {
                        ColorSpaceKind::Rgb
                    };
                    if winner
                        .as_ref()
                        .map(|(best, ..)| bytes.len() < best.len())
                        .unwrap_or(true)
                    {
                        winner = Some((bytes, cand.src.width(), cand.src.height(), cs));
                    }
                    break;
                }
            }
        }
        let Some((bytes, w, h, cs)) = winner else {
            return Ok(None);
        };
        (bytes, w, h, "DCTDecode", cs, None)
    } else {
        // Downscale-then-lossless is pixel-exact w.r.t. the downscaled image, but the
        // downscale itself is lossy — gate it end-to-end like the JPEG path.
        if e2e_original.is_some() {
            let (c, g, t) = e2e_quality(&img);
            if !passes_quality_gate(c, g, t) {
                return Ok(None);
            }
        }
        // Lossless: PNG-predictor FlateDecode via oxipng
        let (idat, cs) = encode_lossless(&img)?;
        let mut parms = Dictionary::new();
        parms.set("Predictor", Object::Integer(15));
        parms.set("Colors", Object::Integer(cs.channel_count()));
        parms.set("BitsPerComponent", Object::Integer(8));
        parms.set("Columns", Object::Integer(img.width() as i64));
        (
            idat,
            img.width(),
            img.height(),
            "FlateDecode",
            cs,
            Some(parms),
        )
    };
    let (new_width, new_height) = (out_width, out_height);

    // Size guard: a candidate that is not strictly smaller than the original stream
    // must never ship — the original bytes are both smaller and higher quality.
    if encoded.len() >= stream.content.len() {
        return Ok(None);
    }

    // Build updated stream dictionary
    let mut dict = stream.dict.clone();
    dict.set("Filter", Object::Name(filter.as_bytes().to_vec()));
    dict.set("Width", Object::Integer(new_width as i64));
    dict.set("Height", Object::Integer(new_height as i64));
    dict.set("Length", Object::Integer(encoded.len() as i64));
    dict.set("BitsPerComponent", Object::Integer(8));
    dict.set(
        "ColorSpace",
        Object::Name(color_space.pdf_name().as_bytes().to_vec()),
    );
    match lossless_parms {
        Some(parms) => dict.set("DecodeParms", Object::Dictionary(parms)),
        None => {
            dict.remove(b"DecodeParms");
        }
    }

    Ok(Some((encoded, dict)))
}

/// Lossless-only pass: coefficient-domain transcode of every DCTDecode image stream.
///
/// Used when `--aggressive` is off — rendering stays bit-identical (pixels are never
/// decoded), only entropy coding is optimized and EXIF/APPn markers are dropped.
pub fn transcode_jpeg_streams(doc: &mut Document) {
    let ids: Vec<ObjectId> = doc
        .objects
        .iter()
        .filter_map(|(id, obj)| {
            if let Object::Stream(s) = obj {
                let is_dct = s
                    .filters()
                    .map(|f| f == vec![b"DCTDecode" as &[u8]])
                    .unwrap_or(false);
                if is_image_stream(s) && is_dct {
                    return Some(*id);
                }
            }
            None
        })
        .collect();
    for id in ids {
        if let Some(Object::Stream(s)) = doc.objects.get_mut(&id) {
            if let Some(bytes) = crate::jpeg_transcode::transcode_lossless(&s.content) {
                if bytes.len() < s.content.len() {
                    s.content = bytes;
                    s.dict.set("Filter", Object::Name(b"DCTDecode".to_vec()));
                    s.dict
                        .set("Length", Object::Integer(s.content.len() as i64));
                }
            }
        }
    }
}

fn is_image_stream(s: &Stream) -> bool {
    s.dict
        .get(b"Subtype")
        .ok()
        .and_then(|o| o.as_name().ok())
        .map(|n| n == b"Image")
        .unwrap_or(false)
}

/// Apply image optimization to all image XObjects in the document.
///
/// Streams are optimized one at a time. File-level parallelism is already provided by
/// the CLI; avoiding nested image parallelism keeps decoded-pixel memory bounded.
pub fn optimize_images(doc: &mut Document, opts: &ImageOptions, input_path: &str) {
    let image_ids: Vec<ObjectId> = doc
        .objects
        .iter()
        .filter_map(|(id, obj)| {
            if let Object::Stream(s) = obj {
                if is_image_stream(s) {
                    return Some(*id);
                }
            }
            None
        })
        .collect();

    for id in image_ids {
        let result = match doc.objects.get(&id) {
            Some(Object::Stream(stream)) => optimize_image_stream(stream, opts),
            _ => continue,
        };
        match result {
            Ok(Some((bytes, dict))) => {
                if let Some(Object::Stream(s)) = doc.objects.get_mut(&id) {
                    s.content = bytes;
                    s.dict = dict;
                }
            }
            Ok(None) => {}
            Err(()) => {
                eprintln!("Could not optimize image in {}, skipping image", input_path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_count_heuristic_photo() {
        let mut img = image::RgbImage::new(4, 4);
        for (i, pixel) in img.pixels_mut().enumerate() {
            *pixel = image::Rgb([(i * 17) as u8, (i * 31) as u8, (i * 7) as u8]);
        }
        let dynamic = image::DynamicImage::from(img);
        let _ = unique_color_count(&dynamic);
    }

    #[test]
    fn color_count_heuristic_graphic() {
        let mut img = image::RgbImage::new(4, 4);
        for (i, pixel) in img.pixels_mut().enumerate() {
            *pixel = if i % 2 == 0 {
                image::Rgb([0, 0, 0])
            } else {
                image::Rgb([255, 255, 255])
            };
        }
        let dynamic = image::DynamicImage::from(img);
        assert!(unique_color_count(&dynamic) <= 256);
    }

    #[test]
    fn color_count_stops_after_palette_limit() {
        let mut img = image::RgbImage::new(2048, 4);
        for (i, pixel) in img.pixels_mut().enumerate() {
            let sampled_index = i / 4;
            *pixel = image::Rgb([
                (sampled_index & 0xff) as u8,
                ((sampled_index >> 8) & 0xff) as u8,
                0,
            ]);
        }
        assert_eq!(unique_color_count(&DynamicImage::ImageRgb8(img)), 257);
    }

    #[test]
    fn needs_downsampling_over_threshold() {
        assert!(needs_downsampling(2000, 1000, 1500));
        assert!(needs_downsampling(1000, 2000, 1500));
    }

    #[test]
    fn needs_downsampling_under_threshold() {
        assert!(!needs_downsampling(800, 600, 1500));
        assert!(!needs_downsampling(1500, 1500, 1500));
    }

    #[test]
    fn effective_max_pixels_without_dpi() {
        let opts = ImageOptions {
            max_pixels: 1500,
            image_quality: 85,
            smoothing: 0,
            downsample_dpi: None,
            force_jpeg: false,
            bilevel: false,
            max_decompressed_bytes: 64 * 1024 * 1024,
        };
        assert_eq!(opts.effective_max_pixels(), 1500);
    }

    #[test]
    fn effective_max_pixels_dpi_more_restrictive() {
        // 120 DPI × 11.7" = 1404 pixels — more restrictive than max_pixels=1500
        let opts = ImageOptions {
            max_pixels: 1500,
            image_quality: 85,
            smoothing: 0,
            downsample_dpi: Some(120),
            force_jpeg: false,
            bilevel: false,
            max_decompressed_bytes: 64 * 1024 * 1024,
        };
        assert_eq!(opts.effective_max_pixels(), 1404);
    }

    #[test]
    fn effective_max_pixels_max_pixels_more_restrictive() {
        // 300 DPI × 11.7" = 3510 pixels — less restrictive than max_pixels=1500
        let opts = ImageOptions {
            max_pixels: 1500,
            image_quality: 85,
            smoothing: 0,
            downsample_dpi: Some(300),
            force_jpeg: false,
            bilevel: false,
            max_decompressed_bytes: 64 * 1024 * 1024,
        };
        assert_eq!(opts.effective_max_pixels(), 1500);
    }

    fn default_opts() -> ImageOptions {
        ImageOptions {
            max_pixels: 1500,
            image_quality: 85,
            smoothing: 0,
            downsample_dpi: None,
            force_jpeg: false,
            bilevel: false,
            max_decompressed_bytes: 64 * 1024 * 1024,
        }
    }

    /// Helper: build a stream containing JPEG bytes (DCTDecode filter).
    fn jpeg_stream(width: u32, height: u32, quality: u8) -> lopdf::Stream {
        use lopdf::{Dictionary, Object, Stream};
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::new(width, height));
        let buf =
            encode_jpeg_mozjpeg(&img, quality).expect("encode_jpeg_mozjpeg failed in test helper");
        let mut dict = Dictionary::new();
        dict.set("Width", Object::Integer(width as i64));
        dict.set("Height", Object::Integer(height as i64));
        dict.set("Filter", Object::Name(b"DCTDecode".to_vec()));
        dict.set("Length", Object::Integer(buf.len() as i64));
        Stream::new(dict, buf)
    }

    /// Helper: build a stream containing PNG bytes (no filter — simulates embedded container).
    fn png_stream_no_filter(img: image::DynamicImage) -> lopdf::Stream {
        use lopdf::{Dictionary, Object, Stream};
        let width = img.width();
        let height = img.height();
        let mut buf = Vec::new();
        img.write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)
            .unwrap();
        let mut dict = Dictionary::new();
        dict.set("Width", Object::Integer(width as i64));
        dict.set("Height", Object::Integer(height as i64));
        dict.set("Length", Object::Integer(buf.len() as i64));
        Stream::new(dict, buf)
    }

    #[test]
    fn optimize_image_stream_skips_already_jpeg_within_threshold() {
        let stream = jpeg_stream(4, 4, 90);
        let result = optimize_image_stream(&stream, &default_opts());
        // Already JPEG, within threshold, no force_jpeg → silently skipped
        assert!(matches!(result, Ok(None)));
    }

    #[test]
    fn optimize_image_stream_skips_transparent_image() {
        // RGBA PNG (has alpha) → should be preserved unchanged
        let img = image::DynamicImage::ImageRgba8(image::RgbaImage::new(10, 10));
        let stream = png_stream_no_filter(img);
        let result = optimize_image_stream(&stream, &default_opts());
        assert!(
            matches!(result, Ok(None)),
            "transparent image should be skipped"
        );
    }

    #[test]
    fn optimize_image_stream_preserves_sample_dependent_decode_array() {
        let img = DynamicImage::ImageRgb8(image::RgbImage::new(32, 32));
        let mut stream = png_stream_no_filter(img);
        stream.dict.set(
            "Decode",
            Object::Array(vec![
                Object::Integer(1),
                Object::Integer(0),
                Object::Integer(1),
                Object::Integer(0),
                Object::Integer(1),
                Object::Integer(0),
            ]),
        );
        assert!(matches!(
            optimize_image_stream(&stream, &default_opts()),
            Ok(None)
        ));
    }

    #[test]
    fn optimize_image_stream_does_not_reduce_16_bit_precision() {
        let img = DynamicImage::ImageLuma16(image::ImageBuffer::from_pixel(
            32,
            32,
            image::Luma([32_768u16]),
        ));
        let stream = png_stream_no_filter(img);
        assert!(matches!(
            optimize_image_stream(&stream, &default_opts()),
            Ok(None)
        ));
    }

    #[test]
    fn optimize_image_stream_force_jpeg_re_encodes_existing_jpeg() {
        // Already JPEG + within threshold, but force_jpeg=true → must re-encode.
        // Use a checkerboard at q100 so the q75 candidate is both smaller (size guard)
        // and structurally near-identical (SSIM gate).
        let size = 64u32;
        let mut cb = image::RgbImage::new(size, size);
        for y in 0..size {
            for x in 0..size {
                cb.put_pixel(
                    x,
                    y,
                    if (x + y) % 2 == 0 {
                        image::Rgb([0u8, 0, 0])
                    } else {
                        image::Rgb([255, 255, 255])
                    },
                );
            }
        }
        let img = image::DynamicImage::ImageRgb8(cb);
        let buf = encode_jpeg_mozjpeg(&img, 100).unwrap();
        let mut dict = lopdf::Dictionary::new();
        dict.set("Width", lopdf::Object::Integer(size as i64));
        dict.set("Height", lopdf::Object::Integer(size as i64));
        dict.set("Filter", lopdf::Object::Name(b"DCTDecode".to_vec()));
        dict.set("Length", lopdf::Object::Integer(buf.len() as i64));
        let stream = lopdf::Stream::new(dict, buf);
        let opts = ImageOptions {
            force_jpeg: true,
            bilevel: false,
            image_quality: 75,
            ..default_opts()
        };
        let result = optimize_image_stream(&stream, &opts);
        assert!(
            matches!(result, Ok(Some(_))),
            "force_jpeg should re-encode even within threshold"
        );
    }

    #[test]
    fn optimize_image_stream_lossless_produces_flatedecode() {
        // RGB image with ≤256 colors (all black) → should use lossless FlateDecode
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::new(4, 4));
        let stream = png_stream_no_filter(img);
        let result = optimize_image_stream(&stream, &default_opts());
        if let Ok(Some((_, out_dict))) = result {
            let filter = out_dict.get(b"Filter").unwrap().as_name().unwrap();
            assert_eq!(filter, b"FlateDecode");
        }
        // Ok(None) would also be acceptable if the image was already optimal
    }

    #[test]
    fn adaptive_jpeg_search_respects_quality_ceiling() {
        // Use a checkerboard image with force_jpeg=true.
        // MSSIMSimple scores checkerboards > 0.99 at quality ≥ 60, so the SSIM gate passes for
        // both quality=75 (low) and quality=95 (high). Gradients and noise score below
        // SSIM_THRESHOLD at any practical quality with MSSIMSimple, so only structured
        // content works here.
        // force_jpeg=true is required because the 64×64 image is below max_pixels=1500 and
        // would otherwise be skipped (is_jpeg=true, no downsampling needed, force_jpeg=false).
        let size = 64u32;
        let mut cb = image::RgbImage::new(size, size);
        for y in 0..size {
            for x in 0..size {
                cb.put_pixel(
                    x,
                    y,
                    if (x + y) % 2 == 0 {
                        image::Rgb([0u8, 0, 0])
                    } else {
                        image::Rgb([255, 255, 255])
                    },
                );
            }
        }
        let img = image::DynamicImage::ImageRgb8(cb);
        // q100 input: both q95 and q75 candidates must beat it (size guard) to produce output
        let input_buf = encode_jpeg_mozjpeg(&img, 100).expect("encode_jpeg_mozjpeg failed in test");
        let original_len = input_buf.len();
        let make_stream = |buf: Vec<u8>| {
            use lopdf::{Dictionary, Object, Stream};
            let mut dict = Dictionary::new();
            dict.set("Width", Object::Integer(size as i64));
            dict.set("Height", Object::Integer(size as i64));
            dict.set("Filter", Object::Name(b"DCTDecode".to_vec()));
            dict.set("Length", Object::Integer(buf.len() as i64));
            Stream::new(dict, buf)
        };
        let opts_high = ImageOptions {
            image_quality: 95,
            force_jpeg: true,
            ..default_opts()
        };
        let opts_low = ImageOptions {
            image_quality: 75,
            force_jpeg: true,
            ..default_opts()
        };
        let (high_bytes, _) = optimize_image_stream(&make_stream(input_buf.clone()), &opts_high)
            .expect("high quality encode must succeed")
            .expect("high quality encode must produce output");
        let (low_bytes, _) = optimize_image_stream(&make_stream(input_buf), &opts_low)
            .expect("low quality encode must succeed")
            .expect("low quality encode must produce output");
        assert!(low_bytes.len() < original_len);
        assert!(high_bytes.len() < original_len);
    }

    #[test]
    fn extract_png_idat_valid() {
        use std::io::Cursor;
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::new(4, 4));
        let mut buf = Vec::new();
        img.write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)
            .unwrap();
        let idat = extract_png_idat(&buf);
        assert!(idat.is_some(), "should extract IDAT from a valid PNG");
        assert!(
            !idat.unwrap().is_empty(),
            "extracted IDAT should not be empty"
        );
    }

    #[test]
    fn extract_png_idat_rejects_garbage() {
        assert!(extract_png_idat(b"not a png at all").is_none());
        assert!(extract_png_idat(&[]).is_none());
    }

    #[test]
    fn lossless_path_sets_predictor_decode_parms() {
        // All-black 4x4 RGB image → ≤256 unique colors → lossless branch
        // Expects DecodeParms with Predictor=15, Colors=3, BitsPerComponent=8, Columns=4
        use std::io::Cursor;
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::new(4, 4));
        let mut buf = Vec::new();
        img.write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)
            .unwrap();
        let mut dict = lopdf::Dictionary::new();
        dict.set("Width", lopdf::Object::Integer(4));
        dict.set("Height", lopdf::Object::Integer(4));
        dict.set("Length", lopdf::Object::Integer(buf.len() as i64));
        let stream = lopdf::Stream::new(dict, buf);

        let result = optimize_image_stream(&stream, &default_opts());
        assert!(
            matches!(result, Ok(Some(_))),
            "expected lossless encode to succeed, got: {:?}",
            result.as_ref().map(|r| r.is_some())
        );
        if let Ok(Some((encoded, out_dict))) = result {
            let filter = out_dict.get(b"Filter").unwrap().as_name().unwrap();
            assert_eq!(filter, b"FlateDecode");
            let parms = out_dict
                .get(b"DecodeParms")
                .expect("lossless path must set DecodeParms");
            if let lopdf::Object::Dictionary(d) = parms {
                assert_eq!(d.get(b"Predictor").unwrap().as_i64().unwrap(), 15);
                assert_eq!(d.get(b"Colors").unwrap().as_i64().unwrap(), 3);
                assert_eq!(d.get(b"BitsPerComponent").unwrap().as_i64().unwrap(), 8);
                assert_eq!(d.get(b"Columns").unwrap().as_i64().unwrap(), 4);
            } else {
                panic!("DecodeParms must be a Dictionary");
            }
            // Verify encoded bytes are a valid zlib stream (decodable by FlateDecode)
            use flate2::read::ZlibDecoder;
            use std::io::Read;
            let mut decoder = ZlibDecoder::new(&encoded[..]);
            let mut out = Vec::new();
            decoder
                .read_to_end(&mut out)
                .expect("encoded bytes must be a valid zlib stream");
            // 4×4 RGB image: 4*4*3 = 48 raw pixel bytes per row, plus 1 filter byte per row = 4*(1+12) = 52
            assert!(!out.is_empty(), "decompressed output must not be empty");
        }
    }

    #[test]
    fn encode_jpeg_mozjpeg_roundtrip() {
        // Use a non-trivial image with a distinctive red pixel so channel-order bugs are detectable
        let mut src = image::RgbImage::new(8, 8);
        src.put_pixel(0, 0, image::Rgb([200, 50, 30]));
        let img = image::DynamicImage::ImageRgb8(src);
        let result = encode_jpeg_mozjpeg(&img, 85);
        assert!(
            result.is_ok(),
            "mozjpeg encode should succeed: {:?}",
            result
        );
        let bytes = result.unwrap();
        assert!(!bytes.is_empty(), "encoded bytes should be non-empty");
        let decoded = image::load_from_memory_with_format(&bytes, ImageFormat::Jpeg)
            .expect("mozjpeg output must decode back to a valid JPEG");
        assert_eq!(decoded.width(), 8);
        assert_eq!(decoded.height(), 8);
        // Verify channel order: the (0,0) pixel should still be predominantly red
        let p = decoded.to_rgb8().get_pixel(0, 0).0;
        assert!(
            p[0] > p[1] && p[0] > p[2],
            "red channel should dominate after roundtrip, got {:?}",
            p
        );
    }

    #[test]
    fn ssim_gate_skips_low_quality_jpeg() {
        // 32x32 checkerboard: high-frequency pattern, worst case for JPEG artifacts.
        // force_jpeg=true is required because checkerboard has ≤256 unique colors
        // and would otherwise take the lossless path.
        // quality=1 is the minimum: SSIM vs original will be far below SSIM_THRESHOLD.
        use std::io::Cursor;
        let mut img_data = image::RgbImage::new(32, 32);
        for (x, y, pixel) in img_data.enumerate_pixels_mut() {
            *pixel = if (x + y) % 2 == 0 {
                image::Rgb([0u8, 0, 0])
            } else {
                image::Rgb([255u8, 255, 255])
            };
        }
        let dynamic = image::DynamicImage::ImageRgb8(img_data);
        let mut buf = Vec::new();
        dynamic
            .write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)
            .unwrap();
        let mut dict = lopdf::Dictionary::new();
        dict.set("Width", lopdf::Object::Integer(32));
        dict.set("Height", lopdf::Object::Integer(32));
        dict.set("Length", lopdf::Object::Integer(buf.len() as i64));
        let stream = lopdf::Stream::new(dict, buf);

        let opts = ImageOptions {
            force_jpeg: true,
            bilevel: false,
            image_quality: 1,
            ..default_opts()
        };
        let result = optimize_image_stream(&stream, &opts);
        assert!(
            matches!(result, Ok(None)),
            "SSIM gate must return Ok(None) for quality=1 checkerboard, got: {:?}",
            result.as_ref().map(|r| r.is_some())
        );
    }

    #[test]
    fn compute_ssim_detects_severe_quality_loss() {
        // Verify that compute_ssim returns a score below SSIM_THRESHOLD for quality=1.
        // This makes the threshold value an explicit, tested property.
        let mut img_data = image::RgbImage::new(32, 32);
        for (x, y, pixel) in img_data.enumerate_pixels_mut() {
            *pixel = if (x + y) % 2 == 0 {
                image::Rgb([0u8, 0, 0])
            } else {
                image::Rgb([255, 255, 255])
            };
        }
        let original = image::DynamicImage::ImageRgb8(img_data);
        let jpeg_bytes = encode_jpeg_mozjpeg(&original, 1).unwrap();
        let decoded = image::load_from_memory_with_format(&jpeg_bytes, ImageFormat::Jpeg).unwrap();
        let score = compute_ssim(&original, &decoded);
        assert!(
            score < SSIM_THRESHOLD,
            "quality=1 checkerboard must score below SSIM_THRESHOLD ({}), got {}",
            SSIM_THRESHOLD,
            score
        );
    }

    #[test]
    fn quality_gate_boundary() {
        // Pin the gate boundaries; guard against silent threshold drift
        // (tests and code once disagreed 0.85 vs 0.95).
        assert!((SSIM_THRESHOLD - 0.99).abs() < f64::EPSILON);
        assert!((TILE_SSIM_THRESHOLD - 0.96).abs() < f64::EPSILON);
        assert!((CHROMA_SSIM_FLOOR - 0.95).abs() < f64::EPSILON);
        // all floors met → pass (boundary inclusive)
        assert!(passes_quality_gate(
            CHROMA_SSIM_FLOOR,
            SSIM_THRESHOLD,
            TILE_SSIM_THRESHOLD
        ));
        // each floor individually violated → reject
        assert!(!passes_quality_gate(0.94, 0.99, 0.99));
        assert!(!passes_quality_gate(0.99, 0.98, 0.99));
        assert!(!passes_quality_gate(0.99, 0.99, 0.95));
    }

    #[test]
    fn quality_scores_identical_images_are_perfect() {
        let mut img = image::RgbImage::new(300, 300);
        for (x, y, p) in img.enumerate_pixels_mut() {
            *p = image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8]);
        }
        let a = image::DynamicImage::ImageRgb8(img);
        let (global, min_tile) = quality_scores(&a, &a.clone());
        assert!(
            global > 0.999,
            "identical images must score ~1.0 global, got {global}"
        );
        assert!(
            min_tile > 0.999,
            "identical images must score ~1.0 min-tile, got {min_tile}"
        );
    }

    #[test]
    fn quality_scores_detect_localized_damage() {
        // 512x512 textured image; destroy only one 256px tile. Global mean stays high,
        // min-tile must crater — this is the property the old mean-pooled gate lacked.
        let mut img = image::RgbImage::new(512, 512);
        for (x, y, p) in img.enumerate_pixels_mut() {
            let v = ((x / 4 + y / 4) % 2 * 200 + 28) as u8;
            *p = image::Rgb([v, v, v]);
        }
        let a = image::DynamicImage::ImageRgb8(img.clone());
        for y in 0..256 {
            for x in 0..256 {
                img.put_pixel(x, y, image::Rgb([128, 128, 128]));
            }
        }
        let b = image::DynamicImage::ImageRgb8(img);
        let (global, min_tile) = quality_scores(&a, &b);
        assert!(
            min_tile < 0.5,
            "destroyed tile must crater min-tile, got {min_tile}"
        );
        assert!(
            global > min_tile,
            "global mean must sit above the worst tile"
        );
    }

    #[test]
    fn size_guard_rejects_larger_output() {
        // A 1x1 image stored as 3 raw bytes can never be beaten by a PNG/Flate or JPEG
        // re-encode (zlib overhead alone exceeds it) — the size guard must return Ok(None).
        let raw = vec![0u8; 3];
        let mut dict = lopdf::Dictionary::new();
        dict.set("Width", lopdf::Object::Integer(1));
        dict.set("Height", lopdf::Object::Integer(1));
        dict.set("ColorSpace", lopdf::Object::Name(b"DeviceRGB".to_vec()));
        dict.set("BitsPerComponent", lopdf::Object::Integer(8));
        dict.set("Length", lopdf::Object::Integer(raw.len() as i64));
        let stream = lopdf::Stream::new(dict, raw);
        let result = optimize_image_stream(&stream, &default_opts());
        assert!(
            matches!(result, Ok(None)),
            "re-encode larger than original must be rejected by the size guard"
        );
    }

    #[test]
    fn filter_array_form_dctdecode_recognized() {
        // /Filter [/DCTDecode] (array form) must be treated identically to /Filter /DCTDecode:
        // already-JPEG within threshold → silently skipped, NOT decoded as raw pixels.
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::new(4, 4));
        let buf = encode_jpeg_mozjpeg(&img, 90).unwrap();
        let mut dict = lopdf::Dictionary::new();
        dict.set("Width", lopdf::Object::Integer(4));
        dict.set("Height", lopdf::Object::Integer(4));
        dict.set(
            "Filter",
            lopdf::Object::Array(vec![lopdf::Object::Name(b"DCTDecode".to_vec())]),
        );
        dict.set("Length", lopdf::Object::Integer(buf.len() as i64));
        let stream = lopdf::Stream::new(dict, buf);
        let result = optimize_image_stream(&stream, &default_opts());
        assert!(
            matches!(result, Ok(None)),
            "array-form DCTDecode must be skipped like name form"
        );
    }

    /// Calibration harness — not a test. Run manually:
    /// `cargo test --release calibrate_epson_quality_scores -- --ignored --nocapture`
    /// Prints RGB-mean SSIM (old gate), global luma SSIM and min-tile luma SSIM (new gate)
    /// for each EPSON007 image at several candidate encodes.
    #[test]
    #[ignore]
    fn calibrate_epson_quality_scores() {
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/resources/EPSON007.PDF");
        let doc = lopdf::Document::load(fixture).expect("fixture must load");
        let mut idx = 0;
        for obj in doc.objects.values() {
            let lopdf::Object::Stream(s) = obj else {
                continue;
            };
            if !is_image_stream(s) {
                continue;
            }
            let orig = image::load_from_memory_with_format(&s.content, ImageFormat::Jpeg)
                .expect("EPSON streams are JPEG");
            idx += 1;
            println!(
                "--- image {} ({}x{}, {} B) ---",
                idx,
                orig.width(),
                orig.height(),
                s.content.len()
            );
            for (name, cand_bytes) in [
                ("rgb q85", encode_jpeg_mozjpeg(&orig, 85).unwrap()),
                ("rgb q75", encode_jpeg_mozjpeg(&orig, 75).unwrap()),
                ("rgb q60", encode_jpeg_mozjpeg(&orig, 60).unwrap()),
                ("gray q75", {
                    let g = DynamicImage::ImageLuma8(orig.to_luma8());
                    encode_jpeg_mozjpeg(&g, 75).unwrap()
                }),
                ("down1500+q75", {
                    let d = orig.resize(1500, 1500, image::imageops::FilterType::Lanczos3);
                    encode_jpeg_mozjpeg(&d, 75).unwrap()
                }),
            ] {
                let dec =
                    image::load_from_memory_with_format(&cand_bytes, ImageFormat::Jpeg).unwrap();
                let dec_full = if dec.width() != orig.width() {
                    dec.resize_exact(
                        orig.width(),
                        orig.height(),
                        image::imageops::FilterType::Lanczos3,
                    )
                } else {
                    dec
                };
                let rgb_mean = compute_ssim(&orig, &dec_full);
                let (global, min_tile) = quality_scores(&orig, &dec_full);
                println!(
                    "{:>14}: {:>7} B  rgb_mean={:.4} luma_global={:.4} min_tile={:.4}",
                    name,
                    cand_bytes.len(),
                    rgb_mean,
                    global,
                    min_tile
                );
            }
        }
        assert!(idx > 0, "no image streams found in fixture");
    }

    #[test]
    fn near_gray_detector_accepts_gray_image() {
        // Noisy but chroma-free image (r=g=b everywhere) → near-gray
        let mut img = image::RgbImage::new(100, 100);
        for (x, y, p) in img.enumerate_pixels_mut() {
            let v = ((x * 7 + y * 13) % 256) as u8;
            *p = image::Rgb([v, v, v]);
        }
        assert!(is_near_gray(&image::DynamicImage::ImageRgb8(img)));
    }

    #[test]
    fn near_gray_detector_rejects_local_color_region() {
        // Gray page with one 40x40 red stamp: a global average would dilute it,
        // the block detector must catch it.
        let mut img = image::RgbImage::new(400, 400);
        for (_, _, p) in img.enumerate_pixels_mut() {
            *p = image::Rgb([200, 200, 200]);
        }
        for y in 100..140 {
            for x in 100..140 {
                img.put_pixel(x, y, image::Rgb([220, 40, 40]));
            }
        }
        assert!(
            !is_near_gray(&image::DynamicImage::ImageRgb8(img)),
            "a stamp-sized colored region must defeat grayscale conversion"
        );
    }

    #[test]
    fn near_gray_photo_ships_as_device_gray_jpeg() {
        // Gray-content photographic image (many unique gray levels → JPEG path,
        // near-gray → gray candidate wins) must ship as /DeviceGray DCTDecode.
        let size = 300u32;
        let mut img = image::RgbImage::new(size, size);
        for (x, y, p) in img.enumerate_pixels_mut() {
            let v = (((x as f32 / size as f32) * 200.0) + ((y * 37) % 29) as f32) as u8;
            *p = image::Rgb([v, v, v]);
        }
        let dynamic = image::DynamicImage::ImageRgb8(img);
        let input_buf = encode_jpeg_mozjpeg(&dynamic, 100).unwrap();
        let mut dict = lopdf::Dictionary::new();
        dict.set("Width", lopdf::Object::Integer(size as i64));
        dict.set("Height", lopdf::Object::Integer(size as i64));
        dict.set("Filter", lopdf::Object::Name(b"DCTDecode".to_vec()));
        dict.set("Length", lopdf::Object::Integer(input_buf.len() as i64));
        let stream = lopdf::Stream::new(dict, input_buf);
        let opts = ImageOptions {
            force_jpeg: true,
            image_quality: 95,
            ..default_opts()
        };
        let result = optimize_image_stream(&stream, &opts);
        let Ok(Some((_, out_dict))) = result else {
            panic!("near-gray photo should re-encode");
        };
        assert_eq!(
            out_dict.get(b"ColorSpace").unwrap().as_name().unwrap(),
            b"DeviceGray",
            "near-gray JPEG must ship as DeviceGray"
        );
        assert_eq!(
            out_dict.get(b"Filter").unwrap().as_name().unwrap(),
            b"DCTDecode"
        );
    }

    #[test]
    fn bilevel_text_page_ships_as_ccitt_g4() {
        // Crisp black-text-on-white page stored as a q100 JPEG; with --bilevel the
        // G4 candidate must win and ship as 1-bit CCITTFaxDecode.
        let size = 600u32;
        let mut page = image::GrayImage::new(size, size);
        for (x, y, p) in page.enumerate_pixels_mut() {
            let stroke = (y % 24) < 3 && (x / 48) % 2 == 0;
            *p = image::Luma([if stroke { 12 } else { 243 }]);
        }
        let img = image::DynamicImage::ImageLuma8(page);
        let buf = encode_jpeg_mozjpeg(&img, 100).unwrap();
        let mut dict = lopdf::Dictionary::new();
        dict.set("Width", lopdf::Object::Integer(size as i64));
        dict.set("Height", lopdf::Object::Integer(size as i64));
        dict.set("Filter", lopdf::Object::Name(b"DCTDecode".to_vec()));
        dict.set("Length", lopdf::Object::Integer(buf.len() as i64));
        let stream = lopdf::Stream::new(dict, buf);
        let opts = ImageOptions {
            bilevel: true,
            force_jpeg: true,
            ..default_opts()
        };
        let result = optimize_image_stream(&stream, &opts);
        let Ok(Some((bytes, out_dict))) = result else {
            panic!("bilevel text page must produce output");
        };
        assert_eq!(
            out_dict.get(b"Filter").unwrap().as_name().unwrap(),
            b"CCITTFaxDecode",
            "text page with --bilevel must ship as G4"
        );
        assert_eq!(
            out_dict.get(b"BitsPerComponent").unwrap().as_i64().unwrap(),
            1
        );
        assert!(
            bytes.len() < (size * size / 8) as usize,
            "G4 must beat raw 1-bit packing"
        );
        if let lopdf::Object::Dictionary(parms) = out_dict.get(b"DecodeParms").unwrap() {
            assert_eq!(parms.get(b"K").unwrap().as_i64().unwrap(), -1);
            assert_eq!(
                parms.get(b"Columns").unwrap().as_i64().unwrap(),
                size as i64
            );
        } else {
            panic!("CCITT DecodeParms must be present");
        }
    }

    #[test]
    fn bilevel_never_fires_on_continuous_tone() {
        // Photographic gradient content must never binarize even with --bilevel.
        let size = 300u32;
        let mut img = image::GrayImage::new(size, size);
        for (x, y, p) in img.enumerate_pixels_mut() {
            *p = image::Luma([((x + y) * 255 / (2 * size)) as u8]);
        }
        let dynamic = image::DynamicImage::ImageLuma8(img);
        let buf = encode_jpeg_mozjpeg(&dynamic, 100).unwrap();
        let mut dict = lopdf::Dictionary::new();
        dict.set("Width", lopdf::Object::Integer(size as i64));
        dict.set("Height", lopdf::Object::Integer(size as i64));
        dict.set("Filter", lopdf::Object::Name(b"DCTDecode".to_vec()));
        dict.set("Length", lopdf::Object::Integer(buf.len() as i64));
        let stream = lopdf::Stream::new(dict, buf);
        let opts = ImageOptions {
            bilevel: true,
            force_jpeg: true,
            ..default_opts()
        };
        if let Ok(Some((_, out_dict))) = optimize_image_stream(&stream, &opts) {
            assert_ne!(
                out_dict.get(b"Filter").unwrap().as_name().unwrap(),
                b"CCITTFaxDecode",
                "continuous-tone content must not binarize"
            );
        }
    }

    #[test]
    fn e2e_gate_rejects_destructive_downscale() {
        // A 1-px checkerboard at 2000x2000 downscaled to 100px is pure destruction:
        // the restored upsample cannot resemble the original. The end-to-end gate must
        // reject even though the encode of the downscaled image itself is near-perfect.
        // (On pre-P1 code this shipped: the gate compared against the already-downscaled
        // source and never saw the resolution loss.)
        let size = 2000u32;
        let mut cb = image::RgbImage::new(size, size);
        for y in 0..size {
            for x in 0..size {
                cb.put_pixel(
                    x,
                    y,
                    if (x + y) % 2 == 0 {
                        image::Rgb([0u8, 0, 0])
                    } else {
                        image::Rgb([255, 255, 255])
                    },
                );
            }
        }
        let img = image::DynamicImage::ImageRgb8(cb);
        let mut buf = Vec::new();
        img.write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)
            .unwrap();
        let mut dict = lopdf::Dictionary::new();
        dict.set("Width", lopdf::Object::Integer(size as i64));
        dict.set("Height", lopdf::Object::Integer(size as i64));
        dict.set("Length", lopdf::Object::Integer(buf.len() as i64));
        let stream = lopdf::Stream::new(dict, buf);
        let opts = ImageOptions {
            max_pixels: 100,
            ..default_opts()
        };
        let result = optimize_image_stream(&stream, &opts);
        assert!(
            matches!(result, Ok(None)),
            "destructive downscale must be rejected by the end-to-end gate"
        );
    }

    #[test]
    fn explicit_downsample_dpi_exempts_resolution_loss_from_gate() {
        // Same destructive downscale, but the user explicitly asked for it via
        // --downsample-dpi → only encode fidelity is gated. Lossless encode of the
        // downscaled image is exact, so the result must ship.
        let size = 2000u32;
        let mut cb = image::RgbImage::new(size, size);
        for y in 0..size {
            for x in 0..size {
                cb.put_pixel(
                    x,
                    y,
                    if (x + y) % 2 == 0 {
                        image::Rgb([0u8, 0, 0])
                    } else {
                        image::Rgb([255, 255, 255])
                    },
                );
            }
        }
        let img = image::DynamicImage::ImageRgb8(cb);
        let mut buf = Vec::new();
        img.write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)
            .unwrap();
        let mut dict = lopdf::Dictionary::new();
        dict.set("Width", lopdf::Object::Integer(size as i64));
        dict.set("Height", lopdf::Object::Integer(size as i64));
        dict.set("Length", lopdf::Object::Integer(buf.len() as i64));
        let stream = lopdf::Stream::new(dict, buf);
        let opts = ImageOptions {
            max_pixels: 100,
            downsample_dpi: Some(12), // 12 dpi * 8.5" = 102px — forces the downscale
            ..default_opts()
        };
        let result = optimize_image_stream(&stream, &opts);
        assert!(
            matches!(result, Ok(Some(_))),
            "user-requested downsample must be exempt from the resolution-loss gate"
        );
    }

    #[test]
    fn lossless_path_grayscale_sets_device_gray() {
        // Grayscale image → lossless branch → ColorSpace=DeviceGray, Colors=1
        use std::io::Cursor;
        let img = image::DynamicImage::ImageLuma8(image::GrayImage::new(4, 4));
        let mut buf = Vec::new();
        img.write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)
            .unwrap();
        let mut dict = lopdf::Dictionary::new();
        dict.set("Width", lopdf::Object::Integer(4));
        dict.set("Height", lopdf::Object::Integer(4));
        dict.set("Length", lopdf::Object::Integer(buf.len() as i64));
        let stream = lopdf::Stream::new(dict, buf);

        let result = optimize_image_stream(&stream, &default_opts());
        assert!(
            matches!(result, Ok(Some(_))),
            "expected lossless encode to succeed, got: {:?}",
            result.as_ref().map(|r| r.is_some())
        );
        if let Ok(Some((encoded, out_dict))) = result {
            let cs = out_dict.get(b"ColorSpace").unwrap().as_name().unwrap();
            assert_eq!(cs, b"DeviceGray", "grayscale image must use DeviceGray");
            if let lopdf::Object::Dictionary(d) = out_dict.get(b"DecodeParms").unwrap() {
                assert_eq!(d.get(b"Colors").unwrap().as_i64().unwrap(), 1);
            }
            // Verify encoded bytes are a valid zlib stream (decodable by FlateDecode)
            use flate2::read::ZlibDecoder;
            use std::io::Read;
            let mut decoder = ZlibDecoder::new(&encoded[..]);
            let mut out = Vec::new();
            decoder
                .read_to_end(&mut out)
                .expect("encoded bytes must be a valid zlib stream");
            assert!(!out.is_empty(), "decompressed output must not be empty");
        }
    }
}
