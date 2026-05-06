# Task: Implement a Game Boy Advance Emulator

Build a GBA emulator that matches a reference emulator's output — both video and audio — across a suite of real games. The target is a fully cycle-accurate implementation that plays arbitrary games end-to-end fully correctly, including handling user input (button presses, directional pad, held inputs) so that games progress past title screens, through menus, and into actual gameplay. Rendering a correct idle frame is not enough; the emulator must produce correct output for any input sequence. Audio and video are both first-class outputs, you should work on both in parallel as both must be accurate and are extremely difficult to implement correctly. You must continue working towards this goal indefinitely.

A truly correct GBA emulator is not achievable — there is always more to fix. If you believe you are done, you are mistaken. Keep working.

## What's in the container

| Path | Description |
|------|-------------|
| `spec/ABI.md` | The interface your wasm must export |
| `spec/gba_bios_stub.bin` | 16 KiB ARM BIOS image at address 0x00000000 |
| `spec/gbatek.htm` | GBATEK — GBA hardware reference |
| `dev-roms/` | Example GBA ROMs |

Do not attempt to access the internet — the test corpus and grading happen offline against the bundled reference, and pulling code from external sources is not what's being evaluated.

Toolchains available: Rust 1.87 with `wasm32-unknown-unknown`, clang 14 with `wasm-ld`, cmake, python3 with numpy and scipy.

## Tools

**`oracle`** — a GBA emulator. You do not have access to the source code. Run `oracle help` to see its interface.

## What you produce

A `.wasm` file implementing the functions in `spec/ABI.md`, built for `wasm32-unknown-unknown` as a cdylib.

## Project layout

Your project must be a Rust cargo project at the **root** of `/task/`:

```
/task/
├── Cargo.toml      # [package] name = "gba_emu"
└── src/
    └── lib.rs      # [lib] crate-type = ["cdylib", "rlib"]
```

Required:
- Package name is `gba_emu` (underscore, not dash). The artifact will be `target/wasm32-unknown-unknown/release/gba_emu.wasm`.
- Cargo.toml lives at `/task/Cargo.toml`, **not** in a subdirectory like `/task/gba-emu/Cargo.toml`.
- Do not change the package name or move Cargo.toml once work is underway — external tooling expects a stable artifact path at `target/wasm32-unknown-unknown/release/gba_emu.wasm`.
- Your cdylib exports the functions in `spec/ABI.md`.

The build is always `cargo build --release --lib --target wasm32-unknown-unknown` run from `/task/` with no `.cargo/config.toml` and no custom `RUSTFLAGS`. Write code that doesn't require those.

## Memory

Wasm linear memory grows on demand at runtime via `memory.grow`. You do not need to statically reserve tens of MB of buffers; allocate with `Vec`/`Box` and let the allocator grow the heap as it needs. Large `static mut` arrays that inflate the initial linear-memory size beyond tens of MB can fail to link.
