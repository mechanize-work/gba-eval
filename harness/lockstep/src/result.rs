//! Comparison results — what `lockstep()` produces.
//!
//! `CompareResult` serializes to per-testcase JSON. `histogram` and
//! `frame_diff_threshold` are in structural-defect units (`ssim_floored`
//! on 5-bit-quantized, 8-bit-expanded luma); the raw-pixel diagnostics
//! (`diverging_frames`, `max_diff_pixels`, `first_diverge_frame`) are in
//! pixel units because "N pixels different" reads more legibly than a
//! structural-defect float.

use serde::{Deserialize, Serialize};

use crate::GBA_PIXELS;
use crate::video::{ENDSTATE_TAU, SHARPNESS, TAU_MIN};

/// Serde default for backwards-compatible deserialization of older
/// result JSON files.
fn default_threshold() -> f32 {
    TAU_MIN
}

/// Per-frame summary with first-mismatch detail. Diagnostic — the grader
/// uses the cheaper `diff_frame()`; this is for `eprintln!`-style probing.
#[derive(Debug, Clone)]
pub struct FrameDiff {
    pub frame: u32,
    pub differing_pixels: usize,
    /// (x, y, ref_pixel, cand_pixel) at the first mismatch, raw 0xAABBGGRR.
    pub first_diff: Option<(usize, usize, u32, u32)>,
}

/// One ROM, one replay, one candidate vs the reference.
///
/// Serializes as `results/<candidate>/<testcase>.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompareResult {
    pub n_frames: u32,

    // ── Video ──────────────────────────────────────────────────────────

    /// Frames where raw 5-bit pixel diff > NOISE_FLOOR. Independent of
    /// the scored metric — a diagnostic for "N/T frames diverged" reporting.
    pub diverging_frames: u32,

    /// Sum of differing pixels across diverging frames only.
    pub total_diff_pixels: u64,

    /// First frame past the pixel-diff noise floor. The "jump to
    /// divergence" button. Pixel-count based so the seek lands on a
    /// frame a human would see as obviously different; a frame with
    /// tiny structural defect but lots of subpixel-scattered differing
    /// pixels is a worse scrub target than the structural metric alone
    /// would suggest.
    pub first_diverge_frame: Option<u32>,

    /// Worst single frame by pixel count, and which one.
    pub max_diff_pixels: usize,
    pub max_diff_frame: Option<u32>,

    /// Per-frame structural defect (`ssim_floored` on 5-bit-quantized,
    /// 8-bit-expanded luma). Length = n_frames. Values in [0, 1]; a
    /// perfectly tracking candidate scores 0 everywhere, a black-screen
    /// candidate on a non-blank reference scores 1 via the blank-frame
    /// gate.
    ///
    /// Feeds the per-frame sigmoid in `frame_score`.
    pub histogram: Vec<f32>,

    /// Per-replay "close-enough" threshold — 90th percentile of the
    /// reference's motion-gated consecutive-frame `ssim_floored`
    /// defects, clamped to `[TAU_MIN, TAU_MAX]`.
    ///
    /// The threshold is a property of the replay, not the candidate.
    /// The percentile measures "how much structural change the reference
    /// carries frame-to-frame during active gameplay"; the floor keeps
    /// near-static replays from demanding bit-exact output from
    /// candidates with sub-perceptual structural drift; the ceiling
    /// keeps pathologically high-motion replays from forgiving whole-
    /// scene candidate errors.
    ///
    /// Read by `frame_score()` as the shape parameter of the per-frame
    /// sigmoid. Set by `lockstep()` after the comparison loop.
    #[serde(default = "default_threshold")]
    pub frame_diff_threshold: f32,

    // ── Audit diagnostics (reported, not scored) ──────────────────────

    /// Per-frame mean-absolute luma error, in 8-bit units. Catches
    /// small uniform rendering drift that preserves edges — block SSIM's
    /// luminance term dampens but does not fully eliminate this class
    /// of defect, so we report it separately. Written per-frame so the
    /// replay-level mean is a plain average downstream. Omitted from
    /// JSON when empty (older cache layouts / non-lockstep producers).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audit_luma_mae_frames: Vec<f32>,

    /// Run-collapsed replay score. Runs are defined by the reference's
    /// motion gate (contiguous frames with no quant5-visible change
    /// collapse to a single run). Diagnostic only — the scored metric
    /// uses the plain frame mean in `video_score()`. When precompute
    /// writes the refcache it leaves this at 0.0; the grader fills it.
    #[serde(default)]
    pub replay_score_deduped: f32,

    // ── Audio ──────────────────────────────────────────────────────────

    /// Continuous 0.0–1.0 audio score against the reference. Computed as
    /// the mean over active frames of a sigmoid of per-frame log-mel L1
    /// distance, with the sigmoid's threshold τ set to the 90th
    /// percentile of the reference's own adjacent-frame diff distribution
    /// (silent pairs excluded). See `audio::score_logmel` for details.
    /// `None` when the reference has no non-silent audio (nothing to
    /// compare — not a failure).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_score: Option<f64>,

    /// Per-replay audio threshold τ — the shape parameter of the audio
    /// sigmoid, analogous to `frame_diff_threshold` on the video side.
    /// In log-mel-L1 units. 0.0 when the reference is all-silent (no
    /// meaningful distribution to derive from).
    #[serde(default)]
    pub audio_diff_threshold: f32,

    /// Per-frame audio RMS delta (candidate/reference ratio, 1.0 = same
    /// volume). `None` for silent frames. Length = n_frames. Omitted
    /// from JSON when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audio_rms_ratio: Vec<Option<f32>>,

    /// Per-frame audio score series from `score_logmel_detailed`. `None`
    /// for silent-both frames (excluded from the mean). The mean of the
    /// `Some(...)` entries equals `audio_score`.
    ///
    /// Length is the reference log-mel spectrogram's analysis-frame
    /// count — roughly `n_frames` but not exactly (STFT windowing trims
    /// a fraction of a frame at the tail). Omitted from JSON when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audio_frame_scores: Vec<Option<f32>>,

    // ── Video file output ─────────────────────────────────────────────

    /// True when `grade_testcase` wrote `<tc>.{ref,cand,diff}.mp4`
    /// alongside this JSON. Omitted from JSON when false so old result
    /// files deserialize unchanged.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub has_video: bool,
}

