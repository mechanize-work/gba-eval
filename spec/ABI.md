# GBA Emulator Wasm ABI

Export these 10 C-ABI functions from a `wasm32-unknown-unknown` cdylib:

```c
int       emu_init(void);           // setup, returns 1/0
uint8_t*  emu_rom_buffer(void);     // ≥32 MiB, stable pointer
int       emu_load_rom(int len);    // load + reset, returns 1/0
int       emu_reset(void);          // identical state to load_rom
void      emu_set_keys(uint32_t k); // active-high, latched
void      emu_run_frame(void);      // advance one frame
uint32_t* emu_framebuffer(void);    // 240×160, 0xAABBGGRR, alpha=0xFF
int16_t*  emu_audio_buffer(void);   // i16 stereo interleaved
int       emu_audio_samples(void);  // stereo pairs since last call (drains)
int       emu_audio_rate(void);     // sample rate in Hz
```

Key bits: 0=A, 1=B, 2=Select, 3=Start, 4=Right, 5=Left, 6=Up, 7=Down, 8=R, 9=L.

Install `gba_bios_stub.bin` (16 KiB) at `0x00000000`. Execute it as ARM code.

All ten exports are required and must be fully end-to-end functional. This will be loaded via wasmtime. No threads, no filesystem, no main loop.
