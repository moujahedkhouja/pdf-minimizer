//! Bilevel (1-bit) re-encoding of text-only scan pages as CCITT Group 4.
//!
//! Opt-in via `--bilevel`: binarization intentionally drops paper texture and
//! grayscale shading, so it must never be a silent default. Three safeguards run
//! before a G4 candidate can ship:
//! 1. spatial near-gray check (color content disqualifies the page),
//! 2. Otsu midtone-fraction check (continuous-tone content disqualifies),
//! 3. a perceptual gate comparing blurred renderings of BOTH the 1-bit mask and the
//!    gray source (symmetric blur measures structure at viewing scale: it forgives
//!    antialiased-edge crushing that a viewer never perceives, while gradients and
//!    texture destroyed by binarization stay destroyed after blurring and fail).

use fax::encoder::Encoder;
use fax::{Color, VecWriter};
use image::{DynamicImage, GrayImage};

/// Maximum fraction of pixels in the midtone luma band for a page to count as bilevel.
/// Text scans measure ~1–3%; continuous-tone content is far higher.
pub const MIDTONE_FRACTION_LIMIT: f64 = 0.03;
/// The "midtone" luma band: pixels that are neither near-black ink nor near-white paper.
const MIDTONE_LO: u8 = 64;
const MIDTONE_HI: u8 = 191;
/// Blur radius applied to both sides before the mask-vs-source comparison.
const MASK_BLUR_SIGMA: f32 = 1.5;
/// Minimum global luma SSIM of blur(mask) vs blur(source).
pub const BILEVEL_SSIM_GLOBAL: f64 = 0.90;
/// Minimum worst-tile luma SSIM of blur(mask) vs blur(source).
pub const BILEVEL_TILE_SSIM: f64 = 0.75;

/// Otsu's method: luma threshold maximizing between-class variance.
pub fn otsu_threshold(luma: &GrayImage) -> u8 {
    let mut hist = [0u64; 256];
    for p in luma.pixels() {
        hist[p.0[0] as usize] += 1;
    }
    let total: u64 = hist.iter().sum();
    if total == 0 {
        return 128;
    }
    let sum_all: f64 = hist
        .iter()
        .enumerate()
        .map(|(v, &c)| v as f64 * c as f64)
        .sum();

    let (mut w_bg, mut sum_bg) = (0f64, 0f64);
    let (mut best_thr, mut best_var) = (128u8, 0f64);
    for (t, &count) in hist.iter().enumerate() {
        w_bg += count as f64;
        if w_bg == 0.0 {
            continue;
        }
        let w_fg = total as f64 - w_bg;
        if w_fg == 0.0 {
            break;
        }
        sum_bg += t as f64 * count as f64;
        let mean_bg = sum_bg / w_bg;
        let mean_fg = (sum_all - sum_bg) / w_fg;
        let var = w_bg * w_fg * (mean_bg - mean_fg).powi(2);
        if var > best_var {
            best_var = var;
            best_thr = t as u8;
        }
    }
    best_thr
}

/// Fraction of pixels in the midtone luma band (neither ink-dark nor paper-bright).
/// High values mean real grayscale content that binarization would destroy.
pub fn midtone_fraction(luma: &GrayImage) -> f64 {
    let mid = luma
        .pixels()
        .filter(|p| (MIDTONE_LO..=MIDTONE_HI).contains(&p.0[0]))
        .count();
    mid as f64 / luma.pixels().len() as f64
}

/// Binarize and encode as CCITT Group 4 (K = -1). Returns the G4 payload.
///
/// Otsu convention: values <= threshold are class 0 (ink/black).
/// Bit semantics follow the CCITTFaxDecode defaults (BlackIs1 = false → decoded
/// 0 = black), matching /DeviceGray /BitsPerComponent 1 where sample 0 is black.
pub fn encode_g4(luma: &GrayImage, threshold: u8) -> Option<Vec<u8>> {
    let (w, h) = luma.dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    let mut enc = Encoder::new(VecWriter::new());
    for y in 0..h {
        let line = (0..w).map(|x| {
            if luma.get_pixel(x, y).0[0] <= threshold {
                Color::Black
            } else {
                Color::White
            }
        });
        enc.encode_line(line, w).ok()?;
    }
    let writer = enc.finish().ok()?;
    Some(writer.finish())
}

