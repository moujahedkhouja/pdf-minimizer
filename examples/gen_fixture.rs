//! Regenerates `resources/fixture_scan.pdf` — the committed synthetic test corpus.
//!
//! Run: `cargo run --release --example gen_fixture`
//!
//! The fixture imitates a one-page office scan: noisy near-white paper, black
//! "text" strokes, and one colored stamp block, stored as a DCTDecode image
//! encoded WITHOUT Huffman optimization (like real scanner firmware), so the
//! lossless transcode pass has real work to do. Fully deterministic — no RNG,
//! no timestamps — so the committed bytes only change when this code changes.

use lopdf::{dictionary, Document, Object, Stream};
use std::io::Cursor;

fn scan_like_page(width: u32, height: u32) -> image::RgbImage {
    let mut img = image::RgbImage::new(width, height);
    for (x, y, p) in img.enumerate_pixels_mut() {
        // Paper: near-white with deterministic grain
        let grain = ((x * 7 + y * 13) % 11) as u8;
        let mut r = 238 + grain;
        let mut g = 236 + grain;
        let mut b = 233 + grain;
        // Text: horizontal stroke bands broken into word-like runs
        let stroke = (y % 28) < 4 && (x / 60) % 2 == 0 && x > 40 && x < width - 40;
        if stroke {
            let ink = 18 + ((x * 3 + y) % 13) as u8;
            r = ink;
            g = ink;
            b = ink;
        }
        // Colored stamp block (defeats near-gray/grayscale conversion, like a logo)
        if (60..220).contains(&x) && (60..140).contains(&y) {
            r = 205;
            g = 40;
            b = 40;
        }
        *p = image::Rgb([r, g, b]);
    }
    img
}

fn main() {
    let (w, h) = (1240u32, 1754u32); // ~150 dpi A4
    let page_img = scan_like_page(w, h);

    // Unoptimized-Huffman JPEG via the image crate (mirrors scanner output)
    let mut jpeg = Vec::new();
    image::DynamicImage::ImageRgb8(page_img)
        .write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(
            &mut Cursor::new(&mut jpeg),
            82,
        ))
        .expect("jpeg encode");

    let mut doc = Document::with_version("1.4");
    let img_id = doc.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => w as i64,
            "Height" => h as i64,
            "ColorSpace" => "DeviceRGB",
            "BitsPerComponent" => 8,
            "Filter" => "DCTDecode",
            "Length" => jpeg.len() as i64,
        },
        jpeg,
    ));
    let content = b"q 595 0 0 842 0 0 cm /Im0 Do Q".to_vec();
    let content_id = doc.add_object(Stream::new(
        dictionary! { "Length" => content.len() as i64 },
        content,
    ));
    let pages_id = doc.new_object_id();
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => Object::Reference(pages_id),
        "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        "Contents" => Object::Reference(content_id),
        "Resources" => dictionary! {
            "XObject" => dictionary! { "Im0" => Object::Reference(img_id) },
        },
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![Object::Reference(page_id)],
            "Count" => 1,
        }),
    );
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => Object::Reference(pages_id),
    });
    doc.trailer.set("Root", Object::Reference(catalog_id));

    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/resources/fixture_scan.pdf");
    doc.save(path).expect("save fixture");
    println!(
        "wrote {} ({} bytes)",
        path,
        std::fs::metadata(path).unwrap().len()
    );
}
