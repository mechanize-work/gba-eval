# Corpus

The test suite: which ROMs, which replays, what each one tests.

---

## `testcases.json`

The grading manifest. Each entry is one (ROM, replay) pair with metadata
for scoring.

```json
{
  "id": "aw-arm-alu",
  "section": "procedural",
  "subsystem": "cpu",
  "audio_subsystem": null,
  "rom_sha256": "9f08d807c03ef296d38ef73e9b827a3d8c77cead9ced44b07d64c10f5f7d0746",
  "rom_name": "test/armwrestler.gba",
  "replay": "aw-arm-alu.txt",
  "frames": 600,
  "scoring_mode": "endstate",
  "description": "Armwrestler ARM ALU — ADC/ADD/AND/BIC/CMN/EOR/MOV/MVN/ORR/RSC/SBC/MLA/MUL/UMULL/SMULL (part 1) + UMLAL/SMLAL/SWP/SWPB/MRS/MSR (part 2)."
}
```

| Field | Type | Required | Description |
|---|---|---|---|
| `id` | string | yes | Unique, URL-safe. Used in result paths (`results/<candidate>/<id>.json`). |
| `section` | string | yes | Which scored section this belongs to: `procedural` or `replay`. (Audio is scored from any testcase that sets `audio_subsystem`, regardless of `section`.) |
| `subsystem` | string | yes | Tag for grouping within the section — see `subsystem_weights` in [`grader.yaml`](grader.yaml) for the recognized tags and their weights. |
| `audio_subsystem` | string \| null | no | If the ROM is expected to produce scorable audio, the audio subsystem tag (also in [`grader.yaml`](grader.yaml)). `null` or omitted = no audio expected; the audio score is skipped for this testcase. |
| `rom_sha256` | string | yes | Lowercase hex SHA-256 of the ROM. The grader hashes every `.gba` under `corpus/roms/` and matches by hash — filenames don't matter. |
| `rom_name` | string | yes | Human-readable. For logs. Not used for lookup. |
| `replay` | string | no | Filename under `corpus/replays/`. Omit or set `""` for no input (run with keys=0). |
| `frames` | u32 | yes | How many frames to run. Should be ≥ the replay's last event + enough slack for the test to render. |
| `scoring_mode` | string | no | `frame_mean` (default) averages the per-frame sigmoid over the whole run. `endstate` scores only the final frame against a tight threshold — for self-checking ROMs that print PASS/FAIL on screen. |
| `description` | string | no | One-liner for logs and tooltips. |

Section weights, subsystem weights, and the scored subsystem tags themselves all live in [`grader.yaml`](grader.yaml). There are no per-testcase weights — adding more testcases to a subsystem improves its measurement precision without shifting any other subsystem's weight.

---

## `roms/`

ROM files, organized into three directories:

- **`test/`** — Hardware test ROMs (deterministic, no input needed).
  Force-added to git.
  - `armwrestler.gba` — visual CPU test (michelS)
  - `fuzzarm.gba` — randomized instruction fuzzer
  - `jsmolka/` — ARM/Thumb/memory/BIOS/PPU/save tests
  - `destoer/` — DMA priority, ISR timing, scanline timing, LYC
    mid-line, window mid-frame, IF ack, conditional edge cases (MIT)
  - `tonc/` — TONC tutorial demos: bitmap modes (m3, bm-modes,
    pageflip), affine BG (sbb-aff, m7-demo), affine sprites (obj-aff),
    blending (bld-demo), windows (win-demo), mosaic (mos-demo), DMA
    (dma-demo), timers (tmr-demo), IRQ (irq-demo), SWI (swi-demo),
    audio PSG (snd1-demo), input (key-demo), layer priority
    (prio-demo) (MIT)
  - `nba-hw/` — NanoBoyAdvance hardware tests: DMA (burst-into-tears,
    force-nseq, latch, start-delay), timer (reload, start-stop), PPU
    (bgpd, bgx, dispcnt-latch, greenswap, ram-access-timing,
    sprite-hmosaic, status-irq-dma, vram-mirror), IRQ delay, bus
    128KB boundary, haltcnt (BSD-3-Clause)
  - `mgba-suite.gba` — mGBA test suite: memory, IO reads, timing,
    timers, timer IRQ, shifter, carry, multiply-long, BIOS math, DMA,
    SIO, misc edge cases, video. Built from mgba-emu/suite (MIT)
  - `misc/240p-test-suite.gba` — video signal / display rendering
    validation (GPLv2+)
  - `misc/gba-sound-demo-rates.gba` — FIFO/DMA audio at configurable
    sample rates (Unlicense)
  - `misc/gba-sound-demo-song.gba` — music playback at multiple sample
    rates (Unlicense)

- **`homebrew/`** — Open-source / freely distributable homebrew games.
  Force-added to git.
  - `celeste-classic.gba` — Celeste Classic port
  - `heartwrench-advance.gba` — Heartwrench Advance
  - `anguna.gba` — Zelda-like action RPG (freely redistributable)
  - `another-world.gba` — Another World port, bitmap mode 4 (GPL)
  - `goodboy-advance.gba` — scrolling platformer (open source)
  - `blindjump.gba` — roguelike, heavy sprites/scrolling/audio (GPL)
  - `chip-advance.gba` — chiptune player, PSG audio exercise
  - `spout.gba` — bitmap mode particle game
  - `waimanu.gba` — scrolling platformer, diverse tilemap use
  - `piugba.gba` — Pump It Up rhythm simulator (MIT)
  - `meteorain.gba`, `trogdor.gba`, `xniq.gba` — dev-roms shipped in
    the agent task container; not in the grader corpus
  - `bulletgba.gba` — Touhou-style bullet-hell simulator, heavy OAM
    (Unlicense / public domain)
  - `varooom-3d.gba` — software-rendered 3D racer, Mode 4 bitmap
    streaming (Zlib code, CC-BY-NC-SA music)
  - `collie-defense.gba` — sheep-herding tower defense, rumble-pack
    GPIO (GPL-3 code, CC-BY-SA art, CC0 music)

- **`commercial/`** — Never checked in (copyrighted). Drop your own
  dumps here. The grader hashes every `.gba` recursively and matches
  by SHA-256 — filenames don't matter.

`.gba` files are gitignored at the repo root. Test ROMs and OSS
homebrew are force-added.

---

## `replays/`

Input logs as `<frame> <keys_hex>` pairs, one event per line; `#`
introduces a comment. `keys_hex` is the GBA KEYINPUT bitmask in
active-high form (bit 0 = A, 1 = B, 2 = Select, 3 = Start, 4 = Right,
5 = Left, 6 = Up, 7 = Down, 8 = R, 9 = L). The recorded value holds
until the next event line. The first few lines of any existing replay
in this directory are a self-explanatory example.

---

## Adding a testcase

1. Get the ROM's SHA-256:
   ```bash
   shasum -a 256 my_rom.gba
   ```

2. If homebrew/test ROM, add it under `roms/`. If commercial, drop it
   there locally — `.gba` is gitignored at repo root, so commercial
   dumps stay out of git.

3. Record a replay if the test needs input. Test ROMs that self-check
   usually don't.

4. Add an entry to `testcases.json` with `section`, `subsystem` (and
   `audio_subsystem` if the ROM has scorable audio) matching tags that
   exist in [`grader.yaml`](grader.yaml).

5. Run the grader to verify:
   ```bash
   cargo run -p grader -- <candidate.wasm> corpus/ /tmp/test-results/
   ```
