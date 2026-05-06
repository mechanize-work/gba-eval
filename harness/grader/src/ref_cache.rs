//! Reference-frame cache: precompute Mesen's per-frame output once per
//! corpus version, reuse across every candidate grade.
//!
//! The grader compares framebuffers pixel-by-pixel with the candidate in
//! lockstep. Mesen's output for a (rom, replay) pair is deterministic —
//! there's no reason to re-run the ~35,000 frames of replay content on
//! every candidate grade. We capture Mesen's framebuffers + audio once
//! into `corpus/reference-cache/<testcase>.refcache` (zstd-compressed
//! bincode), hash-keyed on the inputs so any change auto-invalidates.
//!
//! # Layout
//!
//! ```text
//! corpus/reference-cache/
//!   celeste-gameplay.refcache    # git-LFS tracked
//!   heartwrench-gameplay.refcache
//!   ...
//! ```
//!
//! # Invariants
//!
//! - `frames_flat.len() == frame_count * GBA_BYTES`
//! - `audio_samples_per_frame.len() == frame_count`
//! - `audio_flat.len() == 2 * audio_samples_per_frame.iter().sum::<u32>()`
//!   (stereo i16 pairs, so 2 values per sample)
//!
//! # Hash-based invalidation
//!
//! Every cache file records the sha256 of the reference wasm, the ROM,
//! and the replay file. On load the consumer verifies all three match;
//! any mismatch falls back to live Mesen execution so results stay
//! correct when the corpus or Mesen change.

use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use lockstep::{
    defect_threshold_clamped, derive_audio_threshold, video, Reference, GBA_PIXELS,
};

/// `240 * 160 * 4` — one RGBA framebuffer.
pub const GBA_BYTES: usize = GBA_PIXELS * 4;

/// Bumped when the cache layout changes; mismatch triggers regenerate.
const CACHE_VERSION: u32 = 7;

#[derive(Serialize, Deserialize)]
struct ReferenceCache {
    version: u32,
    ref_wasm_sha256: String,
    rom_sha256: String,
    replay_sha256: String,
    frame_count: u32,
    audio_rate: u32,
    /// Boot frames the live reference would have burned. Stored so the
    /// cache can replay identically without re-deriving.
    boot_frames: u32,
    /// Concatenated framebuffers — `frame_count × GBA_BYTES` bytes.
    frames_flat: Vec<u8>,
    /// Per-frame stereo-pair counts (i.e. interleaved samples / 2).
    audio_samples_per_frame: Vec<u32>,
    /// Concatenated audio — `2 × sum(audio_samples_per_frame)` i16 values.
    audio_flat: Vec<i16>,
    /// Motion-gated 90th percentile of consecutive-reference-frame
    /// GMSDs, clamped to `[TAU_MIN, TAU_MAX]`. The scoring threshold
    /// for this replay — see `CompareResult::frame_score`. Baked in at
    /// precompute time.
    frame_diff_threshold: f32,
    /// τ for the audio sigmoid — 90th percentile of the reference's
    /// adjacent-frame log-mel L1 diffs, silent pairs excluded. See
    /// `lockstep::audio::derive_threshold`. 0.0 for all-silent replays.
    #[serde(default)]
    audio_diff_threshold: f32,
}

/// Compute the sha256 hex digest of the given bytes.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// Hash a file's contents. Returns `None` if the file can't be read.
pub fn sha256_file(path: &Path) -> Option<String> {
    std::fs::read(path).ok().map(|b| sha256_hex(&b))
}

/// Path where the cache for a given testcase lives inside `corpus/`.
pub fn cache_path_for(corpus_dir: &Path, testcase_id: &str) -> PathBuf {
    corpus_dir
        .join("reference-cache")
        .join(format!("{testcase_id}.refcache"))
}

