//! Structural frame comparison.
//!
//! Per-frame score is `ssim_floored` — classical block SSIM (Wang et al.
//! 2004) on 10×10 non-overlapping blocks with a sub-JND perceptual floor,
//! computed on 8-bit-expanded luma after 5-bit quantization. The
//! quantization kills 5→8 channel-expansion differences (different
//! emulators pick `c<<3`, `(c<<3)|(c>>2)`, or `c*255/31`); the canonical
//! `(c<<3)|(c>>2)` expansion is applied before luma projection so SSIM's
//! published stabilization constants land on the scale they were
//! calibrated for.
//!
//! GMSD (Xue et al. 2014) is still available as `gmsd()` for
//! experimental use, but is not the scored metric.
//!
//! The metric targets structural/edge fidelity. It is intentionally less
//! sensitive to uniform global rendering drift (same edges, shifted luma)
//! — those cases show up in the `audit_luma_mae` diagnostic instead.

use crate::{GBA_H, GBA_PIXELS, GBA_W};

/// Published GMSD constant. Calibrated for 8-bit luma; we run on
/// `(c<<3)|(c>>2)`-expanded luma so T lands on its intended scale.
pub const GMSD_T: f32 = 170.0;

/// Canonical 5→8 expansion used after quantization. Matches the GBA's
/// own "double the top bits into the bottom bits" rule that real
/// hardware and most reference docs assume.
#[inline]
fn expand5(c5: u8) -> u8 {
    (c5 << 3) | (c5 >> 2)
}

/// 5-bit-quantize then 8-bit-expand each channel, then project to BT.601
/// luma. Returns a 240×160 row-major luma buffer in the range [0, 255].
///
/// The quantize-then-expand pass is what gives us invariance to
/// emulator-specific 5→8 expansion choices: any two emulators that agree
/// on the GBA's 15-bit output will produce identical luma here.
fn framebuffer_to_luma(fb: &[u32; GBA_PIXELS]) -> Vec<f32> {
    let mut out = vec![0.0f32; GBA_PIXELS];
    for (i, &px) in fb.iter().enumerate() {
        let r8 = (px & 0xFF) as u8;
        let g8 = ((px >> 8) & 0xFF) as u8;
        let b8 = ((px >> 16) & 0xFF) as u8;
        // Quant5 then canonical-expand. `c8 >> 3` is the 5-bit value.
        let r = expand5(r8 >> 3) as f32;
        let g = expand5(g8 >> 3) as f32;
        let b = expand5(b8 >> 3) as f32;
        // BT.601 luma — matches how Mesen's own luma debug output
        // projects 5-bit RGB to grayscale.
        out[i] = 0.299 * r + 0.587 * g + 0.114 * b;
    }
    out
}

/// Prewitt 3×3 gradient magnitude on a luma buffer. Boundary is
/// replicated (standard `conv2(_, _, 'same')` semantics from the
/// reference Matlab implementation). Returns a 240×160 gradient-
/// magnitude buffer.
///
/// Prewitt (not Sobel) is what the GMSD paper uses. The kernels:
///
/// ```text
///     hx = (1/3)·[  1  0 -1  ;  1  0 -1  ;  1  0 -1 ]
///     hy = (1/3)·[  1  1  1  ;  0  0  0  ; -1 -1 -1 ]
/// ```
///
/// The `/3` keeps gradient values on the same scale as the source luma,
/// so the published `T=170` constant applies without rescaling.
fn gradient_magnitude(luma: &[f32]) -> Vec<f32> {
    debug_assert_eq!(luma.len(), GBA_PIXELS);
    let w = GBA_W as isize;
    let h = GBA_H as isize;
    let at = |x: isize, y: isize| -> f32 {
        let xc = x.clamp(0, w - 1) as usize;
        let yc = y.clamp(0, h - 1) as usize;
        luma[yc * GBA_W + xc]
    };
    let mut out = vec![0.0f32; GBA_PIXELS];
    for y in 0..h {
        for x in 0..w {
            // horizontal: sum over the three rows of (right − left).
            let gx = (at(x + 1, y - 1) - at(x - 1, y - 1)
                + at(x + 1, y) - at(x - 1, y)
                + at(x + 1, y + 1) - at(x - 1, y + 1))
                / 3.0;
            // vertical: sum over the three columns of (bottom − top).
            let gy = (at(x - 1, y + 1) - at(x - 1, y - 1)
                + at(x, y + 1) - at(x, y - 1)
                + at(x + 1, y + 1) - at(x + 1, y - 1))
                / 3.0;
            out[(y as usize) * GBA_W + (x as usize)] = (gx * gx + gy * gy).sqrt();
        }
    }
    out
}