/// Render the 1-bit mask back to an 8-bit gray image (for the perceptual gate).
fn mask_to_gray(luma: &GrayImage, threshold: u8) -> GrayImage {
    let (w, h) = luma.dimensions();
    let mut out = GrayImage::new(w, h);
    for (x, y, p) in luma.enumerate_pixels() {
        let v = if p.0[0] <= threshold { 0u8 } else { 255u8 };
        out.put_pixel(x, y, image::Luma([v]));
    }
    out
}

/// Classify + binarize + gate + encode. `quality` is the caller's scoring function
/// over `(original_gray, candidate_gray)` returning `(global, min_tile)` luma SSIM.
///
/// Returns `Some(g4_bytes)` only when the page is structurally bilevel AND the
/// blurred mask passes the perceptual gate.
pub fn try_bilevel<F>(img: &DynamicImage, quality: F) -> Option<Vec<u8>>
where
    F: Fn(&DynamicImage, &DynamicImage) -> (f64, f64),
{
    let luma = img.to_luma8();
    let threshold = otsu_threshold(&luma);
    if midtone_fraction(&luma) > MIDTONE_FRACTION_LIMIT {
        return None;
    }

    // Perceptual gate: symmetric blur on both sides (structure at viewing scale)
    let mask = mask_to_gray(&luma, threshold);
    let blurred_mask = image::imageops::blur(&mask, MASK_BLUR_SIGMA);
    let blurred_src = image::imageops::blur(&luma, MASK_BLUR_SIGMA);
    let (global, min_tile) = quality(
        &DynamicImage::ImageLuma8(blurred_src),
        &DynamicImage::ImageLuma8(blurred_mask),
    );
    if global < BILEVEL_SSIM_GLOBAL || min_tile < BILEVEL_TILE_SSIM {
        return None;
    }

    encode_g4(&luma, threshold)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_like_page(size: u32) -> GrayImage {
        // White page with black "text" strokes: crisp bimodal histogram
        let mut img = GrayImage::new(size, size);
        for (x, y, p) in img.enumerate_pixels_mut() {
            let stroke = (y % 20) < 3 && (x / 40) % 2 == 0;
            *p = image::Luma([if stroke { 10 } else { 245 }]);
        }
        img
    }

    #[test]
    fn otsu_separates_bimodal_histogram() {
        // Otsu convention: class 0 = values <= threshold, so for modes {10, 245}
        // any threshold in [10, 244] separates them.
        let img = text_like_page(200);
        let thr = otsu_threshold(&img);
        assert!((10..245).contains(&(thr as i32)), "threshold must separate the modes, got {thr}");
    }

    #[test]
    fn midtone_fraction_low_for_text_high_for_gradient() {
        let text = text_like_page(200);
        assert!(midtone_fraction(&text) < 0.01);

        let mut gradient = GrayImage::new(200, 200);
        for (x, _, p) in gradient.enumerate_pixels_mut() {
            *p = image::Luma([(x * 255 / 200) as u8]);
        }
        assert!(
            midtone_fraction(&gradient) > MIDTONE_FRACTION_LIMIT,
            "smooth gradient must be classified as continuous-tone"
        );
    }

    #[test]
    fn encode_g4_roundtrips_via_fax_decoder() {
        let img = text_like_page(64);
        let thr = otsu_threshold(&img);
        let g4 = encode_g4(&img, thr).expect("encode must succeed");
        assert!(!g4.is_empty());
        // The G4 payload must be dramatically smaller than raw 1-bit packing
        assert!(g4.len() < (64 * 64 / 8));

        // Round-trip through the fax decoder and verify pixel-exact binarization
        let mut rows: Vec<Vec<fax::Color>> = Vec::new();
        fax::decoder::decode_g4(g4.iter().copied(), 64, None, |transitions| {
            rows.push(fax::decoder::pels(transitions, 64).collect());
        });
        assert_eq!(rows.len(), 64);
        for (y, row) in rows.iter().enumerate() {
            for (x, c) in row.iter().enumerate() {
                let expected = if img.get_pixel(x as u32, y as u32).0[0] <= thr {
                    fax::Color::Black
                } else {
                    fax::Color::White
                };
                assert_eq!(*c, expected, "pixel mismatch at {x},{y}");
            }
        }
    }
}
