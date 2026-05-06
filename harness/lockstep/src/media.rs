//! Shared media writers — PNG for framebuffers, WAV for PCM audio.
//!
//! Consolidated here so every producer in the workspace writes the same
//! on-disk formats (RGBA PNGs and 16-bit stereo WAVs) without per-binary
//! copies drifting in error handling and size caps.
//!
//! The `png` crate dependency moved here from the per-bin Cargo.toml
//! files — downstream crates get it transitively via lockstep.

use std::io;
use std::path::Path;

use crate::{GBA_H, GBA_PIXELS, GBA_W};

/// Max WAV file size the grader will write per replay. 16-bit stereo
/// at 32 kHz = ~128 KB/s of audio, so 20 MB = ~2.6 minutes. Any replay
/// longer than that is a grader config problem, not a real recording.
pub const WAV_SIZE_CAP: usize = 20 * 1024 * 1024;

/// Pack a GBA framebuffer (`0xAABBGGRR` little-endian u32 words — bytes
/// `[R, G, B, A]` in memory) into RGBA bytes. Alpha forced to 255 since
/// the GBA APU doesn't produce a meaningful alpha channel.
///
/// `out` must be `GBA_PIXELS * 4` bytes. Debug-asserted.
pub fn fb_to_rgba(fb: &[u32; GBA_PIXELS], out: &mut [u8]) {
    debug_assert_eq!(out.len(), GBA_PIXELS * 4);
    for (i, &px) in fb.iter().enumerate() {
        out[i * 4]     = (px & 0xFF) as u8;
        out[i * 4 + 1] = ((px >> 8) & 0xFF) as u8;
        out[i * 4 + 2] = ((px >> 16) & 0xFF) as u8;
        out[i * 4 + 3] = 255;
    }
}

/// Write a framebuffer to `path` as an RGBA PNG at native GBA resolution.
pub fn write_png(path: &Path, fb: &[u32; GBA_PIXELS]) -> io::Result<()> {
    let mut rgba = vec![0u8; GBA_PIXELS * 4];
    fb_to_rgba(fb, &mut rgba);
    let file = std::fs::File::create(path)?;
    let writer = std::io::BufWriter::new(file);
    let mut encoder = png::Encoder::new(writer, GBA_W as u32, GBA_H as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .and_then(|mut w| w.write_image_data(&rgba))
        .map_err(|e| io::Error::other(format!("png encode {}: {e}", path.display())))
}

/// Write an i16 stereo-interleaved PCM buffer to `path` as a canonical
/// 16-bit WAV. Empty `pcm` returns `Ok(())` without writing — callers
/// that specifically want a zero-byte placeholder should check
/// emptiness themselves.
pub fn write_wav(path: &Path, pcm: &[i16], rate: u32) -> io::Result<()> {
    write_wav_impl(path, pcm, rate, None)
}

/// Cap-aware WAV writer. If the encoded file would exceed `max_bytes`,
/// silently truncate the tail and log a warning to stderr. Lets the
/// grader bound on-disk artifact size.
pub fn write_wav_capped(
    path: &Path,
    pcm: &[i16],
    rate: u32,
    max_bytes: usize,
) -> io::Result<()> {
    write_wav_impl(path, pcm, rate, Some(max_bytes))
}

fn write_wav_impl(
    path: &Path,
    pcm: &[i16],
    rate: u32,
    max_bytes: Option<usize>,
) -> io::Result<()> {
    if pcm.is_empty() {
        return Ok(());
    }

    let pcm = if let Some(cap) = max_bytes {
        let max_i16 = cap.saturating_sub(44) / 2;
        if pcm.len() > max_i16 {
            eprintln!(
                "note: truncating {} from {} to {} samples (>{} byte cap)",
                path.display(),
                pcm.len(),
                max_i16,
                cap,
            );
            &pcm[..max_i16]
        } else {
            pcm
        }
    } else {
        pcm
    };

    let channels: u16 = 2;
    let byte_rate = rate * channels as u32 * 2;
    let block_align = channels * 2;
    let data_bytes = (pcm.len() * 2) as u32;

    let mut buf = Vec::with_capacity(44 + pcm.len() * 2);
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    buf.extend_from_slice(b"WAVE");
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes()); // PCM format
    buf.extend_from_slice(&channels.to_le_bytes());
    buf.extend_from_slice(&rate.to_le_bytes());
    buf.extend_from_slice(&byte_rate.to_le_bytes());
    buf.extend_from_slice(&block_align.to_le_bytes());
    buf.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_bytes.to_le_bytes());
    for &s in pcm {
        buf.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, &buf)
}
