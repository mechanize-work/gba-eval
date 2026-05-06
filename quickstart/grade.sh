#!/usr/bin/env bash
# Grade a candidate .wasm against the full corpus. Prints the score.
#
# Default path uses docker (only prerequisite: docker). --native runs
# the grader on the host (requires Rust).
#
# Usage:
#   ./grade.sh path/to/gba_emu.wasm                       # docker, auto-named
#   ./grade.sh path/to/gba_emu.wasm my-run                # docker, named run
#   ./grade.sh --reference my-ref.wasm cand.wasm my-run   # use a custom reference wasm
#   ./grade.sh --from-container my-run                    # grab wasm from running task container, grade it
#   ./grade.sh --native path/to/gba_emu.wasm              # host-side grader
#
# --reference defaults to the bundled Mesen2 build at
# `reference/mesen.wasm`. Pass any wasm implementing the
# ABI to grade against a different reference (e.g., grade two
# candidates against each other).
#
# Output: ./results/<name>/ at repo root. summary.json has the overall
# score; grade.sh echoes it on success.

set -euo pipefail
cd "$(dirname "$0")/.."
REPO="$PWD"

MODE="docker"
FROM_CONTAINER=0
WASM=""
NAME=""
REFERENCE=""

while [ $# -gt 0 ]; do
    case "$1" in
        --native)         MODE="native"; shift ;;
        --docker)         MODE="docker"; shift ;;
        --from-container) FROM_CONTAINER=1; shift ;;
        --reference)      REFERENCE="$2"; shift 2 ;;
        -h|--help)
            sed -n '2,21p' "$0" | sed 's/^# \{0,1\}//'
            exit 0 ;;
        --*) echo "unknown flag: $1" >&2; exit 2 ;;
        *)
            if [ -z "$WASM" ] && [ "$FROM_CONTAINER" -eq 0 ]; then
                WASM="$1"
            elif [ -z "$NAME" ]; then
                NAME="$1"
            else
                echo "unexpected arg: $1" >&2; exit 2
            fi
            shift ;;
    esac
done

# ── Pull wasm from the running task container if requested ──────────
if [ "$FROM_CONTAINER" -eq 1 ]; then
    [ -n "$WASM" ] && { echo "error: --from-container and a wasm path are mutually exclusive" >&2; exit 2; }
    WASM="$REPO/quickstart/.extracted.wasm"
    echo "→ extracting wasm from gba-reproduce-task ..."
    if ! docker compose --project-directory "$REPO/quickstart" cp \
            task:/task/target/wasm32-unknown-unknown/release/gba_emu.wasm \
            "$WASM" 2>/dev/null; then
        echo "error: could not copy /task/target/wasm32-unknown-unknown/release/gba_emu.wasm" >&2
        echo "       is the task container up? (cd quickstart && docker compose up -d)" >&2
        echo "       has the candidate been built inside it?" >&2
        exit 1
    fi
fi

[ -z "$WASM" ] && { echo "usage: $0 [--native|--docker|--from-container] [--reference <ref.wasm>] <wasm> [name]" >&2; exit 2; }
[ -f "$WASM" ] || { echo "error: $WASM not found" >&2; exit 1; }
[ -n "$REFERENCE" ] && [ ! -f "$REFERENCE" ] && { echo "error: reference wasm $REFERENCE not found" >&2; exit 1; }

NAME="${NAME:-$(basename "${WASM%.wasm}")-$(date +%Y%m%dT%H%M%SZ)}"
OUT="$REPO/results/$NAME"
mkdir -p "$OUT"

# ── Preflight: LFS pointer files in reference-cache ─────────────────
if [ -d corpus/reference-cache ]; then
    FIRST_CACHE="$(ls corpus/reference-cache/*.refcache 2>/dev/null | head -1 || true)"
    if [ -n "$FIRST_CACHE" ] && head -c 40 "$FIRST_CACHE" 2>/dev/null \
            | grep -q '^version https://git-lfs'; then
        echo "error: corpus/reference-cache/ contains LFS pointer files, not real caches." >&2
        echo "       run: git lfs install && git lfs pull" >&2
        exit 1
    fi
fi

# ── Dispatch ────────────────────────────────────────────────────────
if [ "$MODE" = "docker" ]; then
    command -v docker >/dev/null 2>&1 || { echo "error: docker not found. install docker or use --native." >&2; exit 1; }
    docker info >/dev/null 2>&1 || { echo "error: docker daemon not reachable." >&2; exit 1; }

    if ! docker image inspect gba-eval-grader >/dev/null 2>&1; then
        echo "→ gba-eval-grader image not found; building (~2 min cold)…"
        # No native libs / submodules required — the grader is pure Rust
        # + wasmtime. Build context is the repo root.
        docker build -f quickstart/grader/Dockerfile -t gba-eval-grader .
    fi

    # Mount the wasm by its containing directory so the grader sees a
    # stable inside-container path regardless of where on the host it
    # lives. Repo read-only; results writable. Custom reference wasm
    # mounted alongside if --reference was passed.
    WASM_DIR="$(cd "$(dirname "$WASM")" && pwd)"
    WASM_NAME="$(basename "$WASM")"
    REF_ARGS=()
    REF_MOUNTS=()
    if [ -n "$REFERENCE" ]; then
        REF_DIR="$(cd "$(dirname "$REFERENCE")" && pwd)"
        REF_NAME="$(basename "$REFERENCE")"
        REF_MOUNTS=(-v "$REF_DIR":/ref:ro)
        REF_ARGS=(--reference /ref/"$REF_NAME")
        echo "── Grading $WASM (docker) ref=$REFERENCE → $OUT ──"
    else
        echo "── Grading $WASM (docker) → $OUT ──"
    fi
    docker run --rm \
        -v "$REPO":/repo:ro \
        -v "$WASM_DIR":/wasm:ro \
        "${REF_MOUNTS[@]}" \
        -v "$OUT":/out \
        gba-eval-grader "${REF_ARGS[@]}" /wasm/"$WASM_NAME" corpus/ /out

else
    command -v cargo >/dev/null 2>&1 || { echo "error: cargo not found (needed for --native)." >&2; exit 1; }
    if [ ! -d corpus/reference-cache ] || [ -z "$(ls -A corpus/reference-cache 2>/dev/null)" ]; then
        echo "note: corpus/reference-cache/ is empty — precomputing (one-time, minutes)." >&2
        cargo run -p grader --release -- --precompute corpus/
    fi
    WASM_ABS="$(cd "$(dirname "$WASM")" && pwd)/$(basename "$WASM")"
    REF_FLAG=()
    if [ -n "$REFERENCE" ]; then
        REF_ABS="$(cd "$(dirname "$REFERENCE")" && pwd)/$(basename "$REFERENCE")"
        REF_FLAG=(--reference "$REF_ABS")
        echo "── Grading $WASM (native) ref=$REFERENCE → $OUT ──"
    else
        echo "── Grading $WASM (native) → $OUT ──"
    fi
    cargo run -p grader --release -- "${REF_FLAG[@]}" "$WASM_ABS" corpus/ "$OUT"
fi

# ── Summary ─────────────────────────────────────────────────────────
SUMMARY="$OUT/summary.json"
if [ -f "$SUMMARY" ]; then
    echo
    echo "── summary ──"
    if command -v jq >/dev/null 2>&1; then
        jq . "$SUMMARY"
        OVERALL=$(jq -r '.overall // empty' "$SUMMARY" 2>/dev/null || true)
        [ -n "$OVERALL" ] && echo && echo "overall: $OVERALL"
    else
        cat "$SUMMARY"
    fi
fi
