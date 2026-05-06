//! Load an arbitrary wasm module conforming to spec/ABI.md and drive it
//! through the `Reference` trait. Uses wasmtime so a `run_frame()` that
//! infinite-loops traps cleanly via the per-call fuel budget.
//!
//! ## The emscripten import problem
//!
//! Emscripten-built modules import a pile of `env::*` and
//! `wasi_snapshot_preview1::*` functions even when the C code doesn't
//! use them — emscripten's libc startup touches malloc, stdio, mmap,
//! etc. We stub them all to traps. If a candidate actually CALLS one
//! (say, `printf`), it traps, we catch it, record the failure. The
//! ABI says "no file I/O after load_rom"; this enforces it.
//!
//! Exception: `emscripten_notify_memory_growth` gets a real no-op stub
//! since memory growth is allowed (ALLOW_MEMORY_GROWTH=1).
//!
//! ## Memory layout
//!
//! The ABI's exported pointers (`emu_framebuffer()` etc.) are offsets
//! into wasm linear memory. We hold a `Memory` handle and read through
//! it. Memory growth invalidates host-side slices — we don't cache
//! them, we re-derive on every `framebuffer()` call.

use anyhow::{anyhow, bail, Context, Result};
use wasmtime::{Engine, Instance, Linker, Memory, Module, Store, TypedFunc};

use lockstep::{Reference, GBA_PIXELS};

const MAX_ROM: usize = 32 * 1024 * 1024;

// Fuel budgets come from grader.yaml via GraderConfig — see main.rs
// for defaults. load_rom needs a huge budget because it scans the
// full ROM for save strings + runs init.

pub struct WasmCandidate {
    label: String,
    store: Store<()>,
    memory: Memory,

    // Cached export handles. Resolved once at init; calling is cheap.
    // The ABI uses i32 for sizes/returns — wasm32 has no native u32 in
    // its type system, so even uint32_t parameters surface as i32.
    rom_buf_ptr: u32,
    fb_ptr: u32,
    f_load_rom: TypedFunc<i32, i32>,
    f_set_keys: TypedFunc<i32, ()>,
    f_run_frame: TypedFunc<(), ()>,
    f_framebuffer: TypedFunc<(), i32>,
    f_audio_buffer: TypedFunc<(), i32>,
    f_audio_samples: TypedFunc<(), i32>,
    cached_audio_rate: u32,
    /// Candidate's self-declared boot-frame cost (from the optional
    /// `emu_boot_frames()` export; default 0).
    cached_boot_frames: u32,

    // Owned framebuffer copy; wasmtime's Memory borrows the store.
    fb_local: Box<[u32; GBA_PIXELS]>,

    fuel_per_frame: u64,
    fuel_load_rom: u64,
}