/// Gradient Magnitude Similarity Deviation between two framebuffers.
///
/// Returns 0.0 for bit-identical (after 5-bit quantization) frames, up
/// to ~0.3+ for structurally different scenes. The value is the
/// standard deviation of the per-pixel GMS map — a uniformly-damaged
/// frame still has low deviation; a frame with *locally* concentrated
/// damage (missing sprite, corrupted HUD) produces a high deviation
/// because the GMS map is split between "matching" and "broken"
/// regions. This is why the deviation-pooled form is more sensitive to
/// salient local defects than a mean-pooled gradient similarity would
/// be — see the GMSD paper's Figure 4.
pub fn gmsd(ref_fb: &[u32; GBA_PIXELS], cand_fb: &[u32; GBA_PIXELS]) -> f32 {
    let ref_luma = framebuffer_to_luma(ref_fb);
    let cand_luma = framebuffer_to_luma(cand_fb);
    let gr = gradient_magnitude(&ref_luma);
    let gc = gradient_magnitude(&cand_luma);

    let n = GBA_PIXELS as f32;
    let mut gms = vec![0.0f32; GBA_PIXELS];
    let mut mean = 0.0f32;
    for i in 0..GBA_PIXELS {
        let num = 2.0 * gr[i] * gc[i] + GMSD_T;
        let den = gr[i] * gr[i] + gc[i] * gc[i] + GMSD_T;
        let v = num / den;
        gms[i] = v;
        mean += v;
    }
    mean /= n;
    let mut var = 0.0f32;
    for &v in &gms {
        let d = v - mean;
        var += d * d;
    }
    // Per the paper, GMSD is sqrt(variance) of the GMS map. 1 − mean(GMS)
    // would be the GMSM (gradient magnitude similarity mean) form, which
    // the paper explicitly argues is less sensitive than the deviation.
    (var / n).sqrt()
}

/// Per-block perceptual floor for `ssim_floored`. Block defects
/// below this are treated as sub-JND and contribute zero.
pub const SSIM_FLOORED_PERCEPTUAL_FLOOR: f32 = 0.15;

/// Block size for `ssim_floored`. The 240×160 frame splits into
/// 24×16 = 384 non-overlapping 10×10 blocks.
const SSIM_FLOORED_BLOCK: usize = 10;

/// Classical L·C·S block SSIM with a sub-JND floor, averaged over
/// all non-overlapping 10×10 blocks. Inputs are the quant5 + BT.601
/// luma produced by `framebuffer_to_luma`, shared with the legacy
/// GMSD path so both metrics speak the same pixel units.
///
/// Aggregation: per block, compute 1 − SSIM_block; contribute it if
/// ≥ PERCEPTUAL_FLOOR, else zero. Mean over blocks. Result is a
/// structural defect in [0, 1].
pub fn ssim_floored(ref_fb: &[u32; GBA_PIXELS], cand_fb: &[u32; GBA_PIXELS]) -> f32 {
    let r = framebuffer_to_luma(ref_fb);
    let c = framebuffer_to_luma(cand_fb);
    let w = GBA_W;
    let h = GBA_H;
    let block = SSIM_FLOORED_BLOCK;
    // Wang et al. 2004 constants on an 8-bit luma range. Same as
    // the reference SSIM paper; K1=0.01, K2=0.03.
    let c1 = (0.01 * 255.0f32).powi(2);
    let c2 = (0.03 * 255.0f32).powi(2);
    let mut sum_defect = 0.0f32;
    let mut n_total = 0u32;
    let mut y = 0;
    while y + block <= h {
        let mut x = 0;
        while x + block <= w {
            let mut s_r = 0.0f32;
            let mut s_c = 0.0f32;
            let mut ss_r = 0.0f32;
            let mut ss_c = 0.0f32;
            let mut sxy = 0.0f32;
            let n = (block * block) as f32;
            for dy in 0..block {
                for dx in 0..block {
                    let i = (y + dy) * w + (x + dx);
                    let rv = r[i];
                    let cv = c[i];
                    s_r += rv;
                    s_c += cv;
                    ss_r += rv * rv;
                    ss_c += cv * cv;
                    sxy += rv * cv;
                }
            }
            let mu_r = s_r / n;
            let mu_c = s_c / n;
            let var_r = (ss_r / n - mu_r * mu_r).max(0.0);
            let var_c = (ss_c / n - mu_c * mu_c).max(0.0);
            let cov = sxy / n - mu_r * mu_c;
            let l = (2.0 * mu_r * mu_c + c1) / (mu_r * mu_r + mu_c * mu_c + c1);
            let cs = (2.0 * cov + c2) / (var_r + var_c + c2);
            let block_ssim = (l * cs).clamp(0.0, 1.0);
            let block_defect = 1.0 - block_ssim;
            n_total += 1;
            if block_defect >= SSIM_FLOORED_PERCEPTUAL_FLOOR {
                sum_defect += block_defect;
            }
            x += block;
        }
        y += block;
    }
    if n_total == 0 {
        return 0.0;
    }
    (sum_defect / n_total as f32).clamp(0.0, 1.0)
}

