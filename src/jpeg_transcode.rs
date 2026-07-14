//! Lossless JPEG transcode in the DCT-coefficient domain.
//!
//! Reads the quantized coefficients of an existing JPEG and rewrites the bitstream
//! with optimized Huffman tables, dropping metadata markers (EXIF/APPn). Pixels are
//! never decoded, so the output renders bit-identically — no quality gate needed.
//! Scanner-produced JPEGs with default Annex-K Huffman tables typically shrink 5–12%.
//!
//! This also works on JPEGs the pixel pipeline cannot decode (CMYK, exotic
//! colorspaces): coefficients and critical parameters are copied verbatim.

use mozjpeg_sys::{
    jpeg_common_struct, jpeg_compress_struct, jpeg_copy_critical_parameters, jpeg_create_compress,
    jpeg_create_decompress, jpeg_decompress_struct, jpeg_destroy_compress, jpeg_destroy_decompress,
    jpeg_error_mgr, jpeg_finish_compress, jpeg_finish_decompress, jpeg_mem_dest, jpeg_mem_src,
    jpeg_read_coefficients, jpeg_read_header, jpeg_std_error, jpeg_write_coefficients,
};
use std::mem;
use std::os::raw::c_ulong;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;

/// libjpeg's default error_exit calls exit(); replace it with a panic that
/// catch_unwind in `transcode_lossless` converts to None. The extern ABI is
/// "C-unwind", so unwinding across the C frames is defined behavior.
unsafe extern "C-unwind" fn error_exit_panic(_cinfo: &mut jpeg_common_struct) {
    panic!("libjpeg fatal error during lossless transcode");
}

/// Losslessly re-encode `data` (a complete JPEG bitstream) with optimized Huffman
/// tables and stripped metadata. Returns None on any parse/encode failure.
///
/// The caller decides whether the result is worth shipping (size guard).
pub fn transcode_lossless(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return None; // not a JPEG (SOI missing)
    }
    catch_unwind(AssertUnwindSafe(|| unsafe { transcode_inner(data) })).unwrap_or(None)
}

unsafe fn transcode_inner(data: &[u8]) -> Option<Vec<u8>> {
    // Source: read coefficients without pixel decode
    let mut src_err: jpeg_error_mgr = mem::zeroed();
    let mut src: jpeg_decompress_struct = mem::zeroed();
    // jpeg_std_error fills in ALL handlers (including error_exit), so ours must be
    // installed afterwards — write through the returned reference so the compiler
    // sees the store that C will later read via the err pointer.
    let src_err = jpeg_std_error(&mut src_err);
    src_err.error_exit = Some(error_exit_panic);
    src.common.err = src_err;
    jpeg_create_decompress(&mut src);

    jpeg_mem_src(&mut src, data.as_ptr(), data.len() as c_ulong);
    if jpeg_read_header(&mut src, 1) != 1 {
        jpeg_destroy_decompress(&mut src);
        return None;
    }
    let coefficients = jpeg_read_coefficients(&mut src);
    if coefficients.is_null() {
        jpeg_destroy_decompress(&mut src);
        return None;
    }

    // Destination: same critical parameters, optimized entropy coding.
    // Markers (EXIF/APPn/COM) are intentionally not copied.
    let mut dst_err: jpeg_error_mgr = mem::zeroed();
    let mut dst: jpeg_compress_struct = mem::zeroed();
    let dst_err = jpeg_std_error(&mut dst_err);
    dst_err.error_exit = Some(error_exit_panic);
    dst.common.err = dst_err;
    jpeg_create_compress(&mut dst);

    let mut out_buf: *mut u8 = ptr::null_mut();
    let mut out_size: c_ulong = 0;
    jpeg_mem_dest(&mut dst, &mut out_buf, &mut out_size);
    jpeg_copy_critical_parameters(&src, &mut dst);
    dst.optimize_coding = 1;
    jpeg_write_coefficients(&mut dst, coefficients);
    jpeg_finish_compress(&mut dst);
    jpeg_destroy_compress(&mut dst);
    jpeg_finish_decompress(&mut src);
    jpeg_destroy_decompress(&mut src);

    if out_buf.is_null() || out_size == 0 {
        return None;
    }
    let out = std::slice::from_raw_parts(out_buf, out_size as usize).to_vec();
    libc::free(out_buf.cast());
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageFormat;
    use std::io::Cursor;

    fn noisy_test_image(size: u32) -> image::DynamicImage {
        let mut img = image::RgbImage::new(size, size);
        for (x, y, p) in img.enumerate_pixels_mut() {
            let v = ((x * 31 + y * 17) % 256) as u8;
            *p = image::Rgb([v, v.wrapping_add(40), v.wrapping_mul(3)]);
        }
        image::DynamicImage::ImageRgb8(img)
    }

    /// Encode with the `image` crate's JPEG encoder — it does NOT optimize Huffman
    /// tables, so the transcode must find real savings.
    fn unoptimized_jpeg(size: u32) -> Vec<u8> {
        let img = noisy_test_image(size);
        let mut buf = Vec::new();
        img.write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(
            &mut Cursor::new(&mut buf),
            80,
        ))
        .unwrap();
        buf
    }

    #[test]
    fn transcode_shrinks_unoptimized_jpeg() {
        let original = unoptimized_jpeg(256);
        let transcoded = transcode_lossless(&original).expect("transcode must succeed");
        assert!(
            transcoded.len() < original.len(),
            "optimized Huffman must shrink an unoptimized JPEG: {} -> {}",
            original.len(),
            transcoded.len()
        );
    }

    #[test]
    fn transcode_is_pixel_lossless() {
        let original = unoptimized_jpeg(128);
        let transcoded = transcode_lossless(&original).unwrap();
        let before = image::load_from_memory_with_format(&original, ImageFormat::Jpeg)
            .unwrap()
            .to_rgb8();
        let after = image::load_from_memory_with_format(&transcoded, ImageFormat::Jpeg)
            .unwrap()
            .to_rgb8();
        assert_eq!(
            before.as_raw(),
            after.as_raw(),
            "coefficient-domain transcode must be bit-identical after decode"
        );
    }

    #[test]
    fn transcode_rejects_garbage() {
        assert!(transcode_lossless(b"not a jpeg").is_none());
        assert!(transcode_lossless(&[]).is_none());
        // Truncated JPEG: SOI present, then garbage
        assert!(transcode_lossless(&[0xFF, 0xD8, 0x00, 0x01, 0x02]).is_none());
    }
}
