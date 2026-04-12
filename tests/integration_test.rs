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
