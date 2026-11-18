//! End-to-end tests running the compressor against generated PDFs.
//!
//! Author: Moujahed Khouja

use std::fs;
use tempfile::TempDir;

/// Builds a minimal but valid PDF as bytes using lopdf.
fn make_minimal_pdf() -> Vec<u8> {
    use lopdf::{Dictionary, Document, Object};
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let page_id = doc.new_object_id();

    let mut pages_dict = Dictionary::new();
    pages_dict.set("Type", Object::Name(b"Pages".to_vec()));
    pages_dict.set("Kids", Object::Array(vec![Object::Reference(page_id)]));
    pages_dict.set("Count", Object::Integer(1));
    doc.objects.insert(pages_id, Object::Dictionary(pages_dict));

    let mut page_dict = Dictionary::new();
    page_dict.set("Type", Object::Name(b"Page".to_vec()));
    page_dict.set("Parent", Object::Reference(pages_id));
    doc.objects.insert(page_id, Object::Dictionary(page_dict));

    let mut catalog = Dictionary::new();
    catalog.set("Type", Object::Name(b"Catalog".to_vec()));
    catalog.set("Pages", Object::Reference(pages_id));
    let catalog_id = doc.add_object(catalog);
    doc.trailer.set("Root", Object::Reference(catalog_id));

    let mut buf = Vec::new();
    doc.save_to(&mut buf).unwrap();
    buf
}

#[test]
fn compress_file_lossless_produces_valid_pdf() {
    let dir = TempDir::new().unwrap();
    let input_path = dir.path().join("input.pdf");
    let output_path = dir.path().join("output.pdf");
    fs::write(&input_path, make_minimal_pdf()).unwrap();

    let opts = pdf_minimizer::compressor::CompressOptions {
        aggressive: false,
        max_pixels: 1500,
        image_quality: 85,
        smoothing: 0,
        downsample_dpi: None,
        force_jpeg: false,
        bilevel: false,
        strip_metadata: false,
        dry_run: false,
        force: true,
        zopfli: false,
        max_decompressed_bytes: 64 * 1024 * 1024,
    };
    let stats = pdf_minimizer::compressor::compress_file(
        input_path.to_str().unwrap(),
        output_path.to_str().unwrap(),
        &opts,
    )
    .unwrap();

    assert!(output_path.exists());
    assert!(stats.compressed_bytes > 0);
    assert!(stats.compressed_bytes <= stats.original_bytes);
    // Verify output is parseable as PDF
    lopdf::Document::load(&output_path).unwrap();
}

#[test]
fn compress_file_dry_run_does_not_write() {
    let dir = TempDir::new().unwrap();
    let input_path = dir.path().join("input.pdf");
    let output_path = dir.path().join("output.pdf");
    fs::write(&input_path, make_minimal_pdf()).unwrap();

    let opts = pdf_minimizer::compressor::CompressOptions {
        aggressive: false,
        max_pixels: 1500,
        image_quality: 85,
        smoothing: 0,
        downsample_dpi: None,
        force_jpeg: false,
        bilevel: false,
        strip_metadata: false,
        dry_run: true,
        force: false,
        zopfli: false,
        max_decompressed_bytes: 64 * 1024 * 1024,
    };
    pdf_minimizer::compressor::compress_file(
        input_path.to_str().unwrap(),
        output_path.to_str().unwrap(),
        &opts,
    )
    .unwrap();

    assert!(!output_path.exists(), "dry-run must not write output file");
}

