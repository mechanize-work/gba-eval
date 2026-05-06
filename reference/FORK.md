# Why the fork is faithful

The grading reference is `third_party/mesen` at branch `gba-headless` —
a fork of [Mesen2](https://github.com/SourMesen/Mesen2) at upstream
commit `fabc9a6`. Mesen2 is the most accurate GBA emulator that
compiles to wasm.

The fork deletes ~350,000 lines: NES/SNES/GB/PCE/SMS/WS cores, netplay,
threads, AVI recording, the debugger UI, the NTSC video filter. What
remains is a single-threaded GBA emulator that builds as a static lib
for native and as a wasm module via emscripten.

**The claim:** the fork emulates the GBA identically to upstream. Same
CPU, same memory timing, same DMA, same prefetch model, same APU.

**The proof:** the entire diff to `Core/GBA/` is below. Five files,
+5 / -34 lines. None of them touch emulation.

```
$ git diff fabc9a6..gba-headless --stat -- Core/GBA/
 Core/GBA/Debugger/GbaDebugger.cpp  |  2 +-
 Core/GBA/GbaConsole.cpp            | 15 +--------------
 Core/GBA/GbaDefaultVideoFilter.cpp | 13 +------------
 Core/GBA/GbaDefaultVideoFilter.h   |  6 +-----
 Core/GBA/GbaPpu.cpp                |  3 +--
 5 files changed, 5 insertions(+), 34 deletions(-)
```

The CPU (`GbaCpu*.cpp`), memory manager (`GbaMemoryManager*.cpp`), DMA
controller (`GbaDmaController.cpp`), timers (`GbaTimer.cpp`), prefetch
unit (`GbaRomPrefetch.cpp`), APU (`APU/`), and cartridge handling
(`Cart/`) are byte-identical to upstream.

---

## The five files, line by line

### `GbaPpu.cpp` — the only emulation-path change

```diff
@@ -183,8 +183,7 @@ void GbaPpu::SendFrame()
 	RenderedFrame frame(_currentBuffer, GbaConstants::ScreenWidth, ...);
-	bool rewinding = _emu->GetRewindManager()->IsRewinding();
-	_emu->GetVideoDecoder()->UpdateFrame(frame, rewinding, rewinding);
+	_emu->GetVideoDecoder()->UpdateFrame(frame, true, false);
```

`UpdateFrame(frame, forceSync, isRewind)`. Upstream queues `frame` to a
decoder thread that calls `DecodeFrame()` asynchronously. `forceSync=true`
makes `DecodeFrame()` run inline on the calling thread instead.

Same `DecodeFrame()`. Same input. Same output. The thread hop is gone,
but the function is identical. This is the single change that makes the
fork deterministic: upstream's threaded decode could in principle race
the next `SendFrame()` (in practice it's gated by a wait, but "in
principle" is one too many for a grading reference).

`isRewind=false` because the fork has no rewind manager (it was in the
350K deleted lines). Rewind is a UI feature; the GBA doesn't know about
it.

### `GbaConsole.cpp` — NTSC filter removed

```diff
 BaseVideoFilter* GbaConsole::GetVideoFilter(bool getDefaultFilter)
 {
-	if(getDefaultFilter) {
-		return new GbaDefaultVideoFilter(_emu, false);
-	}
-	VideoFilterType filterType = _emu->GetSettings()->GetVideoConfig().VideoFilter;
-	switch(filterType) {
-		case VideoFilterType::NtscBlargg:
-		case VideoFilterType::NtscBisqwit:
-			return new GbaDefaultVideoFilter(_emu, true);
-		default:
-			return new GbaDefaultVideoFilter(_emu, false);
-	}
+	return new GbaDefaultVideoFilter(_emu);
 }
```

The NTSC filter is a CRT-look post-process. It runs on the
already-decoded RGB888 framebuffer — the GBA's 5-bit output has already
been expanded by the time it sees pixels. Removing it doesn't change
what the GBA computed.

The grader compares in 5-bit space anyway (`quant5`). Even if the filter
were still here, it'd be invisible to the comparison.

### `GbaDefaultVideoFilter.{cpp,h}` — same filter, ctor signature changed

The `applyNtscFilter` parameter and the `_ntscFilter` member are deleted
to break the link dependency on `GenericNtscFilter` (which pulls in
SNES code). Same `ApplyFilter()` body minus the dead `if(_applyNtscFilter)`
branch at the end.

### `GbaDebugger.cpp` — RTTI compat

```diff
-shared_ptr<GbaController> controller = std::dynamic_pointer_cast<...>(...);
+shared_ptr<GbaController> controller = std::static_pointer_cast<...>(...);
```

The fork builds with `-fno-rtti` (smaller binary, and emscripten's
libc++ + RTTI has rough edges). `dynamic_pointer_cast` needs RTTI. The
debugger is never instantiated in the fork — `MESEN_HEADLESS` ifdefs out
the construction site — so this cast never executes. It just needs to
compile.

---

## Outside `Core/GBA/`

The fork adds one accessor:

```cpp
// Core/Shared/Video/VideoDecoder.h
BaseVideoFilter* GetFilter() { return _videoFilter.get(); }
```

Upstream's `Emulator::GetVideoFilter()` allocates a *new* filter on
every call (it's a factory, not a getter). That filter has an empty
`_outputBuffer`. Useless for reading the framebuffer of the frame that
was just decoded. `VideoDecoder::GetFilter()` returns the filter that
actually ran, with the populated buffer.

Pure addition. Doesn't change any existing behavior.

The remaining ~350K-line diff is deletions. You can verify nothing
emulation-relevant was deleted by listing what's still there:

```
$ git -C third_party/mesen ls-tree -r gba-headless --name-only Core/GBA/ | wc -l
58
$ git -C third_party/mesen ls-tree -r fabc9a6 --name-only Core/GBA/ | wc -l
58
```

Same 58 files. Five modified per the diff above; 53 untouched.

---

## Reproducing this

```bash
git submodule update --init --recursive
git -C third_party/mesen diff fabc9a6..gba-headless --stat -- Core/GBA/
git -C third_party/mesen diff fabc9a6..gba-headless          -- Core/GBA/
```

If those numbers ever change, this document is stale and the fork needs
re-auditing.