/// Ensure the cache at `path` is current for `(ref_wasm_sha256,
/// rom_sha256, replay_sha256)`. If so, no-op. Otherwise runs
/// `make_reference` to obtain a fresh Mesen Reference, plays the
/// replay through it, and writes the cache.
///
/// `make_reference` is only invoked on a cache miss — the whole
/// point of the precomputed cache is to avoid re-running Mesen for
/// every grade. Callers that need the cache data after this call
/// should `load(..)` separately; the two-call pattern is what both
/// the `grader --precompute` path and the main `grade_testcase`
/// call site use.
pub fn ensure_written<F>(
    path: &Path,
    replay: &lockstep::InputReplay,
    frame_count: u32,
    ref_wasm_sha256: &str,
    rom_sha256: &str,
    replay_sha256: &str,
    make_reference: F,
) -> Result<CacheStatus>
where
    F: FnOnce() -> Result<Box<dyn Reference>>,
{
    if let Ok(Some(_)) = load(path, ref_wasm_sha256, rom_sha256, replay_sha256) {
        return Ok(CacheStatus::AlreadyCurrent);
    }
    let mut reference = make_reference()?;
    write(
        path,
        reference.as_mut(),
        replay,
        frame_count,
        ref_wasm_sha256.to_string(),
        rom_sha256.to_string(),
        replay_sha256.to_string(),
    )?;
    Ok(CacheStatus::Wrote)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheStatus {
    /// Cache at path already current — `make_reference` was not called.
    AlreadyCurrent,
    /// Cache was absent or stale; a fresh one was written.
    Wrote,
}

/// Write a cache file by running the given reference through the full
/// replay, capturing every framebuffer + audio drain.
pub fn write(
    path: &Path,
    reference: &mut dyn Reference,
    replay: &lockstep::InputReplay,
    frame_count: u32,
    ref_wasm_sha256: String,
    rom_sha256: String,
    replay_sha256: String,
) -> Result<()> {
    let mut frames_flat = Vec::with_capacity(frame_count as usize * GBA_BYTES);
    let mut audio_samples_per_frame = Vec::with_capacity(frame_count as usize);
    let mut audio_flat = Vec::new();
    let mut ref_defects: Vec<f32> = Vec::with_capacity(frame_count as usize);
    let mut prev_fb: [u32; GBA_PIXELS] = [0; GBA_PIXELS];

    for _ in 0..reference.boot_frames() {
        reference.run_frame();
        let _ = reference.drain_audio(); // discard boot-period audio
    }

    for frame in 0..frame_count {
        reference.set_keys(replay.keys_at(frame));
        reference.run_frame();

        let fb = reference.framebuffer();
        // SAFETY: GBA_PIXELS u32s = GBA_BYTES bytes; same layout.
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(fb.as_ptr() as *const u8, GBA_BYTES)
        };
        frames_flat.extend_from_slice(bytes);

        if frame > 0 && video::ref_in_motion(&prev_fb, fb) {
            ref_defects.push(video::ssim_floored(&prev_fb, fb));
        }
        prev_fb.copy_from_slice(fb);

        let samples = reference.drain_audio();
        let pairs = (samples.len() / 2) as u32;
        audio_samples_per_frame.push(pairs);
        audio_flat.extend(samples);
    }

    let frame_diff_threshold = defect_threshold_clamped(&mut ref_defects);
    let audio_rate = reference.audio_rate();
    let audio_diff_threshold = derive_audio_threshold(&audio_flat, audio_rate) as f32;

    let cache = ReferenceCache {
        version: CACHE_VERSION,
        ref_wasm_sha256,
        rom_sha256,
        replay_sha256,
        frame_count,
        audio_rate,
        boot_frames: reference.boot_frames(),
        frames_flat,
        audio_samples_per_frame,
        audio_flat,
        frame_diff_threshold,
        audio_diff_threshold,
    };

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create {parent:?}"))?;
    }
    let file = std::fs::File::create(path)
        .with_context(|| format!("create {path:?}"))?;
    let mut zstd_writer = zstd::Encoder::new(BufWriter::new(file), 3)
        .context("init zstd encoder")?;
    bincode::serialize_into(&mut zstd_writer, &cache)
        .context("bincode serialize")?;
    zstd_writer.finish().context("zstd finish")?;

    Ok(())
}

/// Reads + decompresses the cache, validates hashes.
///
/// Returns `Ok(Some(cache))` if the cache exists and is valid for the
/// given inputs, `Ok(None)` if hashes mismatch (stale cache — caller
/// should fall back to live Mesen), or `Err` on I/O / format errors.
pub fn load(
    path: &Path,
    expected_ref_wasm_sha256: &str,
    expected_rom_sha256: &str,
    expected_replay_sha256: &str,
) -> Result<Option<CachedReference>> {
    if !path.exists() {
        return Ok(None);
    }
    let file = std::fs::File::open(path)
        .with_context(|| format!("open {path:?}"))?;
    let zstd_reader = zstd::Decoder::new(BufReader::new(file))
        .context("init zstd decoder")?;
    let cache: ReferenceCache = bincode::deserialize_from(zstd_reader)
        .with_context(|| format!("deserialize {path:?}"))?;

    if cache.version != CACHE_VERSION {
        eprintln!(
            "note: {path:?} cache version {} ≠ {CACHE_VERSION}; recomputing.",
            cache.version
        );
        return Ok(None);
    }
    if cache.ref_wasm_sha256 != expected_ref_wasm_sha256 {
        return Ok(None);
    }
    if cache.rom_sha256 != expected_rom_sha256 {
        return Ok(None);
    }
    if cache.replay_sha256 != expected_replay_sha256 {
        return Ok(None);
    }

    let expected_frames = cache.frame_count as usize * GBA_BYTES;
    if cache.frames_flat.len() != expected_frames {
        bail!(
            "{path:?} corrupt: frames_flat is {} bytes, expected {}",
            cache.frames_flat.len(), expected_frames
        );
    }
    if cache.audio_samples_per_frame.len() != cache.frame_count as usize {
        bail!(
            "{path:?} corrupt: audio_samples_per_frame has {} entries, expected {}",
            cache.audio_samples_per_frame.len(), cache.frame_count
        );
    }
    let expected_audio: u64 = cache.audio_samples_per_frame.iter().map(|&n| n as u64 * 2).sum();
    if cache.audio_flat.len() as u64 != expected_audio {
        bail!(
            "{path:?} corrupt: audio_flat has {} i16s, expected {}",
            cache.audio_flat.len(), expected_audio
        );
    }

    Ok(Some(CachedReference::new(cache)))
}

