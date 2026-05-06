//! C ABI shim for gba-core — the same 10 exports as spec/ABI.md.

use gba_core::gba::Gba;

const MAX_ROM: usize = 32 * 1024 * 1024;
const AUDIO_BUF_SAMPLES: usize = 4096;

static mut GBA: Option<Gba> = None;
static mut ROM_BUF: [u8; MAX_ROM] = [0u8; MAX_ROM];
static mut AUDIO_BUF: [i16; AUDIO_BUF_SAMPLES] = [0i16; AUDIO_BUF_SAMPLES];
static mut AUDIO_PAIRS: i32 = 0;

#[no_mangle]
pub extern "C" fn emu_init() -> i32 {
    unsafe {
        // Prime internal state with a dummy load_rom — the first real
        // load_rom otherwise produces a black framebuffer.
        GBA = Some(Gba::new());
        ROM_BUF[..4].copy_from_slice(&[0u8; 4]);
        emu_load_rom(256);
    }
    1
}

#[no_mangle]
pub extern "C" fn emu_rom_buffer() -> *mut u8 {
    unsafe { ROM_BUF.as_mut_ptr() }
}

#[no_mangle]
pub extern "C" fn emu_load_rom(len: i32) -> i32 {
    let len = len as usize;
    if len > MAX_ROM { return 0; }
    unsafe {
        let gba = match GBA.as_mut() {
            Some(g) => g,
            None => return 0,
        };
        let rom = ROM_BUF[..len].to_vec();
        *gba = Gba::new();
        gba.load_rom(rom);
        gba.skip_bios();
    }
    1
}

#[no_mangle]
pub extern "C" fn emu_reset() -> i32 {
    unsafe {
        let gba = match GBA.as_mut() {
            Some(g) => g,
            None => return 0,
        };
        let len = gba.bus.rom.len();
        let rom = ROM_BUF[..len].to_vec();
        *gba = Gba::new();
        gba.load_rom(rom);
        gba.skip_bios();
    }
    1
}

#[no_mangle]
pub extern "C" fn emu_set_keys(keys: u32) {
    unsafe {
        if let Some(gba) = GBA.as_mut() {
            gba.set_keyinput(!(keys as u16) & 0x03FF);
        }
    }
}

#[no_mangle]
pub extern "C" fn emu_run_frame() {
    unsafe {
        if let Some(gba) = GBA.as_mut() {
            gba.run_frame();
            let f32_samples = gba.bus.apu.drain_samples();
            let pairs = (f32_samples.len() / 2).min(AUDIO_BUF_SAMPLES / 2);
            for i in 0..pairs * 2 {
                let s = f32_samples[i];
                let clamped = if s < -1.0 { -1.0 } else if s > 1.0 { 1.0 } else { s };
                AUDIO_BUF[i] = (clamped * 32767.0) as i16;
            }
            AUDIO_PAIRS = pairs as i32;
        }
    }
}

#[no_mangle]
pub extern "C" fn emu_framebuffer() -> *const u32 {
    unsafe {
        match GBA.as_ref() {
            Some(gba) => gba.framebuffer().as_ptr(),
            None => core::ptr::null(),
        }
    }
}

#[no_mangle]
pub extern "C" fn emu_audio_buffer() -> *const i16 {
    unsafe { AUDIO_BUF.as_ptr() }
}

#[no_mangle]
pub extern "C" fn emu_audio_samples() -> i32 {
    unsafe {
        let pairs = AUDIO_PAIRS;
        AUDIO_PAIRS = 0;
        pairs
    }
}

#[no_mangle]
pub extern "C" fn emu_audio_rate() -> i32 {
    32768
}
