# Grader image — pure-wasm grader binary.
#
# The grader loads both the reference and the candidate as wasm modules
# in wasmtime; no native libmesen.a link, no C++ build, no submodule
# required at build time. The bundled reference wasm at
# reference/mesen.wasm ships pre-built (1.1 MB, regular
# git, not LFS), and `--reference <other.wasm>` swaps it.
#
# Users run this against a repo clone:
#
#   docker run --rm -v "$PWD":/repo:ro -v "$PWD/results":/out \
#     gba-eval-grader /wasm/candidate.wasm corpus/ /out/run
#
# The grader's default reference path is relative
# (`reference/mesen.wasm`), so the repo root MUST be
# mounted at /repo (the image's WORKDIR). corpus/reference-cache/ is
# read from the same /repo mount.
#
# Build:  docker build -f quickstart/grader/Dockerfile -t gba-eval-grader .

FROM rust:1.87-bookworm AS builder

WORKDIR /build
COPY . .

RUN cargo build --release -p grader
RUN strip target/release/grader

# ─── Runtime ─────────────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates tini ffmpeg \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/grader /usr/local/bin/grader

WORKDIR /repo
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/grader"]
