# GBA Eval

We give frontier AI coding agents 24 hours to write a complete software GBA emulator with WebAssembly support. We grade the emulators they produce against Mesen2, one of the most accurate software GBA emulators available.

GBA Eval is analogous to the work [Mechanize](https://mechanize.work) does with top AI labs when we make environments for evaluating and training frontier LLMs, and we hope it gives a concrete sense of what we mean when we talk about environments and grading. If designing and building high-quality evaluations sounds like something you'd want to do, we're [hiring software engineers](https://mechanize.work/apply/software-engineer/?utm_source=gba-eval&utm_campaign=gba-eval).

## See the attempts

The nine emulators produced by the May 2026 leaderboard lineup —
their full commit-by-commit autosave history and a rendered chat-log
transcript for each — are published at
[mechanize-work/gba-eval-attempts](https://github.com/mechanize-work/gba-eval-attempts).
Each model has its own subrepo so you can clone just the one you're
interested in.

## Quickstart

Grade a wasm, or run your own agent inside the benchmark's container.
Docker is the only required dep.

```bash
git submodule update --init --recursive
git lfs install && git lfs pull

./quickstart/smoke-test.sh                                          # 30s preflight
./quickstart/grade.sh candidates/gba-core/gba_core_shim.wasm baseline
```

See [`quickstart/README.md`](quickstart/README.md) for the full
walkthrough — running your own agent in the container, grading a
wasm your agent produced with `--from-container`, and the `--native`
path that skips docker.

## How grading works

```bash
# One-time: precompute reference frames + per-replay thresholds.
# Writes corpus/reference-cache/*.refcache (git-LFS tracked).
cargo run -p grader --release -- --precompute corpus/

# Per candidate: grade wasm → per-testcase JSON + summary.json
cargo run -p grader --release -- candidate.wasm corpus/ results/<candidate>/

# Custom reference: any wasm implementing the ABI works. Default is the
# bundled Mesen2 build at reference/mesen.wasm.
cargo run -p grader --release -- --reference my-ref.wasm candidate.wasm corpus/ results/<candidate>/
```

## Scoring: three sections

The overall score is the weighted sum of three independent sections.
Default weights, configurable in [`corpus/grader.yaml`](corpus/grader.yaml):

| Section | Weight | What it measures |
|---|---|---|
| **Gameplay Replays** | 60% | Real gameplay with button presses — end-to-end behavior under input |
| **Procedural Tests** | 20% | CPU, memory, timers, DMA — deterministic self-checking ROMs |
| **Audio**            | 20% | Per-frame log-mel spectral distance vs Mesen2 |

```
overall = 0.60 × replay + 0.20 × procedural + 0.20 × audio
```

## Building from source

The bundled `reference/mesen.wasm` is enough to grade. To rebuild it
from source (Mesen2 fork at `third_party/mesen`):

```bash
git submodule update --init --recursive
reference/build-mesen.sh wasm      # → reference/mesen.wasm
cargo build --workspace
```

## License

This repository is multi-licensed.

| Path | License | Why |
|---|---|---|
| `spec/`, `corpus/` (non-ROM), `candidates/`, `harness/`, `quickstart/`, repo root | MIT | Our original work |
| `reference/build-mesen.sh`, `reference/mesen_step.cpp` | GPL-3.0 | Build glue + shim for Mesen2 |
| `third_party/mesen/` | GPL-3.0 | Upstream Mesen2 (submodule) |
| `corpus/roms/` | Per ROM | Homebrew & test ROMs under upstream licenses |
| `reference/mesen.wasm` | GPL-3.0 | Compiled from Mesen2 |
| `spec/gba_bios_stub.bin` | MIT | Clean-room stub, not a Nintendo dump |

Refer to [`LEGAL.md`](LEGAL.md) for more details.

For copyright, trademark, content-removal, or DMCA matters, contact
**stephen@mechanize.work** — see [`CONTACT.md`](CONTACT.md).