/// Replays a cached Mesen run. Implements `Reference` so the grader's
/// lockstep loop can drive it identically to a live backend — it just
/// advances a cursor instead of actually emulating.
pub struct CachedReference {
    cache: ReferenceCache,
    frame_cursor: i64,
    fb_scratch: [u32; GBA_PIXELS],
    audio_offset: usize,
}

impl CachedReference {
    fn new(cache: ReferenceCache) -> Self {
        Self {
            cache,
            // lockstep() burns `boot_frames()` before the grading loop;
            // -(boot_frames) means the first grading `run_frame` lands on 0.
            frame_cursor: -(1i64),
            fb_scratch: [0; GBA_PIXELS],
            audio_offset: 0,
        }
    }
}

impl Reference for CachedReference {
    fn name(&self) -> &str {
        "Mesen (cached)"
    }

    fn run_frame(&mut self) {
        self.frame_cursor += 1;
        let idx = self.frame_cursor;
        if idx < 0 || idx as u32 >= self.cache.frame_count {
            // Boot-frames consume range [-boot_frames, -1], then 0..frame_count
            // for the grading loop. Anything off either end → keep current
            // framebuffer (zero-init for pre-boot, last real frame for past-end).
            return;
        }
        let start = idx as usize * GBA_BYTES;
        let end = start + GBA_BYTES;
        let bytes = &self.cache.frames_flat[start..end];
        // SAFETY: GBA_BYTES = GBA_PIXELS * 4. Unaligned u32 loads are
        // tolerated on wasm/x86/arm; the grader isn't bottlenecked here.
        let src = unsafe {
            std::slice::from_raw_parts(bytes.as_ptr() as *const u32, GBA_PIXELS)
        };
        self.fb_scratch.copy_from_slice(src);
    }

    fn set_keys(&mut self, _keys: u16) {
        // No-op: the replay's inputs are already baked into the cached frames.
    }

    fn framebuffer(&self) -> &[u32; GBA_PIXELS] {
        &self.fb_scratch
    }

    fn drain_audio(&mut self) -> Vec<i16> {
        let idx = self.frame_cursor;
        if idx < 0 || idx as u32 >= self.cache.frame_count {
            return Vec::new();
        }
        let pairs = self.cache.audio_samples_per_frame[idx as usize] as usize;
        let end = self.audio_offset + pairs * 2;
        let slice = &self.cache.audio_flat[self.audio_offset..end];
        self.audio_offset = end;
        slice.to_vec()
    }

    fn audio_rate(&self) -> u32 {
        self.cache.audio_rate
    }

    fn boot_frames(&self) -> u32 {
        // The cache writer already burned the live reference's boot frames
        // before recording — `cache.frames_flat[0]` is frame 0 of the
        // grading loop, not frame 0 of the emulator after load_rom. Tell
        // lockstep to burn ZERO boot frames here so it doesn't advance
        // the cursor past the grading region. `cache.boot_frames` is
        // preserved in the file for diagnostics only.
        0
    }
}

/// Audio-only view of a reference cache — stripped of framebuffer content
/// and with hash-validation bypassed. Diagnostic use only: scoring paths
/// go through `load()` which verifies sha256 against the expected ROM /
/// replay / reference wasm.
pub struct AudioOnly {
    pub audio_rate: u32,
    pub frame_count: u32,
    /// Interleaved stereo i16 samples, length = 2 * sum(samples_per_frame).
    pub audio_flat: Vec<i16>,
    /// Stereo-pair counts per frame.
    pub samples_per_frame: Vec<u32>,
    /// τ baked in at precompute time, in log-mel L1 units. `None` when
    /// the cache was written before v3 (legacy v2 fallback) and the
    /// field didn't exist yet. A fresh v3 cache will always have
    /// `Some(...)` — even if the value is 0.0 for all-silent replays.
    pub cached_audio_diff_threshold: Option<f32>,
}

