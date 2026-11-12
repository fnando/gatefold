//! Lossless image optimization, applied to every image gatefold writes (and,
//! with `gatefold optimize`, to the ones already in a library).
//!
//! - JPEG: the compressed DCT coefficients are re-encoded as they are (no
//!   decoding to pixels, so nothing about the picture can change) with
//!   optimized Huffman tables and a searched progressive scan script, via
//!   mozjpeg — what `jpegtran -optimize -progressive` does. Exif (orientation)
//!   and ICC (colour) markers are kept; comments, XMP and Photoshop blocks go.
//!   The result is read back and its coefficients and quantization tables
//!   compared with the original's: any difference keeps the original.
//! - PNG: oxipng, keeping every colour-related chunk and leaving fully
//!   transparent pixels untouched.
//!
//! Anything else, anything that fails to parse, and any result that isn't
//! smaller comes back as `None`: the caller keeps the bytes it has.

use mozjpeg_sys::{
    JPEG_LIB_VERSION, boolean, jpeg_common_struct, jpeg_compress_struct,
    jpeg_copy_critical_parameters, jpeg_decompress_struct, jpeg_destroy_compress,
    jpeg_destroy_decompress, jpeg_error_mgr, jpeg_finish_compress, jpeg_finish_decompress,
    jpeg_mem_dest, jpeg_mem_src, jpeg_read_coefficients, jpeg_read_header, jpeg_save_markers,
    jpeg_simple_progression, jpeg_std_error, jpeg_write_coefficients, jpeg_write_marker,
    jvirt_barray_control,
};
use sha2::{Digest, Sha256};
use std::ffi::{CStr, c_int, c_long, c_uint, c_ulong, c_void};
use std::mem;
use std::panic::{self, AssertUnwindSafe};
use std::ptr;

const JPEG_SOI: &[u8] = &[0xFF, 0xD8, 0xFF];
const PNG_SIGNATURE: &[u8] = &[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1A, b'\n'];
/// APP1 and APP2, where Exif and ICC profiles live.
const APP1: c_int = 0xE1;
const APP2: c_int = 0xE2;
/// oxipng's default preset: a fraction of the higher presets' time for
/// nearly all of their gain on artwork.
const PNG_PRESET: u8 = 2;

unsafe extern "C" {
    /// `jpeg_mem_dest` allocates its output with `malloc`.
    fn free(ptr: *mut c_void);
}

/// The kind of image `data` holds, by its signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Jpeg,
    Png,
}

pub fn format(data: &[u8]) -> Option<Format> {
    if data.starts_with(JPEG_SOI) {
        Some(Format::Jpeg)
    } else if data.starts_with(PNG_SIGNATURE) {
        Some(Format::Png)
    } else {
        None
    }
}

/// `data` optimized losslessly, or `None` when it can't be made smaller (or
/// isn't a JPEG or PNG gatefold can read).
pub fn image(data: &[u8]) -> Option<Vec<u8>> {
    let optimized = match format(data)? {
        Format::Jpeg => jpeg(data)?,
        Format::Png => png(data)?,
    };
    (optimized.len() < data.len()).then_some(optimized)
}

fn png(data: &[u8]) -> Option<Vec<u8>> {
    let mut options = oxipng::Options::from_preset(PNG_PRESET);
    options.strip = oxipng::StripChunks::Safe;
    options.optimize_alpha = false;
    oxipng::optimize_from_memory(data, &options).ok()
}

fn jpeg(data: &[u8]) -> Option<Vec<u8>> {
    let optimized = transcode(data).ok()?;
    // Belt and braces: the transcode never decodes pixels, but only a result
    // carrying the very same coefficients is lossless by definition.
    (fingerprint(data).ok()? == fingerprint(&optimized).ok()?).then_some(optimized)
}

/// A libjpeg error: the message libjpeg formatted.
#[derive(Debug)]
struct JpegError(String);

/// libjpeg's default `error_exit` calls `exit()`; unwind back to Rust instead
/// (mozjpeg-sys builds libjpeg with `-fexceptions` for this).
unsafe extern "C-unwind" fn error_exit(cinfo: &mut jpeg_common_struct) {
    let buffer = [0u8; 80];
    // SAFETY: `err` is the error manager installed by `ErrorManager::install`.
    if let Some(format) = unsafe { (*cinfo.err).format_message } {
        unsafe { format(cinfo, &buffer) };
    }
    let message = CStr::from_bytes_until_nul(&buffer).map_or_else(
        |_| "invalid JPEG".to_string(),
        |m| m.to_string_lossy().into_owned(),
    );
    panic::resume_unwind(Box::new(JpegError(message)));
}

