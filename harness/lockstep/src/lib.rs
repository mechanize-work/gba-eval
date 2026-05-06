//! Lockstep frame comparison against a reference emulator.
//!
//! The grader drives this. It instantiates a reference and a candidate
//! (both wasm modules implementing the ABI, hosted in wasmtime), feeds
//! both the same ROM and the same frame-indexed input log, and diffs
//! framebuffers in 5-bit color space every frame.
//!
//! Everything is generic over the `Reference` trait. Adding another
//! reference is one more impl with no changes to `lockstep()`.

pub mod audio;
pub mod media;
pub mod replay;
pub mod result;
pub mod video;
pub mod video_encode;

#[cfg(mesen_available)]
pub mod mesen;

pub use replay::InputReplay;
pub use result::{CompareResult, FrameDiff};
pub use audio::{envelope_correlation, derive_threshold as derive_audio_threshold, score_logmel};
pub use video::{
    defect_threshold_clamped, gmsd, luma_mae, ref_in_motion,
    ENDSTATE_TAU, GMSD_T, SHARPNESS, TAU_MAX, TAU_MIN, TAU_PERCENTILE,
};

pub const GBA_W: usize = 240;
pub const GBA_H: usize = 160;
pub const GBA_PIXELS: usize = GBA_W * GBA_H;

// ─────────────────────────────────────────────────────────────────────────
// Reference trait — what every comparison target implements.
// ─────────────────────────────────────────────────────────────────────────

/// An emulator we can drive frame-by-frame.
///
/// Both sides of a comparison implement this — the reference and the
/// candidate. `lockstep()` doesn't care which is which.
pub trait Reference {
    fn name(&self) -> &str;

    /// Advance one frame. After this returns, `framebuffer()` holds the
    /// frame just rendered (scanlines 0–159 of the frame whose VCOUNT
    /// just wrapped to 0).
    fn run_frame(&mut self);

    /// Active-high GBA KEYINPUT layout. Bit 0=A, 1=B, 2=Select, 3=Start,
    /// 4=Right, 5=Left, 6=Up, 7=Down, 8=R, 9=L. Latched until next call.
    fn set_keys(&mut self, keys: u16);

    /// 240×160 pixels, 0xAABBGGRR. Alpha may be garbage — `quant5` masks
    /// it. Pointer is stable across frames; the same buffer gets
    /// overwritten each `run_frame()`.
    fn framebuffer(&self) -> &[u32; GBA_PIXELS];

    /// Drain audio produced by the last `run_frame()`. Returns interleaved
    /// i16 stereo pairs (L, R, L, R, ...). Typical: ~548 pairs at 32 kHz.
    ///
    /// Calling this resets the write head — the next `run_frame()` writes
    /// from offset 0. Backends that don't support audio return empty.
    fn drain_audio(&mut self) -> Vec<i16> {
        Vec::new()
    }

    /// Sample rate of `drain_audio()` output in Hz. Usually 32768 but
    /// SOUNDBIAS resolution can push it to 65536 (Pokemon) or higher.
    fn audio_rate(&self) -> u32 {
        32768
    }

    /// Frames between init and "the framebuffer holds game frame 0".
    ///
    /// The ABI says `load_rom()` should leave you AT frame 0, so for
    /// conformant candidates this is 0. But the reference doesn't
    /// implement the ABI directly — Mesen's `skip_bios` jumps to ROM
    /// entry but the PPU's first Draw() fires at vcount=0 of the NEXT
    /// frame, so its f0 framebuffer is still init-zeros. Hence 1.
    ///
    /// `lockstep()` burns each side's boot frames before comparing, so
    /// "frame N" means the same thing on both.
    fn boot_frames(&self) -> u32 {
        0
    }
}

/// Forward to the boxed impl. Lets `lockstep()`'s `&mut impl Reference`
/// signature accept a `Box<dyn Reference>` — the grader doesn't need
/// to know the concrete type at all.
///
/// Every defaulted trait method MUST be forwarded here — the trait
/// default would silently shadow the boxed impl's override otherwise.
impl Reference for Box<dyn Reference> {
    fn name(&self) -> &str { (**self).name() }
    fn run_frame(&mut self) { (**self).run_frame() }
    fn set_keys(&mut self, keys: u16) { (**self).set_keys(keys) }
    fn framebuffer(&self) -> &[u32; GBA_PIXELS] { (**self).framebuffer() }
    fn drain_audio(&mut self) -> Vec<i16> { (**self).drain_audio() }
    fn audio_rate(&self) -> u32 { (**self).audio_rate() }
    fn boot_frames(&self) -> u32 { (**self).boot_frames() }
}

// ─────────────────────────────────────────────────────────────────────────
// Reference selection
// ─────────────────────────────────────────────────────────────────────────

/// Which reference to compare against. Single-variant today; left as
/// an enum so future references can be added without rewriting callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RefKind {
    #[default]
    Mesen,
}

impl RefKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RefKind::Mesen => "mesen",
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Pixel comparison
// ─────────────────────────────────────────────────────────────────────────