impl WasmCandidate {
    pub fn new(wasm_bytes: &[u8], label: String, fuel_per_frame: u64, fuel_load_rom: u64) -> Result<Self> {
        // 0 is almost certainly a yaml typo (`fuel_per_frame:` with no
        // value parses as 0). Silently falling back to defaults masks
        // the config mistake until grading produces wrong results.
        if fuel_per_frame == 0 {
            bail!("grader config: fuel_per_frame must be > 0 (got 0 — typo in grader.yaml?)");
        }
        if fuel_load_rom == 0 {
            bail!("grader config: fuel_load_rom must be > 0 (got 0 — typo in grader.yaml?)");
        }
        // Fuel needs config. Everything else is default — cranelift,
        // no parallel compilation (single module, doesn't help).
        let mut config = wasmtime::Config::new();
        config.consume_fuel(true);
        let engine = Engine::new(&config)?;

        let module = Module::new(&engine, wasm_bytes)
            .context("compiling candidate wasm — invalid module?")?;

        let mut store = Store::new(&engine, ());
        store.set_fuel(fuel_per_frame).context("initial fuel")?;

        // ─── Stub every import to a trap ─────────────────────────────────
        // We don't know what emscripten will ask for (it varies by version
        // and by which libc paths got DCE'd). So: walk the module's import
        // list, generate a trap stub matching each signature.
        let mut linker = Linker::new(&engine);

        for import in module.imports() {
            let module_name = import.module();
            let field_name = import.name();

            match import.ty() {
                wasmtime::ExternType::Func(fty) => {
                    // Memory growth notification: real no-op. Emscripten
                    // calls this after every memory.grow so JS can
                    // re-derive HEAP* views. We don't have JS views.
                    if field_name == "emscripten_notify_memory_growth" {
                        linker.func_wrap(module_name, field_name, |_: i32| {})?;
                        continue;
                    }

                    // WASI stubs. Emscripten's libc startup pokes at
                    // WASI for things like std::random_device seeding
                    // and std::chrono::system_clock::now(). Stub them
                    // to write zeros and return success so they don't
                    // affect determinism.
                    if module_name == "wasi_snapshot_preview1" {
                        match field_name {
                            // random_get(buf, len) — fill with zeros.
                            "random_get" => {
                                linker.func_wrap(
                                    module_name, field_name,
                                    |mut caller: wasmtime::Caller<'_, ()>, ptr: i32, len: i32| -> i32 {
                                        zero_fill(&mut caller, ptr, len)
                                    },
                                )?;
                                continue;
                            }
                            // clock_time_get(clock_id, precision, out_ptr) — write 0.
                            "clock_time_get" => {
                                linker.func_wrap(
                                    module_name, field_name,
                                    |mut caller: wasmtime::Caller<'_, ()>, _id: i32, _prec: i64, out: i32| -> i32 {
                                        zero_fill(&mut caller, out, 8)
                                    },
                                )?;
                                continue;
                            }
                            // clock_res_get(clock_id, out_ptr) — write 0.
                            "clock_res_get" => {
                                linker.func_wrap(
                                    module_name, field_name,
                                    |mut caller: wasmtime::Caller<'_, ()>, _id: i32, out: i32| -> i32 {
                                        zero_fill(&mut caller, out, 8)
                                    },
                                )?;
                                continue;
                            }
                            // environ_get / environ_sizes_get — empty env.
                            "environ_sizes_get" => {
                                linker.func_wrap(
                                    module_name, field_name,
                                    |mut caller: wasmtime::Caller<'_, ()>, count_out: i32, size_out: i32| -> i32 {
                                        zero_fill(&mut caller, count_out, 4);
                                        zero_fill(&mut caller, size_out, 4);
                                        0
                                    },
                                )?;
                                continue;
                            }
                            "environ_get" => {
                                linker.func_wrap(
                                    module_name, field_name,
                                    |_caller: wasmtime::Caller<'_, ()>, _environ: i32, _buf: i32| -> i32 {
                                        0
                                    },
                                )?;
                                continue;
                            }
                            // proc_exit — hard trap. A candidate should not exit.
                            _ => {}
                        }
                    }

                    // Everything else: trap on call. linker.func_new takes
                    // a runtime FuncType (we don't know the signature at
                    // compile time — emscripten's import list varies by
                    // version and by which libc paths survived DCE).
                    let trap_msg = format!(
                        "candidate called forbidden import: {module_name}::{field_name}"
                    );
                    linker.func_new(
                        module_name,
                        field_name,
                        fty.clone(),
                        move |_caller, _params, _results| {
                            Err(anyhow!("{trap_msg}").into())
                        },
                    )?;
                }
                wasmtime::ExternType::Memory(_)
                | wasmtime::ExternType::Table(_)
                | wasmtime::ExternType::Global(_) => {
                    // Emscripten with MODULARIZE shouldn't import these
                    // (it exports its own memory). If a candidate does,
                    // that's a build misconfiguration — fail loudly.
                    bail!(
                        "candidate imports a non-function: {module_name}::{field_name}. \
                         The ABI requires the module to own its memory \
                         (no -s IMPORTED_MEMORY)."
                    );
                }
            }
        }

        let instance = linker.instantiate(&mut store, &module)
            .context("instantiating — likely a start-function trap")?;

        // ─── Resolve ABI exports ─────────────────────────────────────────
        let memory = instance.get_memory(&mut store, "memory")
            .ok_or_else(|| anyhow!("candidate has no exported `memory`"))?;

        // emscripten's _initialize runs static constructors. Without it,
        // C++ candidates have uninitialized globals. ABI doesn't mandate
        // it (a Rust candidate won't have one), so optional.
        if let Ok(init_rt) = instance.get_typed_func::<(), ()>(&mut store, "_initialize") {
            store.set_fuel(fuel_per_frame)?;
            init_rt.call(&mut store, ()).context("_initialize trapped")?;
        }

        let f_init = get_typed::<(), i32>(&instance, &mut store, "emu_init")?;
        let f_rom_buffer = get_typed::<(), i32>(&instance, &mut store, "emu_rom_buffer")?;
        let f_load_rom = get_typed::<i32, i32>(&instance, &mut store, "emu_load_rom")?;
        let f_set_keys = get_typed::<i32, ()>(&instance, &mut store, "emu_set_keys")?;
        let f_run_frame = get_typed::<(), ()>(&instance, &mut store, "emu_run_frame")?;
        let f_framebuffer = get_typed::<(), i32>(&instance, &mut store, "emu_framebuffer")?;

        // emu_reset is required but the grader re-loads instead. Verify existence.
        if instance.get_func(&mut store, "emu_reset").is_none() {
            bail!("candidate missing required export: emu_reset");
        }

        // Audio exports — resolve handles now, call per-frame in drain_audio.
        let f_audio_buffer = get_typed::<(), i32>(&instance, &mut store, "emu_audio_buffer")?;
        let f_audio_samples = get_typed::<(), i32>(&instance, &mut store, "emu_audio_samples")?;
        let f_audio_rate = get_typed::<(), i32>(&instance, &mut store, "emu_audio_rate")?;

        // Optional boot-frames export. Present on candidates that wrap a
        // pre-existing emulator with its own warmup cost. Missing → default
        // 0 per the ABI (a conformant `load_rom()` leaves you at frame 0).
        let f_boot_frames = instance.get_typed_func::<(), i32>(&mut store, "emu_boot_frames").ok();

        // ─── Call init, cache stable pointers ────────────────────────────
        // emu_init internally calls emu_load_rom(256) as a warmup, so it
        // needs the full load_rom fuel budget, not just per-frame.
        store.set_fuel(fuel_load_rom)?;
        let ok = f_init.call(&mut store, ())
            .context("emu_init trapped")?;
        if ok == 0 {
            bail!("emu_init returned 0 (failure)");
        }

        let rom_buf_ptr = f_rom_buffer.call(&mut store, ())? as u32;
        let fb_ptr = f_framebuffer.call(&mut store, ())? as u32;
        let cached_audio_rate = f_audio_rate.call(&mut store, ()).unwrap_or(32768) as u32;
        let cached_boot_frames = match &f_boot_frames {
            Some(f) => f.call(&mut store, ()).unwrap_or(0).max(0) as u32,
            None => 0,
        };

        // Sanity: framebuffer must fit in current memory. (rom_buffer we
        // check at load_rom time, since memory might grow between now
        // and then.)
        let mem_size = memory.data_size(&store);
        let fb_end = fb_ptr as usize + GBA_PIXELS * 4;
        if fb_end > mem_size {
            bail!(
                "emu_framebuffer() = {fb_ptr:#x} but memory is only {mem_size} bytes \
                 (framebuffer would end at {fb_end:#x})"
            );
        }

        Ok(Self {
            label,
            store,
            memory,
            rom_buf_ptr,
            fb_ptr,
            f_load_rom,
            f_set_keys,
            f_run_frame,
            f_framebuffer,
            f_audio_buffer,
            f_audio_samples,
            cached_audio_rate,
            cached_boot_frames,
            fb_local: Box::new([0u32; GBA_PIXELS]),
            fuel_per_frame,
            fuel_load_rom,
        })
    }

    pub fn load_rom(&mut self, rom: &[u8]) -> Result<()> {
        if rom.len() > MAX_ROM {
            bail!("ROM too large: {} > {MAX_ROM}", rom.len());
        }

        // Memory may have grown since init — re-check bounds against the
        // CURRENT size, not a cached one.
        let mem_size = self.memory.data_size(&self.store);
        let rom_end = self.rom_buf_ptr as usize + rom.len();
        if rom_end > mem_size {
            bail!(
                "emu_rom_buffer() = {:#x} but memory is {mem_size} bytes; \
                 writing {} bytes would overflow",
                self.rom_buf_ptr, rom.len()
            );
        }

        self.memory
            .write(&mut self.store, self.rom_buf_ptr as usize, rom)
            .context("writing ROM into wasm memory")?;

        self.store.set_fuel(self.fuel_load_rom)?;
        let ok = self.f_load_rom.call(&mut self.store, rom.len() as i32)
            .context("emu_load_rom trapped")?;
        if ok == 0 {
            bail!("emu_load_rom returned 0 (failure)");
        }

        // Re-query framebuffer pointer — load_rom may recreate the emulator
        // instance, allocating a new framebuffer at a different address.
        self.store.set_fuel(self.fuel_per_frame)?;
        let new_fb = self.f_framebuffer.call(&mut self.store, ())? as u32;
        if new_fb != self.fb_ptr {
            eprintln!("[{}] framebuffer moved: {:#x} → {:#x}", self.label, self.fb_ptr, new_fb);
            self.fb_ptr = new_fb;
        }

        Ok(())
    }
}