/// Warnings (corrupt data libjpeg worked around) are counted, not printed.
unsafe extern "C-unwind" fn quiet(_: &mut jpeg_common_struct) {}

/// An error manager that unwinds on errors and counts warnings.
fn error_manager() -> Box<jpeg_error_mgr> {
    // SAFETY: `jpeg_error_mgr` is a plain C struct; `jpeg_std_error` fills it in.
    let mut err: Box<jpeg_error_mgr> = Box::new(unsafe { mem::zeroed() });
    unsafe { jpeg_std_error(&mut err) };
    err.error_exit = Some(error_exit);
    err.output_message = Some(quiet);
    err
}

/// Run `f`, turning a libjpeg error (or any panic) into an `Err`.
fn guarded<T>(f: impl FnOnce() -> T) -> Result<T, String> {
    panic::catch_unwind(AssertUnwindSafe(f)).map_err(|payload| {
        payload
            .downcast_ref::<JpegError>()
            .map_or_else(|| "JPEG library panicked".to_string(), |e| e.0.clone())
    })
}

/// A decompressor reading `data`, with Exif and ICC markers saved.
struct Reader<'a> {
    info: Box<jpeg_decompress_struct>,
    _err: Box<jpeg_error_mgr>,
    _data: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Result<Self, String> {
        let len = c_ulong::try_from(data.len()).map_err(|_| "JPEG too large".to_string())?;
        let mut err = error_manager();
        // SAFETY: zeroed then initialized by jpeg_CreateDecompress, as libjpeg expects.
        let mut info: Box<jpeg_decompress_struct> = Box::new(unsafe { mem::zeroed() });
        info.common.err = &raw mut *err;
        guarded(|| unsafe {
            mozjpeg_sys::jpeg_CreateDecompress(
                &raw mut *info,
                JPEG_LIB_VERSION,
                mem::size_of::<jpeg_decompress_struct>(),
            );
        })?;
        let mut reader = Reader {
            info,
            _err: err,
            _data: data,
        };
        guarded(|| unsafe {
            jpeg_mem_src(&mut reader.info, data.as_ptr(), len);
            jpeg_save_markers(&mut reader.info, APP1, 0xFFFF);
            jpeg_save_markers(&mut reader.info, APP2, 0xFFFF);
            jpeg_read_header(&mut reader.info, 1);
        })?;
        Ok(reader)
    }

    /// The image's coefficient arrays, one per component.
    fn coefficients(&mut self) -> Result<*mut *mut jvirt_barray_control, String> {
        guarded(|| unsafe { jpeg_read_coefficients(&mut self.info) })
    }

    fn warnings(&self) -> c_long {
        // SAFETY: `err` points at `_err`, alive as long as `self`.
        unsafe { (*self.info.common.err).num_warnings }
    }
}

impl Drop for Reader<'_> {
    fn drop(&mut self) {
        // SAFETY: created in `new`; destroying is valid in any state.
        unsafe { jpeg_destroy_decompress(&mut self.info) };
    }
}

/// Exif (orientation) and ICC (colour) are worth their bytes; the rest isn't.
fn keep_marker(marker: c_int, data: &[u8]) -> bool {
    (marker == APP1 && data.starts_with(b"Exif\0\0"))
        || (marker == APP2 && data.starts_with(b"ICC_PROFILE\0"))
}

/// Re-encode the coefficients of `data` with optimized Huffman tables and a
/// progressive scan script. Any libjpeg warning (damaged input) is an error.
fn transcode(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut reader = Reader::new(data)?;
    let coefficients = reader.coefficients()?;

    let mut err = error_manager();
    // SAFETY: zeroed then initialized by jpeg_CreateCompress, as libjpeg expects.
    let mut info: Box<jpeg_compress_struct> = Box::new(unsafe { mem::zeroed() });
    info.common.err = &raw mut *err;
    guarded(|| unsafe {
        mozjpeg_sys::jpeg_CreateCompress(
            &raw mut *info,
            JPEG_LIB_VERSION,
            mem::size_of::<jpeg_compress_struct>(),
        );
    })?;

    let mut out: *mut u8 = ptr::null_mut();
    let mut out_len: c_ulong = 0;
    let written = guarded(|| unsafe {
        jpeg_copy_critical_parameters(&reader.info, &mut info);
        // mozjpeg's default profile makes this search for the smallest scans.
        jpeg_simple_progression(&mut info);
        info.optimize_coding = 1 as boolean;
        jpeg_mem_dest(&mut info, &raw mut out, &raw mut out_len);
        jpeg_write_coefficients(&mut info, coefficients);
        let mut marker = reader.info.marker_list;
        while !marker.is_null() {
            let m = &*marker;
            let bytes = std::slice::from_raw_parts(m.data, m.data_length as usize);
            if keep_marker(c_int::from(m.marker), bytes) {
                jpeg_write_marker(&mut info, c_int::from(m.marker), m.data, m.data_length);
            }
            marker = m.next;
        }
        jpeg_finish_compress(&mut info);
        jpeg_finish_decompress(&mut reader.info);
    });
    let warnings = reader.warnings() + err.num_warnings;

    let result = if out.is_null() {
        Vec::new()
    } else {
        // SAFETY: libjpeg wrote `out_len` bytes at `out`.
        let len = usize::try_from(out_len).unwrap_or(0);
        unsafe { std::slice::from_raw_parts(out, len) }.to_vec()
    };
    // SAFETY: created above; `out` came from malloc (or is null).
    unsafe {
        jpeg_destroy_compress(&mut info);
        free(out.cast());
    }
    written?;
    if warnings > 0 {
        return Err(format!("{warnings} warning(s) reading the JPEG"));
    }
    Ok(result)
}

