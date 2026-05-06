//! Grade a candidate wasm against a reference wasm using three scored
//! sections (weights configurable in corpus/grader.yaml):
//!
//!   1. Procedural Tests — self-contained test ROMs
//!   2. Gameplay Replays — replay-driven, timing-critical
//!   3. Audio            — log-mel spectral distance
//!
//! Writes:
//!   <out_dir>/<testcase>.json     per-testcase lockstep result
//!   <out_dir>/summary.json        weighted overall + per-section scores

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use lockstep::media::{write_png, write_wav_capped, WAV_SIZE_CAP};
use lockstep::{lockstep, video_encode::VideoEncoder, Reference};
use rayon::prelude::*;
use serde::Deserialize;

use grader::corpus::{Corpus, SectionScore, Summary, TestCase, TestCaseScore};
use grader::ref_cache;
use grader::wasm_candidate::WasmCandidate;

// ─────────────────────────────────────────────────────────────────────────
// Config
// ─────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(default)]
struct GraderConfig {
    /// Escape hatch for candidates that need warmup beyond what their
    /// own `boot_frames()` reports. For conformant candidates this is 0.
    candidate_boot_frames: u32,
    fuel_per_frame: u64,
    fuel_load_rom: u64,

    /// Section weights. Must sum to 1.0.
    section_weights: HashMap<String, f64>,

    /// Subsystem weights within each section. Each inner map sums to 1.0.
    subsystem_weights: HashMap<String, HashMap<String, f64>>,
}

impl Default for GraderConfig {
    /// Keep these in sync with `corpus/grader.yaml`.
    fn default() -> Self {
        let mut sw = HashMap::new();
        sw.insert("procedural".into(), 0.20);
        sw.insert("replay".into(), 0.60);
        sw.insert("audio".into(), 0.20);

        Self {
            candidate_boot_frames: 0,
            fuel_per_frame: 500_000_000,
            fuel_load_rom: 60_000_000_000,
            section_weights: sw,
            subsystem_weights: HashMap::new(),
        }
    }
}