impl CompareResult {
    pub fn new(n_frames: u32) -> Self {
        Self {
            n_frames,
            diverging_frames: 0,
            total_diff_pixels: 0,
            first_diverge_frame: None,
            max_diff_pixels: 0,
            max_diff_frame: None,
            histogram: Vec::with_capacity(n_frames as usize),
            audit_luma_mae_frames: Vec::with_capacity(n_frames as usize),
            replay_score_deduped: 0.0,
            audio_score: None,
            audio_diff_threshold: 0.0,
            audio_rms_ratio: Vec::with_capacity(n_frames as usize),
            audio_frame_scores: Vec::new(),
            frame_diff_threshold: TAU_MIN,
            has_video: false,
        }
    }

    /// Per-frame score: `1 / (1 + (defect / τ)^SHARPNESS)`. `τ` is the
    /// per-replay threshold (see `defect_threshold_clamped`).
    pub fn frame_score(defect: f32, threshold: f32) -> f64 {
        let t = threshold.max(f32::MIN_POSITIVE) as f64;
        let r = defect as f64 / t;
        1.0 / (1.0 + r.powi(SHARPNESS))
    }

    /// Mean per-frame score across the histogram.
    pub fn video_score(&self) -> f64 {
        if self.histogram.is_empty() {
            return 1.0;
        }
        let t = self.frame_diff_threshold;
        let sum: f64 = self
            .histogram
            .iter()
            .map(|&d| Self::frame_score(d, t))
            .sum();
        sum / self.histogram.len() as f64
    }

    /// Score of only the final frame, against `ENDSTATE_TAU`. For
    /// self-checking ROMs whose verdict is the final framebuffer
    /// (armwrestler, mgba-suite). The tight τ collapses the score on
    /// any visible text/digit difference.
    pub fn endstate_score(&self) -> f64 {
        match self.histogram.last() {
            None => 1.0,
            Some(&d) => Self::frame_score(d, ENDSTATE_TAU),
        }
    }

    /// Average differing pixels per *diverging* frame. "When it's wrong,
    /// how wrong?" — distinguishes 1000 frames each off by 10px (subtle
    /// timing) from 1000 frames each off by 30000px (something exploded).
    pub fn avg_diff_when_diverging(&self) -> f64 {
        if self.diverging_frames == 0 {
            return 0.0;
        }
        self.total_diff_pixels as f64 / self.diverging_frames as f64
    }

    /// Mean per-frame luma MAE. Audit only; not scored. Zero when the
    /// histogram of luma MAE values is empty (old cache or non-lockstep
    /// producers).
    pub fn audit_luma_mae_mean(&self) -> f32 {
        if self.audit_luma_mae_frames.is_empty() {
            return 0.0;
        }
        let sum: f32 = self.audit_luma_mae_frames.iter().sum();
        sum / self.audit_luma_mae_frames.len() as f32
    }

    /// Terminal pretty-print.
    pub fn report(&self, ref_name: &str, cand_name: &str) {
        let mut s = String::new();
        self.report_to(ref_name, cand_name, &mut s);
        eprint!("{s}");
    }

    /// Same output as [`report`] but appended to `out` rather than
    /// printed to stderr. Used by the parallel grader so per-testcase
    /// log lines can be flushed in deterministic order after all
    /// workers finish.
    pub fn report_to(&self, ref_name: &str, cand_name: &str, out: &mut String) {
        use std::fmt::Write as _;
        let _ = writeln!(out, "┌─ {ref_name} vs {cand_name} ─────────────────────────");
        let _ = writeln!(out, "│ video: {:.4}  audio: {}",
                         self.video_score(),
                         self.audio_score.map_or("—".into(), |a| format!("{a:.4}")));
        match self.first_diverge_frame {
            None => {
                let _ = writeln!(out, "│ ✓ all {} frames within pixel noise floor",
                                 self.n_frames);
            }
            Some(first) => {
                let _ = writeln!(out, "│ ✗ {}/{} frames diverge (first: {first})",
                                 self.diverging_frames, self.n_frames);
                let avg = self.avg_diff_when_diverging();
                let _ = writeln!(out, "│   avg {avg:.0} px/frame ({:.2}% of screen)",
                                 100.0 * avg / GBA_PIXELS as f64);
                if let Some(f) = self.max_diff_frame {
                    let _ = writeln!(out, "│   worst: frame {f} ({} px = {:.1}%)",
                                     self.max_diff_pixels,
                                     100.0 * self.max_diff_pixels as f64 / GBA_PIXELS as f64);
                }
            }
        }
        let _ = writeln!(out, "└──────────────────────────────────────────────────");
    }
}
