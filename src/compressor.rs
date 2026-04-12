use crate::image_opt::{optimize_images, ImageOptions};
use crate::stats::FileStats;
use flate2::{write::ZlibEncoder, Compression};
use lopdf::{Document, LoadOptions, Object, ObjectId, SaveOptions};
use std::collections::HashSet;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use tempfile::NamedTempFile;

/// Deflate `raw` to a zlib stream — Zopfli (slow, a few % denser) or flate2 level 9.
fn deflate_zlib(raw: &[u8], zopfli: bool) -> Option<Vec<u8>> {
    if zopfli {
        let mut out = Vec::new();
        zopfli::compress(
            zopfli::Options::default(),
            zopfli::Format::Zlib,
            raw,
            &mut out,
        )
        .ok()?;
        Some(out)
    } else {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(raw).ok()?;
        encoder.finish().ok()
    }
}

/// Recompress all PDF streams that are not already FlateDecode-encoded.
///
/// With `zopfli` set, already-FlateDecode streams are additionally re-deflated with
/// Zopfli: the raw zlib payload is inflated and re-deflated byte-for-byte, so the
/// stream dictionary (Filter, DecodeParms) stays untouched and semantics are identical.
pub fn recompress_streams(doc: &mut Document, zopfli: bool, max_decompressed_bytes: usize) {
    let ids: Vec<_> = doc.objects.keys().cloned().collect();
    for id in ids {
        let obj = doc.objects.get_mut(&id).unwrap();
        if let Object::Stream(stream) = obj {
            // Already FlateDecode (handles both /FlateDecode and [/FlateDecode] forms)
            if let Ok(filters) = stream.filters() {
                if filters == vec![b"FlateDecode" as &[u8]] {
                    if zopfli {
                        // Inflate the zlib payload directly (NOT decompressed_content,
                        // which would also undo predictors) and re-deflate with Zopfli.
                        let mut inflater = flate2::read::ZlibDecoder::new(&stream.content[..]);
                        let mut raw = Vec::new();
                        if inflater
                            .by_ref()
                            .take(max_decompressed_bytes.saturating_add(1) as u64)
                            .read_to_end(&mut raw)
                            .is_err()
                            || raw.len() > max_decompressed_bytes
                        {
                            continue;
                        }
                        if let Some(compressed) = deflate_zlib(&raw, true) {
                            if compressed.len() < stream.content.len() {
                                stream.content = compressed;
                                stream
                                    .dict
                                    .set("Length", Object::Integer(stream.content.len() as i64));
                            }
                        }
                    }
                    continue;
                }
            }
            let has_filter = stream.dict.get(b"Filter").is_ok();
            // Decompress to raw bytes
            // If there's no Filter, the content is already raw
            let raw = if !has_filter {
                if stream.content.len() > max_decompressed_bytes {
                    continue;
                }
                stream.content.clone()
            } else {
                match stream.decompressed_content_with_limit(max_decompressed_bytes) {
                    Ok(bytes) => bytes,
                    Err(_) => continue, // skip streams we can't decompress
                }
            };
            let compressed = match deflate_zlib(&raw, zopfli) {
                Some(b) => b,
                None => continue,
            };
            // Only replace if we actually made it smaller
            if compressed.len() < stream.content.len() {
                stream.content = compressed;
                stream
                    .dict
                    .set("Filter", Object::Name(b"FlateDecode".to_vec()));
                stream.dict.remove(b"DecodeParms");
                stream
                    .dict
                    .set("Length", Object::Integer(stream.content.len() as i64));
            }
        }
    }
}