/// Load just the audio portion of a refcache, ignoring hash validation.
/// Intended for offline diagnostics (e.g. analysing the adjacent-frame
/// log-mel diff distribution across the corpus to calibrate τ).
///
/// Tolerates older cache versions by falling back to legacy struct
/// shapes — the diagnostic needs to work during the v2 → v3 transition
/// before a precompute sweep has rewritten every file.
pub fn load_audio_unchecked(path: &Path) -> Result<AudioOnly> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("open {path:?}"))?;
    let zstd_reader = zstd::Decoder::new(BufReader::new(file))
        .context("init zstd decoder")?;
    // zstd is a streaming decoder; decompress to bytes so we can try
    // multiple struct layouts without reopening the file.
    let mut bytes = Vec::new();
    {
        use std::io::Read;
        let mut r = zstd_reader;
        r.read_to_end(&mut bytes).context("zstd decompress")?;
    }
    if let Ok(cache) = bincode::deserialize::<ReferenceCache>(&bytes) {
        return Ok(AudioOnly {
            audio_rate: cache.audio_rate,
            frame_count: cache.frame_count,
            audio_flat: cache.audio_flat,
            samples_per_frame: cache.audio_samples_per_frame,
            cached_audio_diff_threshold: Some(cache.audio_diff_threshold),
        });
    }
    if let Ok(v5) = bincode::deserialize::<ReferenceCacheV5>(&bytes) {
        return Ok(AudioOnly {
            audio_rate: v5.audio_rate,
            frame_count: v5.frame_count,
            audio_flat: v5.audio_flat,
            samples_per_frame: v5.audio_samples_per_frame,
            cached_audio_diff_threshold: Some(v5.audio_diff_threshold),
        });
    }
    let v2: ReferenceCacheV2 = bincode::deserialize(&bytes)
        .with_context(|| format!("deserialize {path:?} (v6/v5/v2 all failed)"))?;
    Ok(AudioOnly {
        audio_rate: v2.audio_rate,
        frame_count: v2.frame_count,
        audio_flat: v2.audio_flat,
        samples_per_frame: v2.audio_samples_per_frame,
        cached_audio_diff_threshold: None,
    })
}

/// v2 layout — identical to the current `ReferenceCache` minus
/// `audio_diff_threshold`. Used only by `load_audio_unchecked` as a
/// fallback; the scoring `load()` always routes v2 files through
/// live-Mesen regeneration.
#[derive(Deserialize)]
struct ReferenceCacheV2 {
    #[allow(dead_code)]
    version: u32,
    #[allow(dead_code)]
    ref_wasm_sha256: String,
    #[allow(dead_code)]
    rom_sha256: String,
    #[allow(dead_code)]
    replay_sha256: String,
    frame_count: u32,
    audio_rate: u32,
    #[allow(dead_code)]
    boot_frames: u32,
    #[allow(dead_code)]
    frames_flat: Vec<u8>,
    audio_samples_per_frame: Vec<u32>,
    audio_flat: Vec<i16>,
    #[allow(dead_code)]
    frame_diff_threshold: u32,
}

/// v5 layout. Identical to the current `ReferenceCache` except
/// `frame_diff_threshold` is a u32 pixel-count rather than an f32 GMSD
/// value. Used as a `load_audio_unchecked` fallback so audio-only
/// diagnostics keep working against existing caches on disk until the
/// v6 regen pass lands.
#[derive(Deserialize)]
struct ReferenceCacheV5 {
    #[allow(dead_code)]
    version: u32,
    #[allow(dead_code)]
    ref_wasm_sha256: String,
    #[allow(dead_code)]
    rom_sha256: String,
    #[allow(dead_code)]
    replay_sha256: String,
    frame_count: u32,
    audio_rate: u32,
    #[allow(dead_code)]
    boot_frames: u32,
    #[allow(dead_code)]
    frames_flat: Vec<u8>,
    audio_samples_per_frame: Vec<u32>,
    audio_flat: Vec<i16>,
    #[allow(dead_code)]
    frame_diff_threshold: u32,
    audio_diff_threshold: f32,
}

/// Quick statistics for logging + CI sanity.
pub fn cache_size_summary(corpus_dir: &Path) -> Result<String> {
    let dir = corpus_dir.join("reference-cache");
    if !dir.exists() {
        return Ok("no cache".into());
    }
    let mut count = 0;
    let mut total_bytes = 0u64;
    for entry in std::fs::read_dir(&dir).with_context(|| format!("readdir {dir:?}"))? {
        let entry = entry?;
        if entry.path().extension().and_then(|s| s.to_str()) == Some("refcache") {
            count += 1;
            total_bytes += entry.metadata()?.len();
        }
    }
    Ok(format!("{count} cache files, {:.1} MB on disk", total_bytes as f64 / 1024.0 / 1024.0))
}

