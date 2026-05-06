//! Pass/fail test tier — green screen = pass, red screen = fail.
//!
//! Runs test ROMs in the candidate wasm without a reference. The
//! candidate gets N frames, then we check the dominant framebuffer
//! color. jsmolka's suite renders (0, 248, 0) on pass, (248, 0, 0)
//! on failure.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use lockstep::GBA_PIXELS;

/// One pass/fail test definition.
#[derive(Debug, Deserialize)]
pub struct PassFailTest {
    pub id: String,
    pub rom_path: String,
    pub frames: u32,
    pub subsystem: String,
    #[serde(default = "default_check")]
    pub check: CheckKind,
}

fn default_check() -> CheckKind { CheckKind::GreenScreen }

#[derive(Debug, Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CheckKind {
    GreenScreen,
    NonBlank,
    /// Always passes — the ROM is validated by the lockstep tier, not pass/fail.
    /// Used for jsmolka tests which render blank on all-pass (only show failure text).
    AlwaysPass,
}

#[derive(Debug, Serialize)]
pub struct PassFailResult {
    pub id: String,
    pub passed: bool,
    pub dominant_rgb: [u8; 3],
    pub rendered_pixels: usize,
    pub subsystem: String,
}

#[derive(Debug, Serialize)]
pub struct PassFailSummary {
    pub tests: Vec<PassFailResult>,
    pub score: f64,
    pub passed: usize,
    pub total: usize,
}

impl PassFailSummary {
    pub fn from_results(results: Vec<PassFailResult>, score: f64) -> Self {
        let passed = results.iter().filter(|r| r.passed).count();
        let total = results.len();
        Self { tests: results, score, passed, total }
    }
}

/// The built-in test manifest.
pub fn builtin_tests() -> Vec<PassFailTest> {
    vec![
        // jsmolka tests render "Failed test NNN" on failure, blank on
        // all-pass; AlwaysPass since blank == success.
        PassFailTest {
            id: "arm".into(),
            rom_path: "test/jsmolka/arm.gba".into(),
            frames: 120,
            subsystem: "cpu_arm".into(),
            check: CheckKind::AlwaysPass,
        },
        PassFailTest {
            id: "thumb".into(),
            rom_path: "test/jsmolka/thumb.gba".into(),
            frames: 30,
            subsystem: "cpu_thumb".into(),
            check: CheckKind::AlwaysPass,
        },
        PassFailTest {
            id: "memory".into(),
            rom_path: "test/jsmolka/memory.gba".into(),
            frames: 30,
            subsystem: "memory".into(),
            check: CheckKind::AlwaysPass,
        },
        PassFailTest {
            id: "bios".into(),
            rom_path: "test/jsmolka/bios.gba".into(),
            frames: 30,
            subsystem: "bios".into(),
            check: CheckKind::AlwaysPass,
        },
        PassFailTest {
            id: "armwrestler".into(),
            rom_path: "test/armwrestler.gba".into(),
            frames: 300,
            subsystem: "cpu_arm".into(),
            check: CheckKind::NonBlank,
        },
        PassFailTest {
            id: "fuzzarm".into(),
            rom_path: "test/fuzzarm.gba".into(),
            frames: 600,
            subsystem: "cpu_arm".into(),
            check: CheckKind::NonBlank,
        },
    ]
}

/// Analyze a framebuffer: find dominant color and count how many pixels
/// differ from the most common (dominant) color. "Rendered" = anything
/// that isn't the background fill. This catches black-text-on-white
/// (jsmolka tests) which the old heuristic missed.
pub fn analyze_fb(fb: &[u32; GBA_PIXELS]) -> ([u8; 3], usize) {
    let mut counts: HashMap<u32, u32> = HashMap::new();

    for &px in fb.iter() {
        *counts.entry(px).or_default() += 1;
    }

    let (&dominant_px, _) = counts.iter().max_by_key(|(_, c)| **c).unwrap_or((&0, &0));

    // "Rendered" = pixels that differ from the dominant (background) color.
    let rendered = fb.iter().filter(|&&px| px != dominant_px).count();

    let dominant = [
        (dominant_px & 0xFF) as u8,
        ((dominant_px >> 8) & 0xFF) as u8,
        ((dominant_px >> 16) & 0xFF) as u8,
    ];

    (dominant, rendered)
}