/// Collect all object IDs reachable from the given object.
fn collect_reachable(doc: &Document, obj: &Object, seen: &mut HashSet<ObjectId>) {
    match obj {
        Object::Reference(id) => {
            if seen.insert(*id) {
                if let Some(child) = doc.objects.get(id) {
                    collect_reachable(doc, child, seen);
                }
            }
        }
        Object::Array(arr) => {
            for item in arr {
                collect_reachable(doc, item, seen);
            }
        }
        Object::Dictionary(dict) => {
            for (_, val) in dict.iter() {
                collect_reachable(doc, val, seen);
            }
        }
        Object::Stream(stream) => {
            for (_, val) in stream.dict.iter() {
                collect_reachable(doc, val, seen);
            }
        }
        _ => {}
    }
}

/// Remove all PDF objects not reachable from the document roots (trailer dictionary).
pub fn prune_dead_objects(doc: &mut Document) {
    let mut reachable = HashSet::new();
    // Traverse trailer dictionary to find all roots
    let trailer_vals: Vec<Object> = doc.trailer.iter().map(|(_, v)| v.clone()).collect();
    for val in &trailer_vals {
        collect_reachable(doc, val, &mut reachable);
    }
    doc.objects.retain(|id, _| reachable.contains(id));
}

/// Remove author, title, creator, producer, and date metadata from /Info dictionary.
pub fn strip_metadata(doc: &mut Document) {
    // Remove /Info object entirely and deref from trailer
    let info_ref = doc
        .trailer
        .get(b"Info")
        .ok()
        .and_then(|o| o.as_reference().ok());
    if let Some(info_id) = info_ref {
        doc.objects.remove(&info_id);
        doc.trailer.remove(b"Info");
    }
    // Remove XMP metadata stream referenced from document catalog
    if let Ok(root_ref) = doc.trailer.get(b"Root").and_then(|o| o.as_reference()) {
        let metadata_ref = doc
            .objects
            .get(&root_ref)
            .and_then(|o| {
                if let Object::Dictionary(d) = o {
                    Some(d)
                } else {
                    None
                }
            })
            .and_then(|d| d.get(b"Metadata").ok())
            .and_then(|o| o.as_reference().ok());
        if let Some(meta_id) = metadata_ref {
            doc.objects.remove(&meta_id);
            if let Some(Object::Dictionary(catalog)) = doc.objects.get_mut(&root_ref) {
                catalog.remove(b"Metadata");
            }
        }
    }
}

/// Bump the document header version to 1.5 if it is lower.
/// Object streams and cross-reference streams are PDF 1.5 features.
fn ensure_min_version_1_5(doc: &mut Document) {
    let below_1_5 = doc
        .version
        .strip_prefix("1.")
        .and_then(|minor| minor.parse::<u32>().ok())
        .map(|minor| minor < 5)
        .unwrap_or(false);
    if below_1_5 {
        doc.version = "1.5".to_string();
    }
}

/// Configuration options for PDF compression.
pub struct CompressOptions {
    pub aggressive: bool,
    pub max_pixels: u32,
    /// JPEG quality for lossy image encoding (1–100).
    pub image_quality: u8,
    /// JPEG pre-smoothing factor (1–100) to suppress scanner grain; 0 disables.
    pub smoothing: u8,
    /// If set, compute pixel threshold from this DPI value (assumes 8.5" page width).
    pub downsample_dpi: Option<u16>,
    /// Force all images to JPEG regardless of color count heuristic.
    pub force_jpeg: bool,
    /// Try bilevel CCITT G4 for text-only pages (opt-in; drops paper texture).
    pub bilevel: bool,
    pub strip_metadata: bool,
    pub dry_run: bool,
    pub force: bool,
    /// Re-deflate Flate streams with Zopfli (slow, a few % denser output).
    pub zopfli: bool,
    /// Maximum decoded size accepted for any individual PDF stream.
    pub max_decompressed_bytes: usize,
}