impl GraderConfig {
    fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_yaml::from_str(&s)
                .with_context(|| format!("parsing {path:?}")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("note: {path:?} not found, using defaults");
                Ok(Self::default())
            }
            Err(e) => Err(e).with_context(|| format!("reading {path:?}")),
        }
    }
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut positionals = Vec::new();
    let mut config_path: Option<PathBuf> = None;
    let mut reference_override: Option<PathBuf> = None;
    let mut precompute = false;
    let mut emit_video = false;
    while let Some(a) = args.next() {
        if a == "--config" {
            config_path = Some(PathBuf::from(
                args.next().context("--config needs a path")?,
            ));
        } else if a == "--reference" {
            reference_override = Some(PathBuf::from(
                args.next().context("--reference needs a wasm path")?,
            ));
        } else if a == "--precompute" {
            precompute = true;
        } else if a == "--emit-video" {
            emit_video = true;
        } else if a.starts_with("--") {
            bail!("unknown flag: {a}");
        } else {
            positionals.push(a);
        }
    }

    if precompute {
        return precompute_cache(positionals, config_path, reference_override);
    }

    if positionals.len() != 3 {
        eprintln!(
            "usage:\n  \
             grader <candidate.wasm> <corpus_dir> <out_dir> [--config <path>] [--reference <ref.wasm>] [--emit-video]\n  \
             grader --precompute <corpus_dir> [--config <path>] [--reference <ref.wasm>]\n\n\
             --reference defaults to reference/mesen.wasm. Pass any wasm\n\
             implementing the ABI to grade against a different reference."
        );
        std::process::exit(2);
    }
    let candidate_path = PathBuf::from(&positionals[0]);
    let corpus_dir = PathBuf::from(&positionals[1]);
    let out_dir = PathBuf::from(&positionals[2]);

    let config_path = config_path.unwrap_or_else(|| corpus_dir.join("grader.yaml"));
    let config = GraderConfig::load(&config_path)?;

    // ─── Load reference wasm ────────────────────────────────────────────
    // The reference is any wasm implementing the ABI. Default is the
    // bundled Mesen2 build at reference/mesen.wasm; --reference overrides.
    // The reference cache is keyed by the wasm sha, so swapping references
    // doesn't poison the cache.
    let ref_path = reference_override.clone()
        .unwrap_or_else(|| PathBuf::from("reference/mesen.wasm"));
    eprintln!("config: reference={}", ref_path.display());
    let ref_wasm_bytes = std::fs::read(&ref_path)
        .with_context(|| format!("reading reference wasm {ref_path:?}"))?;
    let ref_wasm_sha = ref_cache::sha256_hex(&ref_wasm_bytes);

    // ─── Init candidate ─────────────────────────────────────────────────
    let candidate_bytes = std::fs::read(&candidate_path)
        .with_context(|| format!("reading candidate {candidate_path:?}"))?;
    let candidate_label = candidate_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("candidate")
        .to_string();
    eprintln!("candidate: {candidate_label} ({} bytes)\n", candidate_bytes.len());

    // Sanity-instantiate once to surface link errors before spawning
    // rayon workers — every per-testcase worker below builds its own.
    let _ = WasmCandidate::new(
        &candidate_bytes,
        candidate_label.clone(),
        config.fuel_per_frame,
        config.fuel_load_rom,
    ).context("instantiating candidate wasm")?;

    // ─── Load corpus ────────────────────────────────────────────────────
    let corpus = Corpus::load(&corpus_dir)
        .with_context(|| format!("loading corpus from {corpus_dir:?}"))?;
    eprintln!("corpus: {} testcases\n", corpus.testcases.len());

    let mut summary = Summary::new(candidate_label.clone(), &candidate_bytes);

    // Collect results per section → subsystem → [scores]
    // video_scores: section → subsystem → [video_score]
    // audio_scores: audio_subsystem → [audio_score]
    let mut video_scores: HashMap<String, HashMap<String, Vec<f64>>> = HashMap::new();
    let mut audio_scores: HashMap<String, Vec<f64>> = HashMap::new();

    // ═══════════════════════════════════════════════════════════════════
    // Run all testcases through lockstep (in parallel via rayon)
    // ═══════════════════════════════════════════════════════════════════
    // Testcases are independent: each builds its own reference + candidate
    // wasmtime Store, writes to unique per-tc file paths, and returns its
    // scores. Cap concurrency with RAYON_NUM_THREADS when stacking several
    // grader processes on the same box.
    let tc_results: Vec<TcOut> = corpus.testcases
        .par_iter()
        .map(|tc| grade_testcase(
            tc,
            &corpus,
            &corpus_dir,
            &out_dir,
            &config,
            &ref_wasm_bytes,
            &ref_wasm_sha,
            &candidate_bytes,
            &candidate_label,
            emit_video,
        ))
        .collect::<Result<Vec<_>>>()?;

    // Flush per-tc logs in testcase order so output is deterministic.
    for r in &tc_results {
        eprint!("{}", r.log);
    }

    for r in &tc_results {
        // Skipped testcases (candidate load_rom trapped, e.g. wasm
        // memory too small for the ROM) score 0 and surface the reason.
        // Aggregation treats the 0 as a real score because the candidate
        // is responsible for loading every ROM in the corpus.
        let video = r.video.unwrap_or(0.0);
        summary.per_testcase.insert(r.tc_id.clone(), TestCaseScore {
            video_score: video,
            audio_score: r.audio,
            section: r.section.clone(),
            subsystem: r.subsystem.clone(),
            skipped: r.skip_reason.is_some(),
            skip_reason: r.skip_reason.clone(),
        });
        video_scores
            .entry(r.section.clone())
            .or_default()
            .entry(r.subsystem.clone())
            .or_default()
            .push(video);
        if let (Some(audio_sub), Some(audio_sc)) = (&r.audio_subsystem, r.audio) {
            audio_scores.entry(audio_sub.clone()).or_default().push(audio_sc);
        }
    }

    // ═══════════════════════════════════════════════════════════════════
    // Aggregate: subsystem averages → section scores → overall
    // ═══════════════════════════════════════════════════════════════════

    let mut section_details: HashMap<String, SectionScore> = HashMap::new();

    for section_name in &["procedural", "replay"] {
        let section_str = section_name.to_string();
        let sub_weights = config.subsystem_weights.get(*section_name)
            .cloned()
            .unwrap_or_default();

        let sub_scores_map = video_scores.get(*section_name);
        let mut subsystem_scores: HashMap<String, f64> = HashMap::new();
        let mut weighted_sum = 0.0;
        let mut weight_total = 0.0;

        for (sub_name, sub_weight) in &sub_weights {
            let scores = sub_scores_map
                .and_then(|m| m.get(sub_name))
                .map(|v| v.as_slice())
                .unwrap_or(&[]);
            let avg = if scores.is_empty() { 0.0 } else {
                scores.iter().sum::<f64>() / scores.len() as f64
            };
            subsystem_scores.insert(sub_name.clone(), avg);
            weighted_sum += avg * sub_weight;
            weight_total += sub_weight;
        }

        let score = if weight_total > 0.0 { weighted_sum / weight_total } else { 0.0 };

        let sec_weight = config.section_weights.get(*section_name).copied().unwrap_or(0.0);
        section_details.insert(section_str, SectionScore {
            score,
            weight: sec_weight,
            subsystems: subsystem_scores,
        });
    }

    // Audio section
    {
        let sub_weights = config.subsystem_weights.get("audio")
            .cloned()
            .unwrap_or_default();

        let mut subsystem_scores: HashMap<String, f64> = HashMap::new();
        let mut weighted_sum = 0.0;
        let mut weight_total = 0.0;

        for (sub_name, sub_weight) in &sub_weights {
            let scores = audio_scores.get(sub_name)
                .map(|v| v.as_slice())
                .unwrap_or(&[]);
            let avg = if scores.is_empty() { 0.0 } else {
                scores.iter().sum::<f64>() / scores.len() as f64
            };
            subsystem_scores.insert(sub_name.clone(), avg);
            weighted_sum += avg * sub_weight;
            weight_total += sub_weight;
        }

        let score = if weight_total > 0.0 { weighted_sum / weight_total } else { 0.0 };

        let sec_weight = config.section_weights.get("audio").copied().unwrap_or(0.0);
        section_details.insert("audio".into(), SectionScore {
            score,
            weight: sec_weight,
            subsystems: subsystem_scores,
        });
    }

    // Compute overall
    let mut overall = 0.0;
    for (name, sec) in &section_details {
        let weight = config.section_weights.get(name).copied().unwrap_or(0.0);
        overall += sec.score * weight;
    }

    summary.sections = section_details.clone();
    summary.overall = overall;

    // ═══════════════════════════════════════════════════════════════════
    // Report
    // ═══════════════════════════════════════════════════════════════════
    eprintln!("\n╔══════════════════════════════════════════════════════════╗");
    eprintln!("║  OVERALL: {overall:.4}                                        ║");
    eprintln!("╠══════════════════════════════════════════════════════════╣");
    for name in &["procedural", "replay", "audio"] {
        if let Some(sec) = section_details.get(*name) {
            let bar_len = (sec.score * 20.0).round() as usize;
            let bar: String = "█".repeat(bar_len) + &"░".repeat(20 - bar_len);
            eprintln!("║  {:<12} {:.4} {bar}", name, sec.score);
        }
    }
    eprintln!("╚══════════════════════════════════════════════════════════╝");
    eprintln!("written to {out_dir:?}");

    // Write summary
    std::fs::write(
        out_dir.join("summary.json"),
        serde_json::to_string_pretty(&summary)?,
    )?;

    Ok(())
}

