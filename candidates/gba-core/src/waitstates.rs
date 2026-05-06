//! GBA memory waitstate LUT, mirror of mesen's GbaWaitStates.h.
//!
//! Indexed by `((addr >> 22) & 0x3FC) | (mode & 3)`:
//!   - region byte at bits 9..2  (addr[31..24] shifted into place)
//!   - mode bits 0..1:  bit 0 = Sequential, bit 1 = Word
//!
//! Stored value is TOTAL cycles for the access (1-cycle baseline + waitstates).
//! Caller subtracts 1 to convert to "extra cycles above baseline" when adding
//! to gba-core's existing per-instruction constants (which already assume
//! 1-cycle-per-access on fast memory).

pub const SEQUENTIAL: u8 = 1 << 0;
pub const WORD: u8 = 1 << 1;

pub struct WaitStates {
    lut: Box<[u8; 0x400]>,
    /// WAITCNT-derived values. Layout matches mesen's GbaMemoryManagerState:
    /// [0] = non-seq waitstates (N), [1] = sequential waitstates (S),
    /// both in total cycles (baseline + wait). Defaults are the WAITCNT=0
    /// values (slowest).
    prg0: [u8; 2],
    prg1: [u8; 2],
    prg2: [u8; 2],
    sram: u8,
}

impl WaitStates {
    pub fn new() -> Self {
        let mut ws = Self {
            lut: Box::new([0u8; 0x400]),
            // WAITCNT=0 defaults: N=5 (bits 00=4 waits +1), S per-slot.
            prg0: [5, 3],  // WS0 default: N=5, S=3 (bit 4 = 0 → 2 waits)
            prg1: [5, 5],  // WS1 default: N=5, S=5 (bit 7 = 0 → 4 waits)
            prg2: [5, 9],  // WS2 default: N=5, S=9 (bit 10 = 0 → 8 waits)
            sram: 5,       // SRAM default: 5
        };
        ws.regenerate_lut();
        ws
    }

    /// Update waitstate arrays from WAITCNT register value and rebuild LUT.
    pub fn write_waitcnt(&mut self, waitcnt: u16) {
        // Table values are total cycles (1 baseline + waitstate above).
        const N_TABLE: [u8; 4] = [5, 4, 3, 9]; // bits → 4,3,2,8 waitstates + 1
        // Sequential values per slot — bit 4 / 7 / 10.
        self.prg0[0] = N_TABLE[((waitcnt >> 2) & 3) as usize];
        self.prg0[1] = if waitcnt & (1 << 4) != 0 { 2 } else { 3 }; // 1 or 2 wait
        self.prg1[0] = N_TABLE[((waitcnt >> 5) & 3) as usize];
        self.prg1[1] = if waitcnt & (1 << 7) != 0 { 2 } else { 5 }; // 1 or 4 wait
        self.prg2[0] = N_TABLE[((waitcnt >> 8) & 3) as usize];
        self.prg2[1] = if waitcnt & (1 << 10) != 0 { 2 } else { 9 }; // 1 or 8 wait
        self.sram = N_TABLE[(waitcnt & 3) as usize];
        self.regenerate_lut();
    }

    /// Rebuild LUT matching mesen's GenerateWaitStateLut exactly.
    fn regenerate_lut(&mut self) {
        for mode in 0u8..4 {
            for i in 0u8..=0xFF {
                let is_seq = (mode & SEQUENTIAL) as usize;
                let is_word = mode & WORD != 0;
                let waitstates: u8 = match i {
                    0x02 => if is_word { 6 } else { 3 }, // EWRAM
                    0x05 | 0x06 => if is_word { 2 } else { 1 }, // VRAM/Palette
                    0x08 | 0x09 => {
                        let first = self.prg0[is_seq];
                        first + if is_word { self.prg0[1] } else { 0 }
                    }
                    0x0A | 0x0B => {
                        let first = self.prg1[is_seq];
                        first + if is_word { self.prg1[1] } else { 0 }
                    }
                    0x0C | 0x0D => {
                        let first = self.prg2[is_seq];
                        first + if is_word { self.prg2[1] } else { 0 }
                    }
                    0x0E | 0x0F => self.sram,
                    _ => 1, // BIOS, IWRAM, I/O, OAM — single cycle
                };
                self.lut[(i as usize) << 2 | mode as usize] = waitstates;
            }
        }
    }

    /// Mesen: GetWaitStates with 128KB-boundary sequential mask.
    /// Returns total cycles for this access.
    #[inline]
    pub fn get(&self, addr: u32, mode: u8) -> u8 {
        let seq_mask = if addr & 0x1_FFFF != 0 { SEQUENTIAL } else { 0 };
        let idx = ((addr >> 22) & 0x3FC) | ((mode & (WORD | seq_mask)) as u32);
        self.lut[idx as usize]
    }

    /// For internal prefetch use — no boundary mask.
    #[inline]
    pub fn get_prefetch(&self, addr: u32, mode: u8) -> u8 {
        let idx = ((addr >> 22) & 0x3FC) | ((mode & (WORD | SEQUENTIAL)) as u32);
        self.lut[idx as usize]
    }
}
