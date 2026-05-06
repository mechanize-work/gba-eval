This directory holds candidate emulator implementations. Each candidate
is a separate crate that compiles to a `wasm32-unknown-unknown` cdylib
exporting the ten C functions in [`spec/ABI.md`](../spec/ABI.md). Drop
in your own as a sibling to `gba-core/`.

`gba-core/` is a baseline candidate, included so a fresh clone can grade
something out of the box and so you have a worked example to copy from.
The pre-built `gba-core/gba_core_shim.wasm` is what
`quickstart/grade.sh` grades when no path is supplied; it's an
intentionally partial implementation (overall ≈ 0.53) — useful as a
sanity check that the toolchain is working end-to-end, not as a target
to beat.