impl Reference for WasmCandidate {
    fn name(&self) -> &str {
        &self.label
    }

    fn run_frame(&mut self) {
        // Refuel every frame. set_fuel REPLACES, not adds — we want a
        // fresh budget each time, not accumulating leftover.
        if self.store.set_fuel(self.fuel_per_frame).is_err() {
            return; // store config broken? subsequent frames will be black
        }

        // Trap (out-of-fuel, unreachable, divide-by-zero, called a stub
        // import) → leave fb_local at its previous value. The frame will
        // diverge from the reference and tank the score, which is correct.
        // We don't propagate the error — `Reference::run_frame` is
        // infallible by design (lockstep() doesn't want to handle
        // per-frame errors). One bad frame ≠ abort the testcase.
        if let Err(e) = self.f_run_frame.call(&mut self.store, ()) {
            eprintln!("[{}] run_frame trap: {e}", self.label);
            return;
        }

        // Copy framebuffer out. Memory growth could have happened during
        // run_frame; data() reflects the current memory.
        let mem = self.memory.data(&self.store);
        let start = self.fb_ptr as usize;
        let end = start + GBA_PIXELS * 4;
        if end > mem.len() {
            // Shouldn't happen — fb is static and memory only grows. But
            // a candidate that returns a bogus pointer would hit this.
            eprintln!("[{}] framebuffer out of bounds after growth", self.label);
            return;
        }
        // Wasm linear memory is byte-addressed; the framebuffer is u32 in
        // little-endian. On an LE host (everything we run on), this is a
        // straight memcpy. bytemuck would make this prettier but it's one
        // line and we don't have the dep.
        let src = &mem[start..end];
        let dst = bytemuck_u32(&mut *self.fb_local);
        dst.copy_from_slice(src);
    }