/// Per-testcase output from the parallel grading pass. `video == None`
/// = the testcase was skipped (ROM unreadable or candidate trapped on
/// load_rom); skipped testcases score 0 in aggregation.
struct TcOut {
    tc_id: String,
    section: String,
    subsystem: String,
    audio_subsystem: Option<String>,
    video: Option<f64>,
    audio: Option<f64>,
    skip_reason: Option<String>,
    log: String,
}

#[allow(clippy::too_many_arguments)]
fn grade_testcase(
    tc: &TestCase,
    corpus: &Corpus,
    corpus_dir: &Path,
    out_dir: &Path,
    config: &GraderConfig,
    ref_wasm_bytes: &[u8],
    ref_wasm_sha: &str,
    candidate_bytes: &[u8],
    candidate_label: &str,
    emit_video: bool,
) -> Result<TcOut> {
    let mut log = String::new();
    let _ = writeln!(log, "── {} ({}/{}) ──────────────────────────", tc.id, tc.section, tc.subsystem);

    let skip = |log: String, reason: String| Ok(TcOut {
        tc_id: tc.id.clone(),
        section: tc.section.clone(),
        subsystem: tc.subsystem.clone(),
        audio_subsystem: tc.audio_subsystem.clone(),
        video: None,
        audio: None,
        skip_reason: Some(reason),
        log,
    });

    let rom = match corpus.load_rom(tc) {
        Ok(r) => r,
        Err(e) => {
            let msg = format!("{e}");
            let _ = writeln!(log, "  skip: {msg}");
            return skip(log, format!("load_rom: {msg}"));
        }
    };
    let replay = corpus.load_replay(tc)
        .with_context(|| format!("loading replay for {}", tc.id))?;

    // Try the precomputed cache first. Hit → no reference execution;
    // miss → compute and persist it, then load the fresh file. Going
    // through `ensure_written` rather than a silent live fallback means
    // every grade leaves the cache in a current state.
    let cache_path = ref_cache::cache_path_for(corpus_dir, &tc.id);
    let replay_sha = corpus
        .replay_sha256(tc)
        .unwrap_or_else(|| ref_cache::sha256_hex(&[]));
    match ref_cache::ensure_written(
        &cache_path,
        &replay,
        tc.frames,
        ref_wasm_sha,
        &tc.rom_sha256,
        &replay_sha,
        || {
            let _ = writeln!(log, "  note: {} cache absent/stale; warming", tc.id);
            let mut r = WasmCandidate::new(
                ref_wasm_bytes, "reference".into(),
                config.fuel_per_frame, config.fuel_load_rom,
            ).with_context(|| format!("instantiating reference wasm for {}", tc.id))?;
            r.load_rom(&rom)
                .with_context(|| format!("reference load_rom for {}", tc.id))?;
            Ok(Box::new(r) as Box<dyn lockstep::Reference>)
        },
    ) {
        Ok(_) => {}
        Err(e) => {
            let _ = writeln!(log, "  note: {} warm failed: {e:#}", tc.id);
        }
    }
    let cached = ref_cache::load(&cache_path, ref_wasm_sha, &tc.rom_sha256, &replay_sha)
        .with_context(|| format!("load refcache for {}", tc.id))?
        .with_context(|| format!("refcache for {} still missing after ensure_written", tc.id))?;
    let mut reference: Box<dyn Reference> = Box::new(cached);

    // Each worker owns its own candidate Store — a trap here only
    // poisons this testcase, not the run.
    let mut candidate = WasmCandidate::new(
        candidate_bytes,
        candidate_label.to_string(),
        config.fuel_per_frame,
        config.fuel_load_rom,
    ).context("instantiating candidate wasm")?;

    if let Err(e) = candidate.load_rom(&rom) {
        let msg = format!("{e:#}");
        let _ = writeln!(log, "  skip: candidate load_rom trapped for {}: {msg}", tc.id);
        return skip(log, format!("candidate load_rom trapped: {msg}"));
    }

    // Reference warmup is not burned here — lockstep() does it from
    // reference.boot_frames(). The candidate escape hatch runs on top.
    for _ in 0..config.candidate_boot_frames {
        candidate.run_frame();
    }

    // Optional video output: three ffmpeg children per testcase writing
    // <tc>.ref.mp4 / <tc>.cand.mp4 / <tc>.diff.mp4. Failure to spawn
    // (ffmpeg missing, permissions) demotes to a warning — scoring still
    // runs. Once spawned, per-frame write failures are swallowed inside
    // lockstep (see the `warning: video encoder failed…` log path).
    let video_base = out_dir.join(&tc.id);
    let mut encoder = if emit_video {
        match VideoEncoder::new(&video_base) {
            Ok(e) => Some(e),
            Err(e) => {
                let _ = writeln!(log, "  warning: video disabled for {}: {e}", tc.id);
                None
            }
        }
    } else {
        None
    };

    let mut output = lockstep(
        &mut reference,
        &mut candidate,
        tc.frames,
        &replay,
        encoder.as_mut(),
    );
    if let Some(enc) = encoder {
        match enc.finish() {
            Ok(()) => {
                output.result.has_video = true;
            }
            Err(e) => {
                let _ = writeln!(log, "  warning: video finalize failed for {}: {e}", tc.id);
            }
        }
    }
    output.result.report_to(reference.name(), candidate.name(), &mut log);

    std::fs::write(
        out_dir.join(format!("{}.json", tc.id)),
        serde_json::to_string(&output.result)?,
    )?;
    let ref_png = out_dir.join(format!("{}.ref.png", tc.id));
    let cand_png = out_dir.join(format!("{}.cand.png", tc.id));
    if let Err(e) = write_png(&ref_png, reference.framebuffer()) {
        eprintln!("warning: {}: {e}", ref_png.display());
    }
    if let Err(e) = write_png(&cand_png, candidate.framebuffer()) {
        eprintln!("warning: {}: {e}", cand_png.display());
    }
    if output.result.audio_score.is_some() {
        let ref_wav = out_dir.join(format!("{}.ref.wav", tc.id));
        let cand_wav = out_dir.join(format!("{}.cand.wav", tc.id));
        if let Err(e) = write_wav_capped(&ref_wav, &output.ref_audio, output.audio_rate, WAV_SIZE_CAP) {
            eprintln!("warning: {}: {e}", ref_wav.display());
        }
        if let Err(e) = write_wav_capped(&cand_wav, &output.cand_audio, output.audio_rate, WAV_SIZE_CAP) {
            eprintln!("warning: {}: {e}", cand_wav.display());
        }
    }

    let video = match tc.scoring_mode {
        grader::corpus::ScoringMode::FrameMean => output.result.video_score(),
        grader::corpus::ScoringMode::EndState => output.result.endstate_score(),
    };

    Ok(TcOut {
        tc_id: tc.id.clone(),
        section: tc.section.clone(),
        subsystem: tc.subsystem.clone(),
        audio_subsystem: tc.audio_subsystem.clone(),
        video: Some(video),
        audio: output.result.audio_score,
        skip_reason: None,
        log,
    })
}

