# Legal Notice

This document describes the provenance and licensing of the material in
this repository. License texts are in `LICENSE`, `LICENSE-MIT`,
`LICENSE-GPL3`, and under each `third_party/*/` directory.

## 1. BIOS

The file `spec/gba_bios_stub.bin` (16 KiB) is an original, clean-room
implementation of a subset of the GBA SWI (software interrupt)
interface. It is generated deterministically by
`spec/gen_bios_stub.py`, an ARM assembler written in Python which
forms part of the source of the stub.

The implementation was produced solely from publicly available
documentation of the SWI interface (primarily GBATEK). No portion was
derived from any disassembly, dump, or other copy of the Nintendo
GBA BIOS.

The stub implements the following SWIs: `RegisterRamReset`, `Halt`,
`IntrWait`, `VBlankIntrWait`, `Div`, `DivArm`, `Sqrt`, `CpuSet`,
`CpuFastSet`, `BiosChecksum`, `BitUnPack`, together with the standard
IRQ dispatch wrapper. `BiosChecksum` returns the documented 32-bit
checksum value expected by software that performs BIOS detection. The
stub does not reproduce the memory layout, multiboot protocol, or other
identifying characteristics of the Nintendo BIOS.

`spec/gba_bios_stub.bin` and `spec/gen_bios_stub.py` are licensed under MIT.

## 2. ROMs

No commercial game ROMs are distributed with this project. The grader
identifies ROMs by SHA-256 content hash, so any ROM a user chooses to
evaluate locally remains on the user's machine and is never transmitted
or published by this project.

ROMs committed under `corpus/roms/` fall into two categories:

- **Hardware test ROMs**, including `armwrestler`, `fuzzarm`,
  `jsmolka/*`, `destoer/*`, `tonc/*`, `nba-hw/*`, `mgba-suite`,
  `240p-test-suite`, and the sound demos. Distributed under their
  respective upstream licenses (MIT, BSD-3-Clause, GPL-2.0-or-later, or
  Unlicense).
- **Homebrew games**, including `celeste-classic` (GBA port),
  `heartwrench-advance`, `anguna`, `another-world`, `goodboy-advance`,
  `blindjump`, `chip-advance`, `spout`, `waimanu`, `piugba`,
  `meteorain`, `trogdor`, `xniq`, `bulletgba`, `varooom-3d`,
  and `collie-defense`. Each is distributed in accordance
  with the terms set by its author — whether an open-source license
  (MIT, BSD, GPL, MPL, Zlib, Unlicense, etc.), a freeware
  redistribution grant, or explicit written permission. Per-ROM
  license tags and attributions are listed in `corpus/README.md`.

## 3. Trademarks

Trademarks referenced in the repository — including but not limited to
"Game Boy", "Game Boy Advance", "Nintendo", "Pokémon", and "Celeste" —
are the property of their respective owners. References are
nominative and descriptive: they identify the hardware emulated, the
origin of a homebrew port, or the category of user-supplied input. No
logos, box art, or other protected assets are included. This project is
independent and not affiliated with, endorsed by, or sponsored by any
trademark holder.

## 4. Emulation

Console emulation through clean-room reimplementation is established as
non-infringing under *Sony Computer Entertainment v. Connectix Corp.*,
203 F.3d 596 (9th Cir. 2000) and *Sega Enterprises Ltd. v. Accolade,
Inc.*, 977 F.2d 1510 (9th Cir. 1992).

## 5. License structure

The repository is multi-licensed along directory boundaries.

| Path | License |
| --- | --- |
| `spec/`, `corpus/` (non-ROM), `candidates/`, `harness/`, `quickstart/`, top-level configuration | MIT |
| `reference/mesen.wasm` | GPL-3.0-only (compiled from Mesen2) |
| `reference/build-mesen.sh`, `reference/mesen_step.cpp` | GPL-3.0-only (build glue/shim for the Mesen2 fork) |
| `third_party/mesen/` | GPL-3.0-only |
| `corpus/roms/` | Per-file; see §2 |

Copyright in the MIT-licensed original work is held by Mechanize, Inc.
(2026).

## 6. Vendored reference emulator

The shipped `reference/mesen.wasm` is compiled from the Mesen2 GBA
headless fork. The canonical public source, which is the Corresponding
Source under GPL-3.0 §6, is:

| Component | License | Corresponding Source | Upstream |
| --- | --- | --- | --- |
| Mesen2 (modified — GBA headless fork) | GPL-3.0-only | <https://github.com/yang-29/Mesen2-gba-headless> | <https://github.com/NovaSquirrel/Mesen2> |

The fork is tracked as a git submodule at `third_party/mesen/`. It
contains full upstream history plus our modifications as commits on
top; the per-commit diffs constitute the GPL-3.0 §5(a) modification
record. An aggregate third-party notice is maintained at `NOTICE` in
the repository root.

The grader binary (`harness/grader`) loads the reference WebAssembly
module at runtime via wasmtime — see §6.1 for the arm's-length
boundary that keeps the grader binary itself MIT-licensed.

### 6.1 License boundary between the MIT grader and GPL-3 reference WASM

The grader binary (`harness/grader`) and the reference WebAssembly
module form a **mere aggregation** under GPL-3.0 §5, not a combined
work:

- The grader hosts the reference wasm in an isolated wasmtime instance
  with its own linear memory. Reference and grader run in separate
  address spaces inside the same process.
- Communication crosses a published, stable ABI (`spec/ABI.md`).
- The reference is selected at runtime via the `--reference` flag
  (default: `reference/mesen.wasm`). Any wasm implementing the ABI is
  interchangeable at runtime — evidence that the grader is not derived
  from any specific reference.
- The grader's source contains no code taken from the vendored
  emulator. It is written against the ABI spec alone.

This matches the FSF's own guidance on programs that communicate at
arm's length across a defined interface (GPL FAQ, "What is the
difference between an 'aggregate' and other kinds of 'modified
versions'?"). The grader binary is therefore distributed under MIT.
GPL-3.0 obligations attach only to the reference WASM module it
loads, and are satisfied via the Corresponding Source URL in the
table above.

### 6.2 Agent-authored candidate emulators

Candidate emulators graded against this benchmark are original works
generated by language models against the MIT `spec/ABI.md`, the MIT
BIOS stub, and publicly redistributable hardware documentation
(GBATEK). Reference frame and audio outputs produced by Mesen2 are
observable data about GBA hardware behavior, not source code;
producing an independent implementation from behavioral observation
is the clean-room reimplementation posture described in §4. No
copyleft obligations from the vendored emulator attach to candidate
code.

## 7. Third-party dependencies

All direct and transitive Rust dependencies listed in `Cargo.lock`
are distributed under permissive licenses (MIT, Apache-2.0,
BSD-3-Clause, or MPL-2.0). No GPL-licensed dependencies are
introduced into the MIT-licensed portion of the project. (MPL-2.0
may appear among transitive dependencies — MPL-2.0 is a file-level
copyleft license compatible with both MIT and GPL-3.0 via its
secondary-license clause and does not impose copyleft on the
MIT-licensed original work.)

## 8. Contact

Inquiries regarding licensing, copyright, trademark, content removal,
or DMCA matters: **stephen@mechanize.work**. See
[`CONTACT.md`](CONTACT.md) for required notice elements, counter-notice
procedure, and our response-time commitment.