/// Mean per-pixel luma absolute error. Audit-only diagnostic: a
/// candidate that gets edge structure right but applies a small
/// systematic luma shift can still score high on `ssim_floored` (the
/// CS term dominates within each block) while producing a non-zero
/// MAE here, and that's the signal we want to see separately from the
/// main score.
///
/// Runs on the same quant-and-expand luma as `ssim_floored`, so both
/// diagnostics speak the same units.
pub fn luma_mae(ref_fb: &[u32; GBA_PIXELS], cand_fb: &[u32; GBA_PIXELS]) -> f32 {
    let ref_luma = framebuffer_to_luma(ref_fb);
    let cand_luma = framebuffer_to_luma(cand_fb);
    let mut acc = 0.0f32;
    for i in 0..GBA_PIXELS {
        acc += (ref_luma[i] - cand_luma[i]).abs();
    }
    acc / GBA_PIXELS as f32
}

/// True iff the two framebuffers differ at ALL after 5-bit quantization.
/// Used as the ref-motion gate: "static" ref transitions are excluded
/// from the τ estimator because a p90 driven by mostly-identical frames
/// collapses τ to the floor even when the motion-frame population is
/// perfectly well-defined. Any quant5-visible change counts as motion.
///
/// Simpler than comparing `ssim_floored() > η` and has no free
/// threshold — a transition that's invisible after 5-bit quantization
/// is, by construction, not a transition worth calibrating on.
pub fn ref_in_motion(prev: &[u32; GBA_PIXELS], curr: &[u32; GBA_PIXELS]) -> bool {
    for i in 0..GBA_PIXELS {
        if crate::quant5(prev[i]) != crate::quant5(curr[i]) {
            return true;
        }
    }
    false
}

// ─────────────────────────────────────────────────────────────────────────
// τ calibration
// ─────────────────────────────────────────────────────────────────────────

/// Lower bound on τ. Frame-level defect below this is treated as
/// "within reference noise" regardless of how static the replay is —
/// catches candidates with 5-bit-quant-surviving luma drift on
/// near-static replays where p90(ref-ref defect) collapses to ~0.
///
pub const TAU_MIN: f32 = 0.005;

/// Upper bound on τ. High-motion replays should not buy unbounded
/// forgiveness — a black-screen candidate on a rhythm game with heavy
/// motion has a legitimate structural defect even if ref-to-ref
/// defect runs high. Caps the "the reference moves a lot, so the
/// candidate gets forgiven a lot" compensation.
///
pub const TAU_MAX: f32 = 0.35;

/// Percentile of the motion-gated ref-to-ref `ssim_floored` distribution
/// used as the adaptive τ. 90 matches the pixel-diff pipeline's
/// percentile so the "exclude top 10% scene cuts" rationale carries over.
pub const TAU_PERCENTILE: f32 = 0.90;

/// Shape parameter of the per-frame sigmoid. Kept at 4 from the
/// pixel-diff era — frame_score(0.5τ) ≈ 0.94, frame_score(τ) = 0.5,
/// frame_score(2τ) ≈ 0.06, so the "fraction of frames close enough"
/// reading carries over unchanged.
pub const SHARPNESS: i32 = 4;

/// Tight τ for end-state scoring (verdict-screen ROMs). Deliberately
/// set well below the noise a structural metric can produce on any
/// visible text/digit difference — at 1e-4 with SHARPNESS=4, an
/// `ssim_floored` defect of even ~0.001 (a single wrong glyph pixel)
/// gives D/τ ≈ 10 and collapses the score to effectively zero. A
/// bit-exact verdict framebuffer still scores 1 (D=0). This is the
/// "essentially exact pixel match" rule — anything short of
/// bit-identical on a PASS/FAIL grid is a fail.
pub const ENDSTATE_TAU: f32 = 1.0e-4;

/// Clamp the adaptive τ from a motion-gated ref-to-ref `ssim_floored`
/// series. Equivalent to `delta_threshold_clamped` in the old pixel-
/// diff world, but in structural-defect units. `defects` is mutated
/// (sorted).
pub fn defect_threshold_clamped(defects: &mut [f32]) -> f32 {
    if defects.is_empty() {
        return TAU_MIN;
    }
    defects.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((defects.len() as f32) * TAU_PERCENTILE) as usize;
    let idx = idx.min(defects.len() - 1);
    defects[idx].clamp(TAU_MIN, TAU_MAX)
}
