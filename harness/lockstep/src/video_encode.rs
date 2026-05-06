//! Per-testcase video output via ffmpeg.
//!
//! Spawns three ffmpeg child processes — ref, cand, diff — and pipes
//! per-frame RGBA into each stdin. Output is all-intra H.264: every
//! frame is an I-frame, so consumers can seek to an exact frame with
//! `currentTime = frame / GBA_FPS`. Files are `<base>.ref.mp4`,
//! `<base>.cand.mp4`, `<base>.diff.mp4`.
//!
//! The diff stream bakes a 5-bit-space per-pixel magnitude through a
//! viridis-ish gradient at write time so consumers can play three
//! independent `<video>` elements without doing any pixel math.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};

use crate::media::fb_to_rgba;
use crate::{GBA_H, GBA_PIXELS, GBA_W};

/// GBA native frame rate. Same value used downstream for playback.
pub const GBA_FPS: f64 = 59.7275;

/// Drives three ffmpeg processes for one testcase's ref/cand/diff videos.
pub struct VideoEncoder {
    ref_proc: FfmpegProc,
    cand_proc: FfmpegProc,
    diff_proc: FfmpegProc,
    /// Scratch RGBA buffer for a ref or cand frame. Reused every push.
    fb_rgba: Vec<u8>,
    /// Scratch RGBA buffer for the rendered diff frame.
    diff_rgba: Vec<u8>,
}

impl VideoEncoder {
    /// Create encoders writing `<base>.ref.mp4`, `<base>.cand.mp4`,
    /// `<base>.diff.mp4`. `base` is the testcase stem (without
    /// extension) — same pattern as the PNG/WAV siblings.
    pub fn new(base: &Path) -> io::Result<Self> {
        Ok(Self {
            ref_proc: FfmpegProc::spawn(&with_suffix(base, "ref.mp4"))?,
            cand_proc: FfmpegProc::spawn(&with_suffix(base, "cand.mp4"))?,
            diff_proc: FfmpegProc::spawn(&with_suffix(base, "diff.mp4"))?,
            fb_rgba: vec![0u8; GBA_PIXELS * 4],
            diff_rgba: vec![0u8; GBA_PIXELS * 4],
        })
    }

    /// Push one frame. The diff frame is rendered here from the same two
    /// framebuffers — callers never compute it themselves.
    pub fn push(
        &mut self,
        ref_fb: &[u32; GBA_PIXELS],
        cand_fb: &[u32; GBA_PIXELS],
    ) -> io::Result<()> {
        fb_to_rgba(ref_fb, &mut self.fb_rgba);
        self.ref_proc.write(&self.fb_rgba)?;
        fb_to_rgba(cand_fb, &mut self.fb_rgba);
        self.cand_proc.write(&self.fb_rgba)?;
        render_diff(ref_fb, cand_fb, &mut self.diff_rgba);
        self.diff_proc.write(&self.diff_rgba)?;
        Ok(())
    }

    /// Close stdins and wait for all three ffmpeg processes. MUST be
    /// called — on Drop without finish() the child processes keep running
    /// until they see EOF, and the mp4 files may not be fully flushed.
    pub fn finish(self) -> io::Result<()> {
        // Close all stdins first so all three encoders flush in parallel
        // before we start waiting sequentially. Otherwise a long ref
        // encode blocks us from closing cand's stdin at all.
        let VideoEncoder { mut ref_proc, mut cand_proc, mut diff_proc, .. } = self;
        ref_proc.close_stdin();
        cand_proc.close_stdin();
        diff_proc.close_stdin();
        ref_proc.wait()?;
        cand_proc.wait()?;
        diff_proc.wait()?;
        Ok(())
    }
}

// ─── Per-process wrapper ─────────────────────────────────────────────

struct FfmpegProc {
    child: Child,
    stdin: Option<ChildStdin>,
    path: PathBuf,
}

