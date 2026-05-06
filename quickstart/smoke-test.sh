#!/usr/bin/env bash
# Preflight check — verifies the repo + tooling are set up to grade.
# Fast (<30s). Does NOT run a real grade (cold-cache grading is minutes,
# which is a separate test: `./grade.sh candidates/gba-core/gba_core_shim.wasm`).
#
# Exits 0 on pass, non-zero on the first failure.

set -euo pipefail
cd "$(dirname "$0")/.."

pass() { printf "  \033[32m✓\033[0m %s\n" "$1"; }
fail() { printf "  \033[31m✗\033[0m %s\n     %s\n" "$1" "$2"; exit 1; }

MODE="${1:-docker}"

echo "── gba-eval smoke test (mode=$MODE) ──"

# ── Clone integrity ─────────────────────────────────────────────────
[ -f Cargo.toml ]            || fail "repo root Cargo.toml"   "are you in the gba-eval clone?"
[ -f corpus/testcases.json ] || fail "corpus/testcases.json"  "corpus/ missing"
[ -f corpus/grader.yaml ]    || fail "corpus/grader.yaml"     "grader config missing"
pass "repo layout"

# ── Submodules (informational — only needed to rebuild the reference wasm) ──
# The bundled reference/mesen.wasm is sufficient for grading;
# the Mesen2 submodule is only required if you want to rebuild that wasm
# from source via reference/build-mesen.sh wasm.
if [ ! -d third_party/mesen ] || [ -z "$(ls third_party/mesen 2>/dev/null)" ]; then
    printf "  \033[33m!\033[0m %s\n     %s\n" \
        "third_party/mesen submodule not initialized" \
        "fine for grading; init only if rebuilding the reference wasm: git submodule update --init --recursive"
else
    pass "third_party/mesen submodule initialized"
fi

# ── LFS ─────────────────────────────────────────────────────────────
FIRST_CACHE="$(ls corpus/reference-cache/*.refcache 2>/dev/null | head -1 || true)"
if [ -z "$FIRST_CACHE" ]; then
    fail "corpus/reference-cache populated" "empty cache dir — did git checkout skip LFS? (git lfs install && git lfs pull)"
fi
if head -c 40 "$FIRST_CACHE" 2>/dev/null | grep -q '^version https://git-lfs'; then
    fail "reference-cache entries are real files" "LFS pointer detected. run: git lfs install && git lfs pull"
fi
pass "LFS reference-cache present"

[ -f reference/mesen.wasm ] || fail "reference/mesen.wasm" \
    "Mesen reference wasm missing from working tree. fresh clone may be incomplete."
pass "Mesen reference wasm present"

# ── Baseline candidate ──────────────────────────────────────────────
[ -f candidates/gba-core/gba_core_shim.wasm ] \
    || fail "baseline candidate wasm" \
            "candidates/gba-core/gba_core_shim.wasm missing — expected to ship in-repo"
pass "baseline candidate present"

# ── Mode-specific: toolchain ────────────────────────────────────────
if [ "$MODE" = "docker" ]; then
    command -v docker >/dev/null 2>&1 || fail "docker on PATH" "install docker"
    docker info >/dev/null 2>&1        || fail "docker daemon reachable" "start docker daemon"
    pass "docker reachable"

    if docker image inspect gba-eval-grader >/dev/null 2>&1; then
        pass "gba-eval-grader image built"
        # grader has no --help flag — it errors on unknown flags. Run
        # with no args; it prints usage to stderr and exits 2. Capture
        # output first so pipefail + non-zero exit don't trip the check.
        grader_out="$(docker run --rm gba-eval-grader 2>&1 || true)"
        if ! echo "$grader_out" | grep -q 'usage:'; then
            fail "grader binary functional in image" "image is built but grader didn't print expected usage line"
        fi
        pass "grader binary functional"
    else
        printf "  \033[33m!\033[0m %s\n     %s\n" \
            "gba-eval-grader image not yet built" \
            "first 'grade.sh' invocation will build it (5-10 min cold)"
    fi
elif [ "$MODE" = "native" ]; then
    command -v cargo >/dev/null 2>&1 || fail "cargo on PATH" "install Rust toolchain"
    pass "cargo available"
    [ -f reference/build-mesen/native/libmesen.a ] \
        || fail "Mesen native lib built" "run: reference/build-mesen.sh native"
    pass "Mesen native lib built"
else
    fail "mode=$MODE" "expected 'docker' or 'native'"
fi

echo
echo "✓ all checks passed. next step:"
echo "    ./quickstart/grade.sh candidates/gba-core/gba_core_shim.wasm baseline"
