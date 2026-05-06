//! Mesen2 reference. Native, statically linked from `libmesen.a`.
//!
//! The fork at `third_party/mesen` (branch `gba-headless`) is upstream
//! Mesen with everything non-GBA deleted — NES/SNES/GB/PCE cores,
//! netplay, AVI recording, threads, debugger UI, NTSC filter. ~350K LOC
//! removed. `Core/GBA/` is +5/-34 lines vs upstream `fabc9a6`, all
//! non-emulation:
//!
//!   GbaPpu.cpp:186     UpdateFrame(frame, true, false) — sync decode
//!                      instead of queuing to a thread. Same DecodeFrame()
//!                      runs, just inline.
//!   GbaConsole.cpp     NTSC filter switch deleted (post-processing only)
//!   GbaDebugger.cpp    dynamic_cast → static_cast (-fno-rtti, never runs)
//!
//! CPU, memory manager, DMA, timers, prefetch, APU, cart: byte-identical.
//! See reference/FORK.md for the full diff.
//!
//! Single instance only — Mesen has process-global state (`_emu` in the
//! interop layer). The grader creates one `Mesen`, reuses it across all
//! testcases via `load_rom()`.

use crate::{Reference, GBA_PIXELS};

// reference/mesen_step.cpp — same source compiles for both this (native,
// via cc) and the browser (wasm, via emcc). Same shim, same BIOS bytes.
unsafe extern "C" {
    pub fn mesen_init() -> i32;
    pub fn mesen_rom_buffer() -> *mut u8;
    pub fn mesen_load_rom(len: i32) -> i32;
    fn mesen_set_keys(keys: u32);
    fn mesen_run_frame();
    fn mesen_framebuffer() -> *const u32;
    fn mesen_audio_buffer() -> *const i16;
    fn mesen_audio_samples() -> i32;
    fn mesen_audio_rate() -> i32;
    pub fn mesen_master_clock() -> u64;
    pub fn mesen_pc() -> u32;
    pub fn mesen_exec_one() -> u64;
}

pub struct Mesen {
    _no_send: std::marker::PhantomData<*const ()>,
}

impl Mesen {
    /// One-time init. Call once per process.
    pub fn init() -> Result<Self, &'static str> {
        // SAFETY: idempotent — calling again tears down and reinits.
        if unsafe { mesen_init() } == 0 {
            return Err("mesen_init failed");
        }
        Ok(Self { _no_send: std::marker::PhantomData })
    }

    /// Load a ROM. Reuses the existing instance — call this between
    /// testcases instead of dropping and recreating.
    pub fn load_rom(&mut self, rom: &[u8]) -> Result<(), String> {
        const MAX_ROM: usize = 32 * 1024 * 1024;
        if rom.len() > MAX_ROM {
            return Err(format!("ROM too large: {} > {MAX_ROM}", rom.len()));
        }
        // SAFETY: rom_buffer is a 32MB static array; we write within bounds.
        unsafe {
            std::ptr::copy_nonoverlapping(rom.as_ptr(), mesen_rom_buffer(), rom.len());
        }
        if unsafe { mesen_load_rom(rom.len() as i32) } == 0 {
            return Err("mesen_load_rom failed".into());
        }
        Ok(())
    }
}

impl Reference for Mesen {
    fn name(&self) -> &str {
        "Mesen"
    }

    fn run_frame(&mut self) {
        unsafe { mesen_run_frame() };
    }

    fn set_keys(&mut self, keys: u16) {
        unsafe { mesen_set_keys(keys as u32) };
    }

    fn framebuffer(&self) -> &[u32; GBA_PIXELS] {
        // SAFETY: static buffer, lives forever. mesen_step.cpp's
        // capture_frame() already swizzled ARGB → ABGR and disabled
        // GbaAdjustColors (LCD gamma) + BlendFrames, so this is raw
        // 5-bit-expanded GBA output in our format.
        unsafe { &*(mesen_framebuffer() as *const [u32; GBA_PIXELS]) }
    }

    fn drain_audio(&mut self) -> Vec<i16> {
        let pairs = unsafe { mesen_audio_samples() };
        if pairs <= 0 {
            return Vec::new();
        }
        let ptr = unsafe { mesen_audio_buffer() };
        let count = pairs as usize * 2; // stereo: L,R per pair
        // SAFETY: static buffer, valid until next run_frame. We copy out.
        unsafe { std::slice::from_raw_parts(ptr, count) }.to_vec()
    }

    fn audio_rate(&self) -> u32 {
        let r = unsafe { mesen_audio_rate() };
        if r > 0 { r as u32 } else { 32768 }
    }

    fn boot_frames(&self) -> u32 {
        // Mesen's SkipBootScreen leaves ~5 frames of internal post-reset
        // warmup before the first game pixel; lockstep aligns by
        // burning these.
        6
    }
}