impl FfmpegProc {
    fn spawn(output: &Path) -> io::Result<Self> {
        let size = format!("{}x{}", GBA_W, GBA_H);
        let fps = format!("{GBA_FPS}");
        let mut child = Command::new("ffmpeg")
            .args([
                "-y", "-hide_banner", "-loglevel", "error",
                // Input: raw RGBA frames on stdin at GBA_FPS.
                "-f", "rawvideo",
                "-pix_fmt", "rgba",
                "-s", &size,
                "-framerate", &fps,
                "-i", "-",
                // Encoder: libx264, yuv420p for broad browser support.
                // All-intra (keyint=1, sc_threshold=0) → every frame is
                // a keyframe → frame-exact seek via currentTime. `-crf 20`
                // is visually lossless for pixel-art at this resolution;
                // `-preset medium` balances encode speed and size.
                "-c:v", "libx264",
                "-preset", "medium",
                "-crf", "20",
                "-pix_fmt", "yuv420p",
                "-g", "1", "-keyint_min", "1", "-sc_threshold", "0",
                "-movflags", "+faststart",
            ])
            .arg(output)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| io::Error::new(
                e.kind(),
                format!("spawn ffmpeg for {}: {e} (is ffmpeg on PATH?)", output.display()),
            ))?;
        let stdin = child.stdin.take();
        Ok(Self { child, stdin, path: output.to_path_buf() })
    }

    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        match self.stdin.as_mut() {
            Some(s) => s.write_all(bytes).map_err(|e| io::Error::new(
                e.kind(),
                format!("writing frame to ffmpeg ({}): {e}", self.path.display()),
            )),
            None => Err(io::Error::other(format!(
                "ffmpeg stdin closed before finish ({})", self.path.display()
            ))),
        }
    }

    fn close_stdin(&mut self) {
        // Dropping ChildStdin closes the write end — ffmpeg sees EOF and
        // wraps up the file.
        drop(self.stdin.take());
    }

    fn wait(&mut self) -> io::Result<()> {
        let status = self.child.wait()?;
        if status.success() {
            return Ok(());
        }
        let mut stderr = String::new();
        if let Some(mut s) = self.child.stderr.take() {
            let _ = s.read_to_string(&mut stderr);
        }
        Err(io::Error::other(format!(
            "ffmpeg exited {status} for {}: {}",
            self.path.display(),
            stderr.trim(),
        )))
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────

/// Append a dotted suffix. `foo/tc_id` + `ref.mp4` → `foo/tc_id.ref.mp4`.
/// Keeps the testcase stem intact so the PNG/WAV/MP4 siblings share a
/// prefix in directory listings.
fn with_suffix(base: &Path, suffix: &str) -> PathBuf {
    let mut s = base.as_os_str().to_owned();
    s.push(".");
    s.push(suffix);
    s.into()
}

/// Render one diff frame. Per-pixel sum of 5-bit-space channel
/// differences mapped through a 5-stop viridis-ish gradient. Magnitude
/// 0 renders as solid black (not transparent — the diff panel stands
/// alone, no underlay).
fn render_diff(
    a: &[u32; GBA_PIXELS],
    b: &[u32; GBA_PIXELS],
    out: &mut [u8],
) {
    debug_assert_eq!(out.len(), GBA_PIXELS * 4);
    for i in 0..GBA_PIXELS {
        let pa = a[i];
        let pb = b[i];
        let dr = ((pa & 0xFF) >> 3) as i32 - ((pb & 0xFF) >> 3) as i32;
        let dg = (((pa >> 8) & 0xFF) >> 3) as i32 - (((pb >> 8) & 0xFF) >> 3) as i32;
        let db = (((pa >> 16) & 0xFF) >> 3) as i32 - (((pb >> 16) & 0xFF) >> 3) as i32;
        let m = dr.unsigned_abs() + dg.unsigned_abs() + db.unsigned_abs();
        let (r, g, bl) = if m == 0 {
            (0u8, 0u8, 0u8)
        } else {
            // Saturate past 30 — matches downstream playback. Interesting range
            // (subtle bugs, 1–30) gets the full gradient resolution.
            let t = (m.min(30) as f32) / 30.0;
            viridis(t)
        };
        out[i * 4]     = r;
        out[i * 4 + 1] = g;
        out[i * 4 + 2] = bl;
        out[i * 4 + 3] = 255;
    }
}

/// 5-stop gradient: dark purple → blue → teal → green → yellow. Not
/// real viridis (would need a 256-entry LUT) but indistinguishable at
/// this resolution.
fn viridis(t: f32) -> (u8, u8, u8) {
    let (r, g, b) = if t < 0.25 {
        let u = t * 4.0;
        (lerp(68.0, 59.0, u), lerp(1.0, 82.0, u), lerp(84.0, 139.0, u))
    } else if t < 0.5 {
        let u = (t - 0.25) * 4.0;
        (lerp(59.0, 33.0, u), lerp(82.0, 145.0, u), lerp(139.0, 140.0, u))
    } else if t < 0.75 {
        let u = (t - 0.5) * 4.0;
        (lerp(33.0, 94.0, u), lerp(145.0, 201.0, u), lerp(140.0, 98.0, u))
    } else {
        let u = (t - 0.75) * 4.0;
        (lerp(94.0, 253.0, u), lerp(201.0, 231.0, u), lerp(98.0, 37.0, u))
    };
    (r as u8, g as u8, b as u8)
}

#[inline]
fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}