    fn set_keys(&mut self, keys: u16) {
        // Don't refuel — set_keys is trivial, if it eats 100M fuel
        // something is very wrong and we WANT the next run_frame to trap.
        let _ = self.f_set_keys.call(&mut self.store, keys as i32);
    }

    fn framebuffer(&self) -> &[u32; GBA_PIXELS] {
        &self.fb_local
    }

    fn drain_audio(&mut self) -> Vec<i16> {
        // emu_audio_samples() drains — returns count and resets write head.
        // Must call AFTER run_frame() and BEFORE next run_frame().
        let pairs = match self.f_audio_samples.call(&mut self.store, ()) {
            Ok(n) if n > 0 => n as usize,
            _ => return Vec::new(),
        };
        let ptr = match self.f_audio_buffer.call(&mut self.store, ()) {
            Ok(p) => p as usize,
            _ => return Vec::new(),
        };
        let byte_count = pairs * 2 * 2; // stereo × i16
        let mem = self.memory.data(&self.store);
        if ptr + byte_count > mem.len() {
            return Vec::new();
        }
        // Read i16 LE from wasm memory
        let src = &mem[ptr..ptr + byte_count];
        src.chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect()
    }

    fn audio_rate(&self) -> u32 {
        self.cached_audio_rate
    }

    fn boot_frames(&self) -> u32 {
        self.cached_boot_frames
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────

fn get_typed<P, R>(
    instance: &Instance,
    store: &mut Store<()>,
    name: &str,
) -> Result<TypedFunc<P, R>>
where
    P: wasmtime::WasmParams,
    R: wasmtime::WasmResults,
{
    instance
        .get_typed_func(store, name)
        .with_context(|| format!("missing or wrong-signature export: `{name}`"))
}

/// Fill a wasm-memory byte range with zeros. Returns WASI __WASI_ERRNO_SUCCESS
/// on success, __WASI_ERRNO_INVAL on out-of-bounds. Used to stub random_get,
/// clock_time_get, and other WASI calls whose only effect (for our purposes)
/// is to write a fixed-size result buffer.
fn zero_fill(caller: &mut wasmtime::Caller<'_, ()>, ptr: i32, len: i32) -> i32 {
    let Some(mem) = caller.get_export("memory").and_then(|e| e.into_memory()) else {
        return 28;
    };
    let data = mem.data_mut(caller);
    let start = ptr as usize;
    let end = start.saturating_add(len.max(0) as usize);
    if end <= data.len() {
        for b in &mut data[start..end] { *b = 0; }
        0
    } else {
        28
    }
}

/// Reinterpret &mut [u32; N] as &mut [u8] for copy_from_slice.
/// Safe: u32 has no padding, alignment of the source ([u8]) is 1.
fn bytemuck_u32(s: &mut [u32; GBA_PIXELS]) -> &mut [u8] {
    // SAFETY: u32 is plain-old-data, no padding, no invalid bit patterns.
    // Slice length is exact: GBA_PIXELS u32s = GBA_PIXELS*4 u8s.
    unsafe {
        std::slice::from_raw_parts_mut(s.as_mut_ptr() as *mut u8, GBA_PIXELS * 4)
    }
}
