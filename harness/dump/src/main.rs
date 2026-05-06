//! Dump WAV and/or MP4 from a wasm + ROM + optional replay.
//!
//! Works against any wasm conforming to spec/ABI.md — your candidate,
//! the bundled `reference/mesen.wasm`, or anything else implementing
//! the same 10 exports.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{exit, Child, ChildStdin, Command, Stdio};

use grader::wasm_candidate::WasmCandidate;
use lockstep::media::{fb_to_rgba, write_wav};
use lockstep::video_encode::GBA_FPS;
use lockstep::{InputReplay, Reference, GBA_H, GBA_PIXELS, GBA_W};

const DEFAULT_FUEL_PER_FRAME: u64 = 500_000_000;
const DEFAULT_FUEL_LOAD_ROM: u64 = 300_000_000_000;

fn parse_flag(args: &mut Vec<String>, flag: &str) -> Option<String> {
    if let Some(idx) = args.iter().position(|a| a == flag) {
        if idx + 1 < args.len() {
            let val = args.remove(idx + 1);
            args.remove(idx);
            return Some(val);
        }
    }
    None
}

fn print_usage() {
    eprintln!("usage: dump <wasm> <rom> <frames> \\");
    eprintln!("            [--replay <file>] [--wav <out.wav>] [--mp4 <out.mp4>]");
    eprintln!();
    eprintln!("Renders <rom> through <wasm> for <frames> frames and writes");
    eprintln!("WAV and/or MP4 of the output. At least one of --wav or --mp4");
    eprintln!("must be set.");
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help" || a == "help") {
        print_usage();
        exit(0);
    }

    let replay_path = parse_flag(&mut args, "--replay").map(PathBuf::from);
    let wav_path = parse_flag(&mut args, "--wav").map(PathBuf::from);
    let mp4_path = parse_flag(&mut args, "--mp4").map(PathBuf::from);

    if args.len() != 3 {
        print_usage();
        exit(2);
    }
    if wav_path.is_none() && mp4_path.is_none() {
        eprintln!("error: pass --wav and/or --mp4");
        exit(2);
    }

    let wasm_path = PathBuf::from(&args[0]);
    let rom_path = PathBuf::from(&args[1]);
    let n_frames: u32 = args[2].parse().unwrap_or_else(|_| {
        eprintln!("error: <frames> must be an integer");
        exit(2);
    });

    let wasm_bytes = fs::read(&wasm_path).unwrap_or_else(|e| {
        eprintln!("error: read {}: {e}", wasm_path.display());
        exit(1);
    });
    let rom = fs::read(&rom_path).unwrap_or_else(|e| {
        eprintln!("error: read {}: {e}", rom_path.display());
        exit(1);
    });
    let inputs = match replay_path.as_ref() {
        Some(p) => InputReplay::from_file(p).unwrap_or_else(|e| {
            eprintln!("error: read replay {}: {e}", p.display());
            exit(1);
        }),
        None => InputReplay::new(),
    };

    let label = wasm_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "candidate".into());
    let mut emu = WasmCandidate::new(
        &wasm_bytes, label,
        DEFAULT_FUEL_PER_FRAME, DEFAULT_FUEL_LOAD_ROM,
    ).unwrap_or_else(|e| {
        eprintln!("error: load wasm {}: {e}", wasm_path.display());
        exit(1);
    });
    emu.load_rom(&rom).unwrap_or_else(|e| {
        eprintln!("error: load_rom {}: {e}", rom_path.display());
        exit(1);
    });

    // Burn the candidate's self-declared boot frames so frame 0 of the
    // replay lines up with frame 0 of the recording.
    for _ in 0..emu.boot_frames() {
        emu.run_frame();
        let _ = emu.drain_audio();
    }

    let audio_rate = emu.audio_rate();

    let mut audio: Vec<i16> = Vec::new();
    let mut mp4 = mp4_path
        .as_ref()
        .map(|p| Mp4Encoder::spawn(p).unwrap_or_else(|e| {
            eprintln!("error: spawn ffmpeg for {}: {e}", p.display());
            exit(1);
        }));

    let mut rgba = vec![0u8; GBA_PIXELS * 4];
    for frame in 0..n_frames {
        emu.set_keys(inputs.keys_at(frame));
        emu.run_frame();
        if mp4.is_some() {
            fb_to_rgba(emu.framebuffer(), &mut rgba);
            mp4.as_mut().unwrap().write(&rgba).unwrap_or_else(|e| {
                eprintln!("error: write frame {frame} to mp4: {e}");
                exit(1);
            });
        }
        if wav_path.is_some() {
            audio.extend_from_slice(&emu.drain_audio());
        } else {
            // Drain even when not writing so the candidate's audio
            // buffer doesn't fill up over a long run.
            let _ = emu.drain_audio();
        }
    }

    if let Some(p) = wav_path.as_ref() {
        write_wav(p, &audio, audio_rate).unwrap_or_else(|e| {
            eprintln!("error: write wav {}: {e}", p.display());
            exit(1);
        });
        eprintln!(
            "wav: {} stereo pairs @ {} Hz → {}",
            audio.len() / 2, audio_rate, p.display(),
        );
    }
    if let Some(p) = mp4_path.as_ref() {
        mp4.unwrap().finish().unwrap_or_else(|e| {
            eprintln!("error: finalize mp4 {}: {e}", p.display());
            exit(1);
        });
        eprintln!("mp4: {} frames → {}", n_frames, p.display());
    }
}

// ─── Single-stream MP4 encoder ────────────────────────────────────────
//
// Pipes raw RGBA into ffmpeg. Same encoder settings as
// lockstep::video_encode (libx264, all-intra, yuv420p, faststart) so
// the output is frame-exact-seekable for downstream consumers.

struct Mp4Encoder {
    child: Child,
    stdin: Option<ChildStdin>,
    path: PathBuf,
}

impl Mp4Encoder {
    fn spawn(output: &PathBuf) -> std::io::Result<Self> {
        let size = format!("{GBA_W}x{GBA_H}");
        let fps = format!("{GBA_FPS}");
        let mut child = Command::new("ffmpeg")
            .args([
                "-y", "-hide_banner", "-loglevel", "error",
                "-f", "rawvideo",
                "-pix_fmt", "rgba",
                "-s", &size,
                "-framerate", &fps,
                "-i", "-",
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
            .stderr(Stdio::inherit())
            .spawn()?;
        let stdin = child.stdin.take();
        Ok(Self { child, stdin, path: output.clone() })
    }

    fn write(&mut self, rgba: &[u8]) -> std::io::Result<()> {
        if let Some(s) = self.stdin.as_mut() {
            s.write_all(rgba)
        } else {
            Ok(())
        }
    }

    fn finish(mut self) -> std::io::Result<()> {
        drop(self.stdin.take()); // close stdin → ffmpeg flushes + exits
        let status = self.child.wait()?;
        if !status.success() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("ffmpeg failed for {}: status {status}", self.path.display()),
            ));
        }
        Ok(())
    }
}
