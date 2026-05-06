//! Black-box reference oracle.
//!
//! The model calls this to observe reference behavior on any ROM + input
//! sequence. Same 10-function ABI that the grader evaluates, but the model
//! can't see the source — just the binary.
//!
//! Usage:
//!   oracle run <rom> <frames> [--replay <file>] [--dump-frames <dir>] [--dump-audio <file>]
//!   oracle info
//!
//! Rate limit: the environment tracks total frames executed. The model is
//! told its budget upfront (e.g. 500,000 frames). Exceeding the limit
//! causes the oracle to exit with a clear error, not a silent failure.

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::exit;

use grader::wasm_candidate::WasmCandidate;
use lockstep::media::write_wav;
use lockstep::{InputReplay, Reference, GBA_PIXELS, GBA_W, GBA_H};

// Default path to the Mesen wasm inside the services container.
// quickstart/services/Dockerfile copies reference/mesen.wasm here —
// the exact same binary the grader uses as its reference. Override
// with ORACLE_MESEN_WASM.
const DEFAULT_MESEN_WASM: &str = "/opt/gba-eval/mesen.wasm";

fn load_mesen_reference(rom: &[u8]) -> Box<dyn Reference> {
    let wasm_path = env::var("ORACLE_MESEN_WASM")
        .unwrap_or_else(|_| DEFAULT_MESEN_WASM.to_string());
    let wasm_bytes = fs::read(&wasm_path).unwrap_or_else(|e| {
        eprintln!(
            "error: read mesen wasm {wasm_path}: {e}\n\
             Set ORACLE_MESEN_WASM or copy reference/mesen.wasm to {DEFAULT_MESEN_WASM}."
        );
        exit(1);
    });
    // 1B/frame leaves ~3× headroom over Mesen's observed worst case
    // without being so large that real infinite loops escape detection.
    const MESEN_FUEL_PER_FRAME: u64 = 1_000_000_000;
    const MESEN_FUEL_LOAD_ROM: u64 = 2_000_000_000;
    let mut cand = WasmCandidate::new(
        &wasm_bytes, "mesen".to_string(),
        MESEN_FUEL_PER_FRAME, MESEN_FUEL_LOAD_ROM,
    ).unwrap_or_else(|e| {
        eprintln!("error: mesen wasm init: {e:?}");
        exit(1);
    });
    cand.load_rom(rom).unwrap_or_else(|e| {
        eprintln!("error: mesen wasm load_rom: {e:?}");
        exit(1);
    });
    Box::new(cand)
}

// ─────────────────────────────────────────────────────────────────────────
// Usage tracking (informational, no limit)
// ─────────────────────────────────────────────────────────────────────────

const USAGE_FILE: &str = ".oracle_frames_used";

