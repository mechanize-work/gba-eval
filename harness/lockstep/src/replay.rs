//! Frame-indexed input replay.
//!
//! Two formats: text (human-editable) and JSON (what an editor emits).
//!
//! ## Text format
//!
//! ```text
//! # comments
//! 47   001    # press A on frame 47
//! 50   000    # release on frame 50
//! 120  009    # A + Start
//! ```
//!
//! `<frame> <keys_hex>` per line. State persists between entries — at
//! frame 100 in the example above, no buttons are pressed (last event
//! was the release at 50).
//!
//! ## JSON format
//!
//! ```json
//! { "events": [[47, 1], [50, 0], [120, 9]] }
//! ```
//!
//! Same semantics. The grader accepts either.
//!
//! ## Why this works without savestates
//!
//! The GBA has no entropy. No RTC by default, no analog input, no wall
//! clock visible to games. RNG seeds come from a timer or VCOUNT sampled
//! at the moment of player input. Pin the input frame → pin the seed →
//! identical playthrough, indefinitely. The replay IS the full state.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct InputReplay {
    /// (frame, keys) sorted by frame. Keys are active-high KEYINPUT.
    events: Vec<(u32, u16)>,
}

impl InputReplay {
    pub fn new() -> Self {
        Self { events: Vec::new() }
    }

    /// Builder-style: press these keys starting at `frame`.
    pub fn at(mut self, frame: u32, keys: u16) -> Self {
        self.events.push((frame, keys));
        self.events.sort_unstable_by_key(|&(f, _)| f);
        self
    }

    /// Keys held during `frame`. The most recent event ≤ frame wins.
    pub fn keys_at(&self, frame: u32) -> u16 {
        // Linear scan — replays have few events.
        let mut keys = 0u16;
        for &(f, k) in &self.events {
            if f > frame {
                break;
            }
            keys = k;
        }
        keys
    }

    /// Last frame anything happens. Useful default for `n_frames`.
    pub fn last_event_frame(&self) -> u32 {
        self.events.last().map(|&(f, _)| f).unwrap_or(0)
    }

    pub fn from_text(text: &str) -> Self {
        let mut r = Self::new();
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let mut parts = line.split_whitespace();
            if let (Some(f), Some(k)) = (parts.next(), parts.next()) {
                if let (Ok(f), Ok(k)) = (f.parse(), u16::from_str_radix(k, 16)) {
                    r.events.push((f, k));
                }
            }
        }
        r.events.sort_unstable_by_key(|&(f, _)| f);
        r
    }

    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        let mut r: Self = serde_json::from_str(json)?;
        r.events.sort_unstable_by_key(|&(f, _)| f);
        Ok(r)
    }

    /// Try JSON first, fall back to text. Lets `corpus/replays/` mix both.
    pub fn from_file(path: &Path) -> Result<Self, std::io::Error> {
        let s = std::fs::read_to_string(path)?;
        Ok(Self::from_json(&s).unwrap_or_else(|_| Self::from_text(&s)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_persist_between_events() {
        let r = InputReplay::new().at(10, 0x001).at(20, 0x000);
        assert_eq!(r.keys_at(5), 0);
        assert_eq!(r.keys_at(10), 0x001);
        assert_eq!(r.keys_at(15), 0x001);
        assert_eq!(r.keys_at(20), 0x000);
        assert_eq!(r.keys_at(100), 0x000);
    }

    #[test]
    fn text_parses() {
        let r = InputReplay::from_text("47 001\n# comment\n50 000\n");
        assert_eq!(r.keys_at(47), 1);
        assert_eq!(r.keys_at(50), 0);
    }

    #[test]
    fn json_round_trips() {
        let r = InputReplay::new().at(10, 1).at(20, 9);
        let s = serde_json::to_string(&r).unwrap();
        let r2 = InputReplay::from_json(&s).unwrap();
        assert_eq!(r2.keys_at(15), 1);
    }
}