/// Quantize 0x??BBGGRR → 0x00B5G5R5 (5 bits per channel, alpha masked).
///
/// The GBA outputs 5-bit color. Emulators expand 5→8 differently —
/// `(c << 3)`, `(c << 3) | (c >> 2)`, `c * 255 / 31`. Quantizing back
/// to 5 bits compares what the GBA actually computed, not the expansion.
#[inline]
pub fn quant5(px: u32) -> u32 {
    (px >> 3) & 0x001F_1F1F
}

/// Per-frame pixel diff in 5-bit space.
pub fn diff_frame(a: &[u32; GBA_PIXELS], b: &[u32; GBA_PIXELS]) -> usize {
    let mut count = 0;
    for i in 0..GBA_PIXELS {
        if quant5(a[i]) != quant5(b[i]) {
            count += 1;
        }
    }
    count
}

/// Per-frame diff with first-mismatch coordinates. Slower; for diagnostics.
pub fn diff_frame_detail(
    frame: u32,
    a: &[u32; GBA_PIXELS],
    b: &[u32; GBA_PIXELS],
) -> FrameDiff {
    let mut count = 0;
    let mut first = None;
    for i in 0..GBA_PIXELS {
        if quant5(a[i]) != quant5(b[i]) {
            if first.is_none() {
                first = Some((i % GBA_W, i / GBA_W, a[i], b[i]));
            }
            count += 1;
        }
    }
    FrameDiff { frame, differing_pixels: count, first_diff: first }
}

// ─────────────────────────────────────────────────────────────────────────
// Lockstep — the comparison kernel
// ─────────────────────────────────────────────────────────────────────────

/// A frame "diverges" if it differs by more than this many pixels.
/// Not zero — sprite-edge races and similar can produce a handful of
/// pixel-level differences even on correct emulators. Real bugs
/// produce hundreds.
pub const NOISE_FLOOR: usize = 8;

/// Drive both emulators in lockstep, applying the same inputs each frame.
///
/// Each side burns its `boot_frames()` first to align "frame 0" to the
/// same game state. After that, frame N means the same thing on both.
///
/// `inputs` is indexed by post-boot frame number.
/// RMS threshold below which a frame is considered silent. ~-50 dB in i16
/// space — well below the GBA APU's noise floor.
const SILENCE_RMS: f64 = 100.0;

