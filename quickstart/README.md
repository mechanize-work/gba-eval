# GBA Eval Quickstart

This directory contains scripts and files allowing you to either:

1. **Run your own agent in the same container** the benchmark's agents
   saw — `docker compose up -d && ./shell.sh`
2. **Grade a wasm you or an agent produce** — `./grade.sh path/to/my.wasm`

## Zero-to-score (most users start here)

```bash
# from repo root, after cloning
git submodule update --init --recursive
git lfs install && git lfs pull          # pulls corpus/reference-cache/

./quickstart/smoke-test.sh                # 30s preflight — should print all ✓
./quickstart/grade.sh candidates/gba-core/gba_core_shim.wasm baseline
```

First grade: 5-10 min cold (docker builds `gba-eval-grader` the first
time it runs). Subsequent grades: seconds to a minute, depending on
whether the reference cache is warm.

Output lands in `./results/baseline/` — per-testcase JSON, PNG
screenshots, and `summary.json` with the overall score.

## Requirements

Pick one path:

- **Docker (default)** — docker and docker compose.
- **Native (`--native`)** — Rust 1.87, clang, cmake, and a one-time
  `reference/build-mesen.sh native`. Use this if you're hacking on the
  grader.

Either way you also need `git-lfs` for `corpus/reference-cache/`
(~230 MB pull, 51 precomputed reference frames). `smoke-test.sh` will
tell you if LFS didn't fire.

## Running your own agent

The `task` service is the container the benchmark's agents ran inside —
Rust and the wasm target, wasmtime, `/task/spec/`, `/task/dev-roms/`,
`/task/TASK.md`, and an `oracle` CLI that forwards to the reference
emulator sidecar.

```bash
cd quickstart
cp .env.example .env    # add whichever model key your agent needs
docker compose up -d    # builds images on first run

./shell.sh              # interactive shell as uid 1000
./shell.sh cargo build --release --lib --target wasm32-unknown-unknown
./shell.sh oracle info
```

The container starts with no agent CLI installed — you bring your own.
The compose file passes `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`,
`GEMINI_API_KEY`, `GOOGLE_API_KEY`, and `OPENROUTER_API_KEY` through
from your `.env`, so once a CLI is installed inside the container it
can authenticate. For example, to install Claude Code:

```bash
./shell.sh bash -lc 'curl -fsSL https://claude.ai/install.sh | bash'
./shell.sh claude --task /task/TASK.md
```

Other agent CLIs follow the same pattern — `npm install -g`,
`pip install`, or a shell installer, run via `./shell.sh bash -lc '...'`.

The `task-work` named volume persists `/task/` across restarts, so
your agent's working tree (including its `.git` history) survives
`docker compose down`.

When your agent has built a wasm, grade it:

```bash
./quickstart/grade.sh --from-container my-run
```

This copies `/task/target/wasm32-unknown-unknown/release/gba_emu.wasm`
out of the running task container and grades it.

## Commands

| Script               | What it does                                                 |
|----------------------|--------------------------------------------------------------|
| `smoke-test.sh`      | Preflight: submodules, LFS, baseline, grader image. ~30s.    |
| `grade.sh <wasm>`    | Grade a wasm (docker by default; `--native` to skip docker). |
| `grade.sh --from-container` | Grab the wasm from the running task container, grade it. |
| `shell.sh [cmd]`     | Interactive shell (or one-shot command) in the task container. |
| `docker compose up -d` | Bring up task env + oracle sidecar.                        |
| `docker compose down`  | Stop containers (volume preserved).                        |
| `docker compose down -v` | Stop + wipe the /task/ volume (fresh-start).             |

## Troubleshooting

- **"error: corpus/reference-cache/ contains LFS pointer files"** —
  run `git lfs install && git lfs pull`.
- **"error: third_party/mesen submodule is empty"** —
  run `git submodule update --init --recursive`.
- **Grade is slow on first run** — `gba-eval-grader` image builds once
  (5-10 min). Afterwards reference-cache is warm and grades are fast.
- **Can't pull wasm with `--from-container`** — did you `docker compose
  up -d` from `quickstart/`? Has your agent built a wasm at
  `/task/target/wasm32-unknown-unknown/release/gba_emu.wasm` yet?
- **I don't want docker** — pass `--native` to `grade.sh`. You'll need
  Rust + clang + cmake and to run `reference/build-mesen.sh native` once.

## What's not included

Droplet provisioning, per-model CLI images, the host supervisor,
auto-checkpointing to a private git repo, and the egress allowlist all
lived in our internal deployment and aren't shipped here. `task/Dockerfile`
and `task/TASK.md` are the starting points if you want to build your
own harness on top.