// ─────────────────────────────────────────────────────────────────────────
// Precompute mode
// ─────────────────────────────────────────────────────────────────────────

/// Run Mesen through every testcase and cache its output to disk. Called
/// via `grader --precompute <corpus_dir> [--config <path>]`. Idempotent:
/// rerunning it just overwrites the same files. Hashes in each cache
/// entry ensure live grading auto-invalidates on Mesen / ROM / replay
/// changes.
fn precompute_cache(
    positionals: Vec<String>,
    config_path: Option<PathBuf>,
    reference_override: Option<PathBuf>,
) -> Result<()> {
    if positionals.len() != 1 {
        eprintln!("usage: grader --precompute <corpus_dir> [--config <path>] [--reference <ref.wasm>]");
        std::process::exit(2);
    }
    let corpus_dir = PathBuf::from(&positionals[0]);
    let config_path = config_path.unwrap_or_else(|| corpus_dir.join("grader.yaml"));
    let config = GraderConfig::load(&config_path)?;

    let ref_wasm_path = reference_override
        .unwrap_or_else(|| PathBuf::from("reference/mesen.wasm"));
    let ref_wasm_bytes = std::fs::read(&ref_wasm_path)
        .with_context(|| format!("reading reference wasm {ref_wasm_path:?}"))?;
    let ref_wasm_sha = ref_cache::sha256_hex(&ref_wasm_bytes);

    let corpus = Corpus::load(&corpus_dir)
        .with_context(|| format!("loading corpus from {corpus_dir:?}"))?;
    eprintln!("precompute: corpus={:?} reference={} ({} bytes, sha {})",
        corpus_dir, ref_wasm_path.display(), ref_wasm_bytes.len(), &ref_wasm_sha[..12]);

    // Run testcases in parallel. Each call gets its own wasmtime Store
    // (via `WasmCandidate::new`) and writes to a unique path, so they
    // don't share state. `RAYON_NUM_THREADS` caps concurrency when
    // stacking multiple grader processes on the same box.
    use std::sync::atomic::{AtomicUsize, Ordering};
    let wrote = AtomicUsize::new(0);
    let skipped = AtomicUsize::new(0);
    let errors = AtomicUsize::new(0);
    let ref_wasm_bytes = &ref_wasm_bytes; // borrow for the closures

    corpus.testcases.par_iter().for_each(|tc| {
        let rom = match corpus.load_rom(tc) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("  skip {}: {e}", tc.id);
                skipped.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        let replay = match corpus.load_replay(tc) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("  err  {}: loading replay: {e}", tc.id);
                errors.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        let replay_sha = corpus.replay_sha256(tc).unwrap_or_else(|| ref_cache::sha256_hex(&[]));
        let cache_path = ref_cache::cache_path_for(&corpus_dir, &tc.id);

        let status = ref_cache::ensure_written(
            &cache_path,
            &replay,
            tc.frames,
            &ref_wasm_sha,
            &tc.rom_sha256,
            &replay_sha,
            || {
                eprintln!("  run  {} ({} frames)", tc.id, tc.frames);
                let mut reference = WasmCandidate::new(
                    ref_wasm_bytes, "Mesen".into(),
                    config.fuel_per_frame, config.fuel_load_rom,
                ).with_context(|| format!("instantiate Mesen for {}", tc.id))?;
                reference.load_rom(&rom)
                    .with_context(|| format!("Mesen load_rom for {}", tc.id))?;
                Ok(Box::new(reference) as Box<dyn lockstep::Reference>)
            },
        );
        match status {
            Ok(ref_cache::CacheStatus::AlreadyCurrent) => {
                eprintln!("  ok   {} (cache current)", tc.id);
            }
            Ok(ref_cache::CacheStatus::Wrote) => {
                wrote.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                eprintln!("  err  {}: {e:#}", tc.id);
                errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    });

    let wrote = wrote.load(Ordering::Relaxed);
    let skipped = skipped.load(Ordering::Relaxed);
    let errors = errors.load(Ordering::Relaxed);
    eprintln!("\nprecompute: {wrote} written, {skipped} skipped, {errors} errors");
    eprintln!("{}", ref_cache::cache_size_summary(&corpus_dir).unwrap_or_default());
    if errors > 0 {
        bail!("{errors} testcases failed during precompute");
    }
    Ok(())
}