/// Compress the real scanned-document fixture end-to-end (aggressive mode) and validate
/// the output with `qpdf --check` when qpdf is installed. Skips silently otherwise.
#[test]
fn compress_epson_fixture_qpdf_check() {
    let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/resources/EPSON007.PDF");
    if !std::path::Path::new(fixture).exists() {
        return;
    }
    let dir = TempDir::new().unwrap();
    let output_path = dir.path().join("epson_out.pdf");

    let opts = pdf_minimizer::compressor::CompressOptions {
        aggressive: true,
        max_pixels: 1500,
        image_quality: 75,
        smoothing: 0,
        downsample_dpi: None,
        force_jpeg: false,
        bilevel: false,
        strip_metadata: false,
        dry_run: false,
        force: true,
        zopfli: false,
        max_decompressed_bytes: 64 * 1024 * 1024,
    };
    let stats =
        pdf_minimizer::compressor::compress_file(fixture, output_path.to_str().unwrap(), &opts)
            .unwrap();

    assert!(stats.compressed_bytes > 0);
    assert!(
        stats.compressed_bytes < stats.original_bytes,
        "aggressive mode must shrink the scan fixture"
    );
    // Output must re-parse with lopdf regardless of qpdf availability
    lopdf::Document::load(&output_path).unwrap();

    // Structural validation with qpdf when available
    if let Ok(out) = std::process::Command::new("qpdf")
        .arg("--check")
        .arg(&output_path)
        .output()
    {
        assert!(
            out.status.success(),
            "qpdf --check failed:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // Upper-bound regression: adaptive quality with strict global/tile/chroma floors
    // should beat the previous fixed-q75 result (~361 KB). Quality-floor drift is
    // pinned separately by the quality_gate_boundary unit test; do not impose a
    // lower size bound that would reject future compression improvements.
    assert!(
        stats.compressed_bytes <= 345_000,
        "EPSON007 adaptive output exceeded regression ceiling: {} bytes",
        stats.compressed_bytes
    );
}

/// Committed synthetic scan corpus (regenerate: `cargo run --release --example
/// gen_fixture`). Unlike the EPSON fixture this one is in git, so it MUST exist —
/// no silent skip — and it keeps corpus regression coverage alive in CI.
#[test]
fn compress_synthetic_fixture_bands() {
    let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/resources/fixture_scan.pdf");
    assert!(
        std::path::Path::new(fixture).exists(),
        "committed fixture missing — run `cargo run --release --example gen_fixture`"
    );
    let dir = TempDir::new().unwrap();
    let output_path = dir.path().join("fixture_out.pdf");

    let opts = pdf_minimizer::compressor::CompressOptions {
        aggressive: false,
        max_pixels: 1500,
        image_quality: 75,
        smoothing: 0,
        downsample_dpi: None,
        force_jpeg: false,
        bilevel: false,
        strip_metadata: false,
        dry_run: false,
        force: true,
        zopfli: false,
        max_decompressed_bytes: 64 * 1024 * 1024,
    };
    let stats =
        pdf_minimizer::compressor::compress_file(fixture, output_path.to_str().unwrap(), &opts)
            .unwrap();

    // Lossless mode: the fixture's JPEG is deliberately unoptimized (scanner-style),
    // so the coefficient transcode must recover at least 35%.
    let reduction = 1.0 - stats.compressed_bytes as f64 / stats.original_bytes as f64;
    assert!(
        reduction >= 0.35,
        "lossless transcode reduction regressed: {:.1}% (expected >= 35%)",
        reduction * 100.0
    );
    // Output must re-parse and pass structural validation
    lopdf::Document::load(&output_path).unwrap();
    if let Ok(out) = std::process::Command::new("qpdf")
        .arg("--check")
        .arg(&output_path)
        .output()
    {
        assert!(
            out.status.success(),
            "qpdf --check failed:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn compress_file_strip_metadata_succeeds() {
    let dir = TempDir::new().unwrap();
    let input_path = dir.path().join("input.pdf");
    let output_path = dir.path().join("output.pdf");
    fs::write(&input_path, make_minimal_pdf()).unwrap();

    let opts = pdf_minimizer::compressor::CompressOptions {
        aggressive: false,
        max_pixels: 1500,
        image_quality: 85,
        smoothing: 0,
        downsample_dpi: None,
        force_jpeg: false,
        bilevel: false,
        strip_metadata: true,
        dry_run: false,
        force: true,
        zopfli: false,
        max_decompressed_bytes: 64 * 1024 * 1024,
    };
    let stats = pdf_minimizer::compressor::compress_file(
        input_path.to_str().unwrap(),
        output_path.to_str().unwrap(),
        &opts,
    )
    .unwrap();

    assert!(output_path.exists());
    assert!(stats.compressed_bytes > 0);
    // Output must still be a valid PDF after metadata stripping
    lopdf::Document::load(&output_path).unwrap();
}

#[test]
fn compress_file_can_atomically_replace_input_path() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("input.pdf");
    fs::write(&path, make_minimal_pdf()).unwrap();

    let opts = pdf_minimizer::compressor::CompressOptions {
        aggressive: false,
        max_pixels: 1500,
        image_quality: 85,
        smoothing: 0,
        downsample_dpi: None,
        force_jpeg: false,
        bilevel: false,
        strip_metadata: false,
        dry_run: false,
        force: true,
        zopfli: false,
        max_decompressed_bytes: 64 * 1024 * 1024,
    };
    pdf_minimizer::compressor::compress_file(path.to_str().unwrap(), path.to_str().unwrap(), &opts)
        .unwrap();

    lopdf::Document::load(&path).unwrap();
}

/// Builds a one-page PDF whose only image is an Adobe-style CMYK JPEG (APP14,
/// YCCK transform, samples stored inverted: 255 = no ink), the structure that
/// `--recommended` used to turn into a near-black DeviceRGB image.
fn make_adobe_cmyk_pdf(width: u32, height: u32) -> Vec<u8> {
    use lopdf::{Dictionary, Document, Object, Stream};

    let mut px = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            let k_ink = y > height / 3 && y < height / 3 + 10 && x > 30 && x < width - 30;
            px.extend_from_slice(if k_ink {
                &[255, 255, 255, 0]
            } else {
                &[255; 4]
            });
        }
    }
    let mut comp = mozjpeg::Compress::new(mozjpeg::ColorSpace::JCS_CMYK);
    comp.set_color_space(mozjpeg::ColorSpace::JCS_YCCK);
    comp.set_size(width as usize, height as usize);
    comp.set_quality(95.0);
    let mut started = comp.start_compress(Vec::new()).unwrap();
    started.write_scanlines(&px).unwrap();
    let jpeg = started.finish().unwrap();

    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();

    let mut img = Dictionary::new();
    img.set("Type", Object::Name(b"XObject".to_vec()));
    img.set("Subtype", Object::Name(b"Image".to_vec()));
    img.set("Width", Object::Integer(width as i64));
    img.set("Height", Object::Integer(height as i64));
    img.set("BitsPerComponent", Object::Integer(8));
    img.set("ColorSpace", Object::Name(b"DeviceCMYK".to_vec()));
    img.set("Filter", Object::Name(b"DCTDecode".to_vec()));
    let img_id = doc.add_object(Stream::new(img, jpeg));

    let content = b"q 595 0 0 842 0 0 cm /Im0 Do Q".to_vec();
    let content_id = doc.add_object(Stream::new(Dictionary::new(), content));

    let mut xobjects = Dictionary::new();
    xobjects.set("Im0", Object::Reference(img_id));
    let mut resources = Dictionary::new();
    resources.set("XObject", Object::Dictionary(xobjects));

    let mut page = Dictionary::new();
    page.set("Type", Object::Name(b"Page".to_vec()));
    page.set("Parent", Object::Reference(pages_id));
    page.set(
        "MediaBox",
        Object::Array(vec![0.into(), 0.into(), 595.into(), 842.into()]),
    );
    page.set("Resources", Object::Dictionary(resources));
    page.set("Contents", Object::Reference(content_id));
    let page_id = doc.add_object(page);

    let mut pages = Dictionary::new();
    pages.set("Type", Object::Name(b"Pages".to_vec()));
    pages.set("Kids", Object::Array(vec![Object::Reference(page_id)]));
    pages.set("Count", Object::Integer(1));
    doc.objects.insert(pages_id, Object::Dictionary(pages));

    let mut catalog = Dictionary::new();
    catalog.set("Type", Object::Name(b"Catalog".to_vec()));
    catalog.set("Pages", Object::Reference(pages_id));
    let catalog_id = doc.add_object(catalog);
    doc.trailer.set("Root", Object::Reference(catalog_id));

    let mut buf = Vec::new();
    doc.save_to(&mut buf).unwrap();
    buf
}

/// Regression: `--recommended` (aggressive + 120 DPI + q75 + force_jpeg + zopfli)
/// must keep a CMYK JPEG in its own color model instead of re-encoding the
/// misinterpreted pixels as DeviceRGB (rendered black where the paper was white).
#[test]
fn recommended_preset_preserves_adobe_cmyk_jpeg() {
    let dir = TempDir::new().unwrap();
    let input_path = dir.path().join("cmyk.pdf");
    let output_path = dir.path().join("cmyk_out.pdf");
    fs::write(&input_path, make_adobe_cmyk_pdf(1200, 870)).unwrap();

    let opts = pdf_minimizer::compressor::CompressOptions {
        aggressive: true,
        max_pixels: 1500,
        image_quality: 75,
        smoothing: 0,
        downsample_dpi: Some(120),
        force_jpeg: true,
        bilevel: false,
        strip_metadata: false,
        dry_run: false,
        force: true,
        zopfli: true,
        max_decompressed_bytes: 64 * 1024 * 1024,
    };
    pdf_minimizer::compressor::compress_file(
        input_path.to_str().unwrap(),
        output_path.to_str().unwrap(),
        &opts,
    )
    .unwrap();

    let doc = lopdf::Document::load(&output_path).unwrap();
    let images: Vec<_> = doc
        .objects
        .values()
        .filter_map(|o| o.as_stream().ok())
        .filter(|s| {
            s.dict
                .get(b"Subtype")
                .and_then(|o| o.as_name())
                .is_ok_and(|n| n == b"Image")
        })
        .collect();
    assert_eq!(images.len(), 1);
    let img = images[0];
    assert_eq!(
        img.dict.get(b"ColorSpace").unwrap().as_name().unwrap(),
        b"DeviceCMYK",
        "CMYK image was re-encoded into another color model"
    );
    assert!(
        img.content.windows(2).any(|w| w == [0xFF, 0xEE]),
        "Adobe APP14 marker (inverted-CMYK flag) must survive"
    );
    if let Ok(out) = std::process::Command::new("qpdf")
        .arg("--check")
        .arg(&output_path)
        .output()
    {
        assert!(out.status.success(), "qpdf --check failed");
    }
}
