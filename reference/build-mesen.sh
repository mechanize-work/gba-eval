#!/bin/bash
# Build Mesen2 GBA core → static lib → linked with mesen_step.cpp.
#
# Mesen has no CMakeLists for Core/ — the upstream makefile builds a
# .dylib from a flat list of every .cpp in the tree. We do the same but
# only for the directories the GBA core actually pulls in.
#
# Stage 1: native build (debugging the source set).
# Stage 2: wasm build (once native links).
#
# The fork: third_party/mesen @ gba-headless. Core/GBA/ is +5/-34 vs
# upstream — see FORK.md. `git -C third_party/mesen log gba-headless`
# shows the surgery commit-by-commit.

set -euo pipefail
cd "$(dirname "$0")"

REF_DIR=$(pwd)
ROOT="$REF_DIR/.."
MESEN="$ROOT/third_party/mesen"
BUILD_DIR="$REF_DIR/build-mesen"

TARGET="${1:-native}"  # native | wasm

mkdir -p "$BUILD_DIR/$TARGET"

# BIOS stub: ARM SWI handlers + IRQ wrapper (see spec/ABI.md → BIOS).
# Mesen has no HLE SWI — without this, games stall on the first IntrWait.
# Regenerated on every build so spec/gba_bios_stub.h tracks the assembler.
python3 "$ROOT/spec/gen_bios_stub.py" \
    "$BUILD_DIR/gba_bios_stub.bin" \
    "$ROOT/spec/gba_bios_stub.h"

# ─── Source set ──────────────────────────────────────────────────────────
# What we compile and why:
#   Core/GBA/         — the actual emulator
#   Core/Shared/      — Emulator, settings, video decode, control mgr
#   Core/Debugger/    — Emulator.cpp constructs Debugger; ScriptManager
#                       (Lua) is excluded — only LuaApi.cpp/LuaScript* hit it
#   Core/Netplay/     — Emulator ctor news GameServer/GameClient. Sockets
#                       compile fine on emscripten (stubbed); they're never
#                       opened.
#   Utilities/        — VirtualFile, Serializer, hash, etc.
#   SevenZip/         — VirtualFile pulls in ArchiveReader → 7z.
#
# What we DON'T compile (MESEN_HEADLESS ifdef'd out the dispatch):
#   Core/{NES,SNES,Gameboy,PCE,SMS,WS}/
#   Lua/
#   Sdl/, Linux/, MacOS/, Windows/

# `|| true` guards each find so set -e doesn't kill the subshell on a
# nonzero find (e.g. missing dir).
#
# Debugger exclusions: Lua bindings + multi-console disassembler bits
# whose switch-on-CpuType bodies still reference SNES/NES types.
# Headless never creates a Debugger; the few referenced symbols get
# stubbed in mesen_step.cpp.
SOURCES=()
while IFS= read -r f; do SOURCES+=("$f"); done < <(
    find "$MESEN/Core/GBA" "$MESEN/Core/Shared" \
         "$MESEN/Utilities" "$MESEN/SevenZip" \
         -name "*.cpp" -o -name "*.c" 2>/dev/null || true
    find "$MESEN/Core/Debugger" -name "*.cpp" \
         ! -name "LuaApi.cpp" ! -name "LuaScriptingContext.cpp" \
         ! -name "LuaCallHelper.cpp" ! -name "ScriptManager.cpp" \
         ! -name "ScriptHost.cpp" \
         ! -name "Disassembler.cpp" ! -name "DisassemblyInfo.cpp" \
         ! -name "ExpressionEvaluator.cpp" \
         2>/dev/null || true
)

echo "── ${#SOURCES[@]} source files ──"

# ─── Compile flags ───────────────────────────────────────────────────────
# pch.h is included by every TU but Mesen doesn't actually precompile it
# (the makefile just lets each TU re-parse it). Include path covers it.
# -I Utilities: SimpleLock.cpp does #include <Timer.h> with angle brackets.
COMMON_FLAGS=(
    -DMESEN_HEADLESS
    -I"$MESEN/Core"
    -I"$MESEN"
    -I"$MESEN/Utilities"
    -I"$ROOT/spec"   # gba_bios_stub.h
    -O2
    # Mesen type-puns; required to avoid SEGV under -O2.
    -fno-strict-aliasing
    -Wno-switch -Wno-unused-parameter -Wno-deprecated-declarations
)
CXX_ONLY_FLAGS=(
    -std=c++17
    # -fno-rtti: worker stripped all dynamic_cast users (MemoryDumper
    # console dispatch, BaseControlManager::GetControlDevice<T> template,
    # GbaDebugger controller cast). The lib is built without RTTI.
    -fno-rtti
)

if [[ "$TARGET" == "native" ]]; then
    CXX=clang++
    CC=clang
    AR=ar
    OUT_LIB="$BUILD_DIR/native/libmesen.a"
    LINK_FLAGS=()