fn read_usage() -> u64 {
    fs::read_to_string(USAGE_FILE)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn record_usage(frames: u64) {
    let used = read_usage();
    let _ = fs::write(USAGE_FILE, format!("{}\n", used + frames));
}

// ─────────────────────────────────────────────────────────────────────────
// PPM output
// ─────────────────────────────────────────────────────────────────────────

fn write_ppm(path: &Path, fb: &[u32; GBA_PIXELS]) {
    let mut f = fs::File::create(path).unwrap();
    write!(f, "P6\n{GBA_W} {GBA_H}\n255\n").unwrap();
    let mut rgb = vec![0u8; GBA_PIXELS * 3];
    for i in 0..GBA_PIXELS {
        let px = fb[i];
        rgb[i * 3]     = (px & 0xFF) as u8;
        rgb[i * 3 + 1] = ((px >> 8) & 0xFF) as u8;
        rgb[i * 3 + 2] = ((px >> 16) & 0xFF) as u8;
    }
    f.write_all(&rgb).unwrap();
}

// ─────────────────────────────────────────────────────────────────────────
// Commands
// ─────────────────────────────────────────────────────────────────────────

fn cmd_help() {
    println!("GBA Eval Oracle — black-box reference emulator");
    println!();
    println!("The oracle runs a reference GBA emulator on any ROM and returns the");
    println!("exact framebuffer and audio output. Use it to compare your emulator's");
    println!("output against the reference and iterate until they match.");
    println!();
    println!("COMMANDS:");
    println!();
    println!("  oracle help");
    println!("      Show this help message.");
    println!();
    println!("  oracle info");
    println!("      Print JSON with your frame budget, pixel format, resolution,");
    println!("      and audio format. Run this first to see your remaining budget.");
    println!();
    println!("  oracle run <rom> <frames> [options]");
    println!("      Run the reference emulator on <rom> for <frames> frames.");
    println!("      Outputs a JSON summary to stdout.");
    println!();
    println!("      Options:");
    println!("        --replay <file>       Feed a recorded input sequence");
    println!("        --dump-frames <dir>   Write each frame as a PPM image");
    println!("        --dump-audio <file>   Write all audio as a WAV file");
    println!();
    println!("EXAMPLES:");
    println!();
    println!("  # See what the reference renders for armwrestler (60 frames)");
    println!("  oracle run dev-roms/armwrestler.gba 60 --dump-frames /tmp/ref");
    println!();
    println!("  # Run with inputs and capture audio");
    println!("  oracle run dev-roms/celeste-classic.gba 300 \\");
    println!("      --replay my-inputs.txt --dump-audio /tmp/ref.wav");
    println!();
    println!("  # Check remaining budget");
    println!("  oracle info");
    println!();
    println!("USAGE:");
    println!("  `oracle info` shows how many frames you've used so far.");
    println!("  There is no hard limit — use the oracle as much as you need.");
    println!();
    println!("PIXEL FORMAT:");
    println!("  240x160, 32-bit ABGR (0xAABBGGRR). PPM output is RGB.");
    println!("  Matches emu_framebuffer() from spec/ABI.md.");
    println!();
    println!("REPLAY FORMAT:");
    println!("  Text: one line per event, `<frame> <keys_hex>`.");
    println!("  Keys are active-high: bit 0=A, 1=B, 2=Select, 3=Start,");
    println!("  4=Right, 5=Left, 6=Up, 7=Down, 8=R, 9=L.");
    println!("  State persists between events (last keys stay held).");
}

fn cmd_info() {
    let used = read_usage();
    let info = serde_json::json!({
        "abi_version": 1,
        "reference": "mesen",
        "frames_used": used,
        "pixel_format": "0xAABBGGRR (little-endian: R,G,B,A bytes)",
        "resolution": { "width": GBA_W, "height": GBA_H },
        "audio_format": "i16 stereo interleaved (L,R,L,R,...)",
        "audio_rate": "32768 Hz (may be 65536 for some games via SOUNDBIAS)",
    });
    println!("{}", serde_json::to_string_pretty(&info).unwrap());
}

fn cmd_run(
    rom_path: PathBuf,
    n_frames: u32,
    replay_path: Option<PathBuf>,
    dump_frames: Option<PathBuf>,
    dump_audio: Option<PathBuf>,
) {
    // Track usage (informational)

    // Load replay
    let inputs = match replay_path {
        Some(ref p) => InputReplay::from_file(p).unwrap_or_else(|e| {
            eprintln!("error: read replay {}: {e}", p.display());
            exit(1);
        }),
        None => InputReplay::new(),
    };

    // Load ROM
    let rom = fs::read(&rom_path).unwrap_or_else(|e| {
        eprintln!("error: read {}: {e}", rom_path.display());
        exit(1);
    });

    // Init reference — mesen_candidate.wasm under wasmtime.
    let mut reference = load_mesen_reference(&rom);

    // Create output dirs
    if let Some(ref dir) = dump_frames {
        fs::create_dir_all(dir).unwrap();
    }

    // Burn boot frames
    for _ in 0..reference.boot_frames() {
        reference.run_frame();
    }

    // Run
    let mut audio: Vec<i16> = Vec::new();
    let audio_rate = reference.audio_rate();

    for frame in 0..n_frames {
        let keys = inputs.keys_at(frame);
        reference.set_keys(keys);
        reference.run_frame();

        if let Some(ref dir) = dump_frames {
            let path = dir.join(format!("frame_{frame:05}.ppm"));
            write_ppm(&path, reference.framebuffer());
        }

        let samples = reference.drain_audio();
        audio.extend_from_slice(&samples);
    }

    // Write audio
    if let Some(ref path) = dump_audio {
        write_wav(path, &audio, audio_rate).unwrap_or_else(|e| {
            eprintln!("error: write wav {}: {e}", path.display());
            exit(1);
        });
        eprintln!("audio: {} stereo pairs @ {} Hz → {}", audio.len() / 2, audio_rate, path.display());
    }

    record_usage(n_frames as u64);

    let summary = serde_json::json!({
        "frames_executed": n_frames,
        "frames_used_total": read_usage(),
        "audio_rate": audio_rate,
        "audio_pairs": audio.len() / 2,
    });
    println!("{}", serde_json::to_string(&summary).unwrap());
}

// ─────────────────────────────────────────────────────────────────────────
// CLI
// ─────────────────────────────────────────────────────────────────────────

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

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();

    if args.is_empty() {
        cmd_help();
        exit(2);
    }

    match args[0].as_str() {
        "help" | "--help" | "-h" => cmd_help(),
        "info" => cmd_info(),
        "run" => {
            let mut rest: Vec<String> = args[1..].to_vec();

            let replay = parse_flag(&mut rest, "--replay").map(PathBuf::from);
            let dump_frames = parse_flag(&mut rest, "--dump-frames").map(PathBuf::from);
            let dump_audio = parse_flag(&mut rest, "--dump-audio").map(PathBuf::from);

            let rom_path: PathBuf = match rest.first() {
                Some(p) => p.into(),
                None => {
                    eprintln!("error: missing <rom> argument");
                    exit(2);
                }
            };
            let n_frames: u32 = rest.get(1)
                .map(|s| s.parse().expect("invalid frame count"))
                .unwrap_or(60);

            cmd_run(rom_path, n_frames, replay, dump_frames, dump_audio);
        }
        other => {
            eprintln!("error: unknown command '{other}'. Use 'info' or 'run'.");
            exit(2);
        }
    }
}