pub fn lockstep(
    reference: &mut impl Reference,
    candidate: &mut impl Reference,
    n_frames: u32,
    inputs: &InputReplay,
    mut video: Option<&mut video_encode::VideoEncoder>,
) -> LockstepOutput {
    // Drain boot-period audio from both sides so frame 0 starts with
    // an empty buffer on both. Skipping this leaks boot samples into
    // frame 0's first drain and shifts the log-mel analysis grid for
    // the rest of the replay.
    for _ in 0..reference.boot_frames() {
        reference.run_frame();
        let _ = reference.drain_audio();
    }
    for _ in 0..candidate.boot_frames() {
        candidate.run_frame();
        let _ = candidate.drain_audio();
    }

    let mut r = CompareResult::new(n_frames);
    let audio_rate = reference.audio_rate();
    let mut ref_audio: Vec<i16> = Vec::new();
    let mut cand_audio: Vec<i16> = Vec::new();

    // Track consecutive-reference `ssim_floored` defects on frames
    // where the ref actually changes (in 5-bit-quantized space). The
    // motion gate keeps long idle stalls from collapsing the p90
    // estimator to zero.
    let mut prev_ref_fb: [u32; GBA_PIXELS] = [0; GBA_PIXELS];
    let mut ref_defects: Vec<f32> = Vec::with_capacity(n_frames as usize);
    // Per-frame "is this a new run" flags, for the run-collapsed audit
    // score. Frame 0 always starts a new run; later frames start a new
    // run iff the reference changed (in 5-bit-quantized space) since
    // the prior frame.
    let mut new_run: Vec<bool> = Vec::with_capacity(n_frames as usize);

    for frame in 0..n_frames {
        let keys = inputs.keys_at(frame);
        reference.set_keys(keys);
        candidate.set_keys(keys);
        reference.run_frame();
        candidate.run_frame();

        // ── Video ──
        let ref_fb = reference.framebuffer();
        let cand_fb = candidate.framebuffer();

        // Mirror frames into the encoder before the scoring math — the
        // video is a visual record of exactly what was scored. A write
        // failure (ffmpeg died, disk full) drops video for the rest of
        // the testcase but shouldn't abort scoring: log once and null
        // the encoder so later frames short-circuit cheaply.
        if let Some(enc) = video.as_deref_mut() {
            if let Err(e) = enc.push(ref_fb, cand_fb) {
                eprintln!("warning: video encoder failed on frame {frame}: {e}");
                video = None;
            }
        }

        // Structural defect: floored-SSIM on 10×10 blocks of quant-and-
        // expand luma. Zero when ref/cand agree in 5-bit space; grows
        // as edges/gradients diverge. Global luma drift with preserved
        // edges is captured separately as `audit_luma_mae`, not scored.
        let mut defect = video::ssim_floored(ref_fb, cand_fb);
        // Blank-frame gate: when the candidate is flat (≥99.9% one
        // colour) but the reference isn't, force the defect to its
        // max so the sigmoid can't forgive a stuck/black candidate
        // via a permissive τ.
        if is_flat_frame(cand_fb) && !is_flat_frame(ref_fb) {
            defect = 1.0;
        }
        r.histogram.push(defect);

        // Luma MAE — audit only, not scored in v1. Written per-frame so
        // the final replay-level mean is a simple average downstream.
        r.audit_luma_mae_frames.push(video::luma_mae(ref_fb, cand_fb));

        // Pixel-count diagnostics. Independent of the scored metric;
        // for human-readable reports where pixel counts are legible.
        let raw_diff = diff_frame(ref_fb, cand_fb);
        if raw_diff > NOISE_FLOOR {
            r.diverging_frames += 1;
            r.total_diff_pixels += raw_diff as u64;
            if r.first_diverge_frame.is_none() {
                r.first_diverge_frame = Some(frame);
            }
            if raw_diff > r.max_diff_pixels {
                r.max_diff_pixels = raw_diff;
                r.max_diff_frame = Some(frame);
            }
        }

        let in_motion = frame == 0 || video::ref_in_motion(&prev_ref_fb, ref_fb);
        new_run.push(in_motion);
        if frame > 0 && in_motion {
            ref_defects.push(video::ssim_floored(&prev_ref_fb, ref_fb));
        }
        prev_ref_fb.copy_from_slice(ref_fb);

        // ── Audio ──
        let ra = reference.drain_audio();
        let ca = candidate.drain_audio();

        let ref_rms = audio::rms_stereo_left(&ra);
        let cand_rms = audio::rms_stereo_left(&ca);
        r.audio_rms_ratio.push(if ref_rms > SILENCE_RMS {
            Some((cand_rms / ref_rms) as f32)
        } else {
            None
        });

        ref_audio.extend_from_slice(&ra);
        cand_audio.extend_from_slice(&ca);
    }

    // Audio scoring: per-frame log-mel L1 → sigmoid with τ = p90 of
    // the reference's own adjacent-frame diffs (silent pairs excluded).
    // Mirrors the video scorer — see `audio::score_logmel_detailed`.
    r.audio_diff_threshold = audio::derive_threshold(&ref_audio, audio_rate) as f32;
    if let Some(s) = audio::score_logmel_detailed(
        &ref_audio,
        &cand_audio,
        audio_rate,
        r.audio_diff_threshold as f64,
    ) {
        r.audio_score = Some(s.mean);
        r.audio_frame_scores = s.per_frame.into_iter()
            .map(|opt| opt.map(|v| v as f32))
            .collect();
    }
    r.frame_diff_threshold = video::defect_threshold_clamped(&mut ref_defects);

    // Run-collapsed replay score — diagnostic only. Groups consecutive
    // same-state reference frames into runs, averages frame scores
    // inside each run, then averages across runs.
    let tau = r.frame_diff_threshold;
    let mut run_sum = 0.0f64;
    let mut run_len = 0u32;
    let mut run_count = 0u32;
    let mut deduped = 0.0f64;
    for (i, &defect) in r.histogram.iter().enumerate() {
        if new_run[i] && run_len > 0 {
            deduped += run_sum / run_len as f64;
            run_count += 1;
            run_sum = 0.0;
            run_len = 0;
        }
        run_sum += CompareResult::frame_score(defect, tau);
        run_len += 1;
    }
    if run_len > 0 {
        deduped += run_sum / run_len as f64;
        run_count += 1;
    }
    r.replay_score_deduped = if run_count > 0 {
        (deduped / run_count as f64) as f32
    } else {
        1.0
    };

    LockstepOutput { result: r, ref_audio, cand_audio, audio_rate }
}

/// Minimum fraction of pixels matching the first pixel for a frame
/// to count as "flat" (candidate stuck on a single colour).
/// 99.9% leaves room for small overlays on near-solid backgrounds.
pub const FLAT_FRAME_THRESHOLD: f64 = 0.999;

/// True when ≥ `FLAT_FRAME_THRESHOLD` of the frame's pixels match its
/// first pixel.
pub fn is_flat_frame(fb: &[u32; GBA_PIXELS]) -> bool {
    let first = fb[0];
    let matching = fb.iter().filter(|&&p| p == first).count();
    matching as f64 / GBA_PIXELS as f64 >= FLAT_FRAME_THRESHOLD
}

/// Everything `lockstep()` produces. `CompareResult` serializes to JSON
/// for downstream playback; the audio buffers are written as separate WAV files.
pub struct LockstepOutput {
    pub result: CompareResult,
    pub ref_audio: Vec<i16>,
    pub cand_audio: Vec<i16>,
    pub audio_rate: u32,
}