elif [[ "$TARGET" == "wasm" ]]; then
    CXX=em++
    CC=emcc
    # macOS ar doesn't grok wasm objects — empty TOC, then wasm-ld fails
    # with "section too large" on the first .o it tries to scan raw.
    AR=emar
    OUT_LIB="$BUILD_DIR/wasm/libmesen.a"
    # STANDALONE_WASM: minimize imports to WASI + `env.emscripten_notify_
    # memory_growth` so the same wasm runs in wasmtime (the grader) and
    # in any other host with WASI shims. FORCE_FILESYSTEM keeps
    # emscripten's MEMFS inside the wasm so fopen works in either
    # runtime. --embed-file pre-populates the BIOS stub at the exact
    # path FirmwareHelper::LoadGbaBootRom reads — no host FS, no JS
    # glue, no drift. EXPORTED_FUNCTIONS lists both prefixes: `mesen_*`
    # for the reference path and `emu_*` for the grader's candidate ABI
    # loader (spec/ABI.md).
    LINK_FLAGS=(
        -s MODULARIZE=1 -s EXPORT_ES6=1 -s ENVIRONMENT=web
        -s STANDALONE_WASM=1
        -s FORCE_FILESYSTEM=1
        -s ALLOW_MEMORY_GROWTH=1 -s INITIAL_MEMORY=64MB
        # No main() — this is a function library, not an executable.
        # --no-entry stops the linker asking for main; INVOKE_RUN=0 stops
        # the JS glue from calling __start on init.
        -Wl,--no-entry
        -s INVOKE_RUN=0
        --embed-file "$BUILD_DIR/gba_bios_stub.bin@/mesen/Firmware/gba_bios.bin"
        -s 'EXPORTED_FUNCTIONS=["_mesen_init","_mesen_rom_buffer","_mesen_load_rom","_mesen_reset","_mesen_set_keys","_mesen_run_frame","_mesen_framebuffer","_mesen_audio_buffer","_mesen_audio_samples","_mesen_audio_rate","_mesen_frame_count","_mesen_debug_keyinput","_emu_init","_emu_rom_buffer","_emu_load_rom","_emu_reset","_emu_set_keys","_emu_run_frame","_emu_framebuffer","_emu_audio_buffer","_emu_audio_samples","_emu_audio_rate","_emu_boot_frames"]'
        -s 'EXPORTED_RUNTIME_METHODS=["HEAPU8"]'
    )
else
    echo "usage: $0 [native|wasm]" >&2; exit 1
fi

# ─── Stage 1: object files → archive ────────────────────────────────────
# Parallel compile. Hash-named objects so changing one source rebuilds
# only that object.
NCPU=$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 4)
OBJ_DIR="$BUILD_DIR/$TARGET/obj"
mkdir -p "$OBJ_DIR"

export CXX CC OBJ_DIR
export COMMON_FLAGS_STR="${COMMON_FLAGS[*]}"
export CXX_ONLY_FLAGS_STR="${CXX_ONLY_FLAGS[*]}"

# bash arrays don't export; reconstruct in subshell. Per-file: pick CXX/CC
# and add C++-only flags only for .cpp.
printf '%s\n' "${SOURCES[@]}" | xargs -P"$NCPU" -I{} bash -c '
    src="$1"
    hash=$(echo "$src" | shasum | cut -c1-16)
    obj="$OBJ_DIR/$hash.o"
    [[ "$obj" -nt "$src" ]] && exit 0
    if [[ "$src" == *.c ]]; then
        "$CC" $COMMON_FLAGS_STR -c "$src" -o "$obj" 2>&1 | head -3
    else
        "$CXX" $COMMON_FLAGS_STR $CXX_ONLY_FLAGS_STR -c "$src" -o "$obj" 2>&1 | head -3
    fi
' _ {}

OBJS=("$OBJ_DIR"/*.o)
echo "── ${#OBJS[@]} objects compiled ──"

# ar rcs APPENDS — a stale .a from a wider source set keeps old members
# and you get duplicate-symbol at link. The obj/ glob also picks up stale
# .o files that no longer correspond to anything in SOURCES (changed
# paths, deleted files). Build the archive from scratch every time; the
# compile cache above is what makes incremental builds fast, not this.
rm -f "$OUT_LIB"
"$AR" rcs "$OUT_LIB" "${OBJS[@]}"
echo "── archived → $OUT_LIB ──"

# ─── Stage 2: link shim ─────────────────────────────────────────────────
if [[ "$TARGET" == "native" ]]; then
    # Native: build a tiny test executable to surface undefined symbols.
    "$CXX" "${COMMON_FLAGS[@]}" "${CXX_ONLY_FLAGS[@]}" \
        "$REF_DIR/mesen_step.cpp" "$OUT_LIB" \
        -lz $(if [ "$(uname)" = "Darwin" ]; then echo "-lc++"; else echo "-lstdc++"; fi) \
        -o "$BUILD_DIR/native/mesen_step_test"
    echo "── native test linked ──"
    ls -lh "$BUILD_DIR/native/mesen_step_test"
else
    "$CXX" "${COMMON_FLAGS[@]}" "${CXX_ONLY_FLAGS[@]}" \
        "$REF_DIR/mesen_step.cpp" "$OUT_LIB" \
        "${LINK_FLAGS[@]}" \
        -o "$BUILD_DIR/wasm/mesen_step.js"
    cp "$BUILD_DIR/wasm/mesen_step.wasm" "$REF_DIR/mesen.wasm"
    echo "── wasm built → $REF_DIR/mesen.wasm ──"
    ls -lh "$REF_DIR/mesen.wasm"
fi
