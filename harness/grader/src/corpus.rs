//! Corpus manifest: which ROMs, which replays, what each one tests.
//!
//! `corpus/testcases.json` is the source of truth. ROMs are referenced by
//! SHA-256 — the manifest doesn't care where the file lives, only that
//! its hash matches. All ROMs are homebrew or open-source test suites.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use lockstep::InputReplay;

#[derive(Debug, Deserialize)]
pub struct Corpus {
    pub testcases: Vec<TestCase>,

    #[serde(skip)]
    rom_index: HashMap<String, PathBuf>,
    #[serde(skip)]
    root: PathBuf,
}

#[derive(Debug, Deserialize)]
pub struct TestCase {
    pub id: String,
    /// Which of the three grading sections this belongs to.
    pub section: String,  // "procedural" | "replay"
    /// Subsystem tag for weighting within the section.
    pub subsystem: String,
    /// Audio subsystem tag, if this ROM is expected to produce audio.
    /// null/absent = no audio expected.
    #[serde(default)]
    pub audio_subsystem: Option<String>,
    /// Lowercase hex SHA-256 of the ROM.
    pub rom_sha256: String,
    /// Human-readable, for logs. Not used for lookup.
    pub rom_name: String,
    /// Path relative to `corpus/replays/`. Empty string = no input.
    #[serde(default)]
    pub replay: String,
    /// Frames to run.
    pub frames: u32,
    /// How to aggregate per-frame video scores.
    /// `FrameMean` (default) scores the mean over all frames — the
    /// right thing for timing-sensitive comparisons. `EndState` scores
    /// only the final frame — the right thing for self-checking ROMs
    /// that print PASS/FAIL on screen, where cycle drift during the
    /// test run is irrelevant as long as the final verdict matches.
    #[serde(default)]
    pub scoring_mode: ScoringMode,
    #[allow(dead_code)]
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScoringMode {
    /// Mean of per-frame sigmoid scores over the full histogram.
    #[default]
    FrameMean,
    /// Only the final frame's sigmoid score. For tests whose verdict
    /// lives in the final framebuffer (armwrestler, mgba-suite, etc.)
    /// and where inter-frame timing drift isn't part of what's being
    /// measured.
    #[serde(rename = "endstate")]
    EndState,
}

impl Corpus {
    pub fn load(dir: &Path) -> Result<Self> {
        let manifest = dir.join("testcases.json");
        let json = std::fs::read_to_string(&manifest)
            .with_context(|| format!("reading {manifest:?}"))?;
        let mut corpus: Corpus = serde_json::from_str(&json)
            .with_context(|| format!("parsing {manifest:?}"))?;

        corpus.root = dir.to_path_buf();
        corpus.rom_index = index_roms(&dir.join("roms"))?;

        // Validate sections
        for tc in &corpus.testcases {
            if !["procedural", "replay"].contains(&tc.section.as_str()) {
                eprintln!("warning: testcase {} has unknown section '{}'", tc.id, tc.section);
            }
        }

        Ok(corpus)
    }

    pub fn load_rom(&self, tc: &TestCase) -> Result<Vec<u8>> {
        let path = self.rom_index.get(&tc.rom_sha256).ok_or_else(|| {
            anyhow::anyhow!(
                "no ROM with sha256={} found under {:?}/roms/. \
                 Expected: {}.",
                tc.rom_sha256, self.root, tc.rom_name
            )
        })?;
        std::fs::read(path).with_context(|| format!("reading {path:?}"))
    }

    pub fn load_replay(&self, tc: &TestCase) -> Result<InputReplay> {
        if tc.replay.is_empty() {
            return Ok(InputReplay::new());
        }
        let path = self.root.join("replays").join(&tc.replay);
        InputReplay::from_file(&path)
            .with_context(|| format!("reading replay {path:?}"))
    }

    /// sha256 of the raw replay file bytes, or of the empty string for
    /// no-input testcases. Used as a cache key — same inputs, same
    /// Mesen output, same cache.
    pub fn replay_sha256(&self, tc: &TestCase) -> Option<String> {
        if tc.replay.is_empty() {
            return Some(crate::ref_cache::sha256_hex(&[]));
        }
        let path = self.root.join("replays").join(&tc.replay);
        crate::ref_cache::sha256_file(&path)
    }
}

/// Walk `roms/` recursively, hash every `.gba`, build sha → path.
fn index_roms(dir: &Path) -> Result<HashMap<String, PathBuf>> {
    let mut index = HashMap::new();
    let mut stack = vec![dir.to_path_buf()];

    while let Some(d) = stack.pop() {
        let entries = match std::fs::read_dir(&d) {
            Ok(e) => e,
            Err(_) => continue, // dir doesn't exist yet, fine
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|s| s.to_str()) != Some("gba") {
                continue;
            }
            let bytes = std::fs::read(&path)?;
            let hash = format!("{:x}", Sha256::digest(&bytes));
            if let Some(prev) = index.insert(hash.clone(), path.clone()) {
                // Two files with the same hash — duplicate ROM. Harmless
                // but worth knowing.
                eprintln!("note: duplicate ROM: {prev:?} == {path:?}");
            }
        }
    }

    eprintln!("indexed {} ROMs under {dir:?}", index.len());
    Ok(index)
}

// ─────────────────────────────────────────────────────────────────────────
// Aggregate summary — one per candidate, denormalized for downstream consumers.
// ─────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct SectionScore {
    pub score: f64,
    pub weight: f64,
    pub subsystems: HashMap<String, f64>,
}

#[derive(Debug, Serialize)]
pub struct Summary {
    pub candidate: String,
    pub candidate_sha256: String,
    pub graded_at: String,

    /// Per-testcase scores.
    pub per_testcase: HashMap<String, TestCaseScore>,

    /// The three grading sections.
    pub sections: HashMap<String, SectionScore>,

    /// Weighted overall. The overall sort key.
    pub overall: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TestCaseScore {
    pub video_score: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_score: Option<f64>,
    pub section: String,
    pub subsystem: String,
    /// True when the candidate couldn't run this testcase (typically a
    /// load_rom trap from a wasm whose linear memory is too small for
    /// the ROM). Such entries score 0 — the candidate is responsible
    /// for declaring enough memory — but they still appear in the
    /// summary so consumers can render a placeholder panel instead of
    /// silently dropping the testcase.
    #[serde(skip_serializing_if = "is_false")]
    pub skipped: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<String>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl Summary {
    pub fn new(candidate: String, wasm_bytes: &[u8]) -> Self {
        Self {
            candidate,
            candidate_sha256: format!("{:x}", Sha256::digest(wasm_bytes)),
            graded_at: timestamp(),
            per_testcase: HashMap::new(),
            sections: HashMap::new(),
            overall: 0.0,
        }
    }

    pub fn overall_score(&self) -> f64 {
        self.overall
    }
}

fn timestamp() -> String {
    // `date -u +%Y-%m-%dT%H:%M:%SZ` is portable across mac/linux. If it
    // fails (no `date`? unlikely), an empty string is fine — it's a
    // display field, not load-bearing.
    std::process::Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}