/// SHA-256 over everything that determines a JPEG's pixels: per component,
/// its sampling, size in blocks, quantization table and every coefficient.
fn fingerprint(data: &[u8]) -> Result<[u8; 32], String> {
    let mut reader = Reader::new(data)?;
    let arrays = reader.coefficients()?;
    let mut hash = Sha256::new();
    let info = &mut reader.info;
    let components = usize::try_from(info.num_components).unwrap_or(0);
    hash.update(info.image_width.to_le_bytes());
    hash.update(info.image_height.to_le_bytes());
    hash.update((info.jpeg_color_space as c_uint).to_le_bytes());
    guarded(|| unsafe {
        let access = (*info.common.mem)
            .access_virt_barray
            .expect("libjpeg memory manager");
        for index in 0..components {
            let component = &*info.comp_info.add(index);
            for value in [
                component.h_samp_factor,
                component.v_samp_factor,
                component.component_id,
            ] {
                hash.update(value.to_le_bytes());
            }
            hash.update(component.width_in_blocks.to_le_bytes());
            hash.update(component.height_in_blocks.to_le_bytes());
            if let Some(table) = component.quant_table.as_ref() {
                for q in table.quantval {
                    hash.update(q.to_le_bytes());
                }
            }
            let array = *arrays.add(index);
            let width = component.width_in_blocks as usize;
            for row in 0..component.height_in_blocks {
                let rows = access(&mut info.common, array, row, 1, 0);
                let blocks = std::slice::from_raw_parts(*rows, width);
                for block in blocks {
                    for coefficient in block {
                        hash.update(coefficient.to_le_bytes());
                    }
                }
            }
        }
    })?;
    Ok(hash.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 64×64 baseline JPEG (4:2:0, with an Exif block and a comment), as
    /// most encoders write it: standard Huffman tables, not progressive.
    fn sample_jpeg() -> Vec<u8> {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sample.jpg");
        std::fs::read(path).expect("fixture")
    }

    #[test]
    fn detects_formats() {
        assert_eq!(format(&sample_jpeg()), Some(Format::Jpeg));
        assert_eq!(format(PNG_SIGNATURE), Some(Format::Png));
        assert_eq!(format(b"GIF89a"), None);
        assert_eq!(image(b"not an image"), None);
    }

    #[test]
    fn jpegs_shrink_without_changing_coefficients() {
        let original = sample_jpeg();
        let optimized = image(&original).expect("smaller");
        assert!(optimized.len() < original.len());
        assert_eq!(fingerprint(&original), fingerprint(&optimized));
        // Already optimal: nothing left to gain, so nothing to write.
        assert_eq!(image(&optimized), None);
    }

    #[test]
    fn jpegs_keep_exif_and_drop_comments() {
        let optimized = image(&sample_jpeg()).unwrap();
        let has = |needle: &[u8]| optimized.windows(needle.len()).any(|w| w == needle);
        assert!(has(b"Exif\0\0"), "Exif kept");
        assert!(!has(b"gatefold test comment"), "comment dropped");
    }

    #[test]
    fn broken_jpegs_are_left_alone() {
        let mut truncated = sample_jpeg();
        truncated.truncate(truncated.len() / 2);
        assert_eq!(image(&truncated), None);
        assert_eq!(image(&[0xFF, 0xD8, 0xFF, 0x00]), None);
    }

    #[test]
    fn pngs_shrink_losslessly() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sample.png");
        let original = std::fs::read(path).expect("fixture");
        let optimized = image(&original).expect("smaller");
        assert!(optimized.len() < original.len());
        assert_eq!(format(&optimized), Some(Format::Png));
    }
}