/// Compress a single PDF file with the given options.
/// Returns FileStats with original and compressed sizes.
pub fn compress_file(
    input_path: &str,
    output_path: &str,
    opts: &CompressOptions,
) -> Result<FileStats, String> {
    let original_bytes = fs::metadata(input_path)
        .map_err(|_| format!("Cannot read/write file: {}", input_path))?
        .len();

    if !opts.dry_run && !opts.force && std::path::Path::new(output_path).exists() {
        return Err(format!(
            "Output already exists: {} (use --force to overwrite)",
            output_path
        ));
    }

    let load_options = LoadOptions::with_max_decompressed_size(opts.max_decompressed_bytes);
    let mut doc = Document::load_with_options(input_path, load_options)
        .map_err(|e| format!("Could not parse PDF: {}", e))?;

    // Remove dead data before doing any expensive stream or image work.
    prune_dead_objects(&mut doc);
    if opts.strip_metadata {
        strip_metadata(&mut doc);
        prune_dead_objects(&mut doc);
    }

    // Lossless JPEG transcode (bit-identical rendering). In aggressive mode the
    // per-image pipeline already tries it as a fallback for every DCT stream.
    if !opts.aggressive {
        crate::image_opt::transcode_jpeg_streams(&mut doc);
    }

    // Lossy pass
    if opts.aggressive {
        let image_opts = ImageOptions {
            max_pixels: opts.max_pixels,
            image_quality: opts.image_quality,
            smoothing: opts.smoothing,
            downsample_dpi: opts.downsample_dpi,
            force_jpeg: opts.force_jpeg,
            bilevel: opts.bilevel,
            max_decompressed_bytes: opts.max_decompressed_bytes,
        };
        optimize_images(&mut doc, &image_opts, input_path);
    }

    // Recompress remaining general streams last so dead or replaced payloads are
    // never inflated and encoded unnecessarily.
    recompress_streams(&mut doc, opts.zopfli, opts.max_decompressed_bytes);

    // Save with modern serialization: object streams + cross-reference streams
    // shrink structural overhead and require PDF 1.5+.
    ensure_min_version_1_5(&mut doc);
    let save_opts = SaveOptions::builder()
        .use_object_streams(true)
        .use_xref_streams(true)
        .compression_level(9)
        .build();

    let compressed_bytes = if opts.dry_run {
        let mut tmp =
            NamedTempFile::new().map_err(|e| format!("Failed to create temp file: {}", e))?;
        doc.save_with_options(tmp.as_file_mut(), save_opts)
            .map_err(|e| format!("Failed to write output: {}", e))?;
        tmp.as_file()
            .metadata()
            .map(|m| m.len().min(original_bytes))
            .unwrap_or(original_bytes)
    } else {
        let output = std::path::Path::new(output_path);
        let output_dir = output
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        let mut tmp = NamedTempFile::new_in(output_dir)
            .map_err(|e| format!("Failed to create temporary output: {}", e))?;
        doc.save_with_options(tmp.as_file_mut(), save_opts)
            .map_err(|e| format!("Failed to write output: {}", e))?;
        tmp.as_file_mut()
            .flush()
            .map_err(|e| format!("Failed to flush output: {}", e))?;

        let generated_bytes = tmp
            .as_file()
            .metadata()
            .map_err(|e| format!("Failed to inspect output: {}", e))?
            .len();

        // Whole-document guard: structural rewriting can outweigh stream-level
        // savings. In that case the byte-identical input is the best candidate.
        let final_bytes = if generated_bytes >= original_bytes {
            let file = tmp.as_file_mut();
            file.set_len(0)
                .and_then(|_| file.seek(SeekFrom::Start(0)))
                .map_err(|e| format!("Failed to prepare fallback output: {}", e))?;
            let mut input =
                fs::File::open(input_path).map_err(|e| format!("Failed to reopen input: {}", e))?;
            std::io::copy(&mut input, file)
                .map_err(|e| format!("Failed to preserve smaller input: {}", e))?;
            original_bytes
        } else {
            generated_bytes
        };

        tmp.as_file_mut()
            .sync_all()
            .map_err(|e| format!("Failed to sync output: {}", e))?;
        tmp.persist(output)
            .map_err(|e| format!("Failed to install output atomically: {}", e.error))?;
        final_bytes
    };

    Ok(FileStats {
        original_bytes,
        compressed_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{Dictionary, Document, Object, Stream};

    const TEST_LIMIT: usize = 16 * 1024 * 1024;

    fn make_doc_with_uncompressed_stream() -> Document {
        let mut doc = Document::with_version("1.5");
        let mut dict = Dictionary::new();
        // Use a highly compressible payload (repeated pattern, 5000 bytes)
        // so flate2 will reliably produce a smaller output than the raw bytes.
        let payload = b"AAAAAAAAAA".repeat(500);
        dict.set("Length", Object::Integer(payload.len() as i64));
        let stream = Stream::new(dict, payload);
        doc.objects.insert((1, 0), Object::Stream(stream));
        doc
    }

    #[test]
    fn recompress_streams_compresses_uncompressed_stream() {
        let mut doc = make_doc_with_uncompressed_stream();
        recompress_streams(&mut doc, false, TEST_LIMIT);
        let obj = doc.objects.get(&(1, 0)).unwrap();
        if let Object::Stream(stream) = obj {
            let filter = stream.dict.get(b"Filter");
            assert!(
                filter.is_ok(),
                "stream should have a Filter after recompression"
            );
            let filter_name = filter.unwrap().as_name().unwrap();
            assert_eq!(filter_name, b"FlateDecode");
        } else {
            panic!("expected stream object");
        }
    }

    #[test]
    fn recompress_streams_skips_already_flate_encoded() {
        let mut doc = make_doc_with_uncompressed_stream();
        // First pass
        recompress_streams(&mut doc, false, TEST_LIMIT);
        // Capture compressed size
        let size_after_first = if let Object::Stream(s) = doc.objects.get(&(1, 0)).unwrap() {
            s.content.len()
        } else {
            0
        };
        // Second pass — should be idempotent
        recompress_streams(&mut doc, false, TEST_LIMIT);
        let size_after_second = if let Object::Stream(s) = doc.objects.get(&(1, 0)).unwrap() {
            s.content.len()
        } else {
            0
        };
        assert_eq!(size_after_first, size_after_second);
    }

    #[test]
    fn recompress_streams_skips_array_form_flate_encoded() {
        // PDFs often encode Filter as a single-element array [/FlateDecode]
        let mut doc = Document::with_version("1.5");
        let mut dict = Dictionary::new();
        let payload = b"AAAAAAAAAA".repeat(50);
        dict.set(
            "Filter",
            Object::Array(vec![Object::Name(b"FlateDecode".to_vec())]),
        );
        dict.set("Length", Object::Integer(payload.len() as i64));
        let stream = Stream::new(dict, payload.clone());
        doc.objects.insert((2, 0), Object::Stream(stream));
        let size_before = payload.len();
        recompress_streams(&mut doc, false, TEST_LIMIT);
        let size_after = if let Object::Stream(s) = doc.objects.get(&(2, 0)).unwrap() {
            s.content.len()
        } else {
            0
        };
        // Should be skipped — size unchanged
        assert_eq!(size_before, size_after);
    }

    #[test]
    fn recompress_streams_actually_reduces_size() {
        let mut doc = make_doc_with_uncompressed_stream();
        let original_size = 500; // payload is b"AAAAAAAAAA".repeat(50) = 500 bytes
        recompress_streams(&mut doc, false, TEST_LIMIT);
        let compressed_size = if let Object::Stream(s) = doc.objects.get(&(1, 0)).unwrap() {
            s.content.len()
        } else {
            panic!("expected stream")
        };
        assert!(
            compressed_size < original_size,
            "compressed should be smaller than raw"
        );
    }

    #[test]
    fn prune_dead_objects_removes_unreachable() {
        let mut doc = Document::with_version("1.5");
        // Create a minimal valid document structure
        let catalog_id = doc.add_object(Dictionary::new());
        doc.trailer.set("Root", Object::Reference(catalog_id));
        // Add an orphaned object (not referenced from root)
        let orphan_id = doc.add_object(Object::Integer(42));
        assert!(doc.objects.contains_key(&orphan_id));
        prune_dead_objects(&mut doc);
        assert!(
            !doc.objects.contains_key(&orphan_id),
            "orphan should be removed"
        );
        assert!(doc.objects.contains_key(&catalog_id), "root should be kept");
    }

    #[test]
    fn zopfli_redeflates_existing_flate_stream_and_preserves_dict() {
        let mut doc = make_doc_with_uncompressed_stream();
        // First: normal flate2 pass
        recompress_streams(&mut doc, false, TEST_LIMIT);
        let (flate_size, had_parms) = if let Object::Stream(s) = doc.objects.get(&(1, 0)).unwrap() {
            (s.content.len(), s.dict.get(b"DecodeParms").is_ok())
        } else {
            panic!("expected stream")
        };
        // Second: zopfli pass must shrink (or keep) the already-FlateDecode stream
        // without touching Filter/DecodeParms.
        recompress_streams(&mut doc, true, TEST_LIMIT);
        if let Object::Stream(s) = doc.objects.get(&(1, 0)).unwrap() {
            assert!(
                s.content.len() <= flate_size,
                "zopfli must never grow the stream"
            );
            assert_eq!(
                s.dict.get(b"Filter").unwrap().as_name().unwrap(),
                b"FlateDecode"
            );
            assert_eq!(s.dict.get(b"DecodeParms").is_ok(), had_parms);
            // Payload must still inflate to the original bytes
            use std::io::Read;
            let mut inflater = flate2::read::ZlibDecoder::new(&s.content[..]);
            let mut raw = Vec::new();
            inflater.read_to_end(&mut raw).unwrap();
            assert_eq!(raw, b"AAAAAAAAAA".repeat(500));
        } else {
            panic!("expected stream")
        }
    }

    #[test]
    fn zopfli_skips_streams_exceeding_decompression_limit() {
        let mut doc = make_doc_with_uncompressed_stream();
        recompress_streams(&mut doc, false, TEST_LIMIT);
        let before = match doc.objects.get(&(1, 0)).unwrap() {
            Object::Stream(stream) => stream.content.clone(),
            _ => panic!("expected stream"),
        };

        recompress_streams(&mut doc, true, 128);
        let after = match doc.objects.get(&(1, 0)).unwrap() {
            Object::Stream(stream) => &stream.content,
            _ => panic!("expected stream"),
        };
        assert_eq!(after, &before);
    }

    #[test]
    fn ensure_min_version_bumps_low_versions() {
        let mut doc = Document::with_version("1.4");
        ensure_min_version_1_5(&mut doc);
        assert_eq!(doc.version, "1.5");
    }

    #[test]
    fn ensure_min_version_keeps_higher_versions() {
        for v in ["1.5", "1.7", "2.0"] {
            let mut doc = Document::with_version(v);
            ensure_min_version_1_5(&mut doc);
            assert_eq!(doc.version, v);
        }
    }

    #[test]
    fn strip_metadata_removes_info_fields() {
        let mut doc = Document::with_version("1.5");
        let mut info = Dictionary::new();
        info.set("Author", Object::string_literal("Alice"));
        info.set("Title", Object::string_literal("My Doc"));
        let info_id = doc.add_object(info);
        doc.trailer.set("Info", Object::Reference(info_id));
        strip_metadata(&mut doc);
        // Info object and trailer reference should be fully removed
        assert!(
            !doc.objects.contains_key(&info_id),
            "Info object should be removed from doc"
        );
        assert!(
            doc.trailer.get(b"Info").is_err(),
            "Info key should be removed from trailer"
        );
    }
}
