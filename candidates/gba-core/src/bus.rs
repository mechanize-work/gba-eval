//! GBA memory bus. All CPU/DMA accesses route through here.
//!
//! Address map:
//!   0000_0000-0000_3FFF   BIOS (16 KiB, read-only, protected after boot)
//!   0200_0000-0203_FFFF   EWRAM (256 KiB, 16-bit bus, slow)
//!   0300_0000-0300_7FFF   IWRAM (32 KiB, 32-bit bus, fast)
//!   0400_0000-0400_03FE   I/O registers
//!   0500_0000-0500_03FF   Palette RAM (1 KiB)
//!   0600_0000-0601_7FFF   VRAM (96 KiB)
//!   0700_0000-0700_03FF   OAM (1 KiB)
//!   0800_0000-0DFF_FFFF   Game Pak ROM (waitstate 0/1/2 mirrors)
//!   0E00_0000-0E00_FFFF   Game Pak SRAM (8-bit bus)

use crate::apu::Apu;
use crate::dma::{Dma, DmaBusAccess, DmaTiming};
use crate::gpio::Gpio;
use crate::io::IoRegs;
use crate::ppu::Ppu;
use crate::save::Save;
use crate::scheduler::Scheduler;
use crate::timer::Timers;
use crate::waitstates::{WaitStates, SEQUENTIAL, WORD};

const BIOS_SIZE: usize = 16 * 1024;
const EWRAM_SIZE: usize = 256 * 1024;
const IWRAM_SIZE: usize = 32 * 1024;

pub struct Bus {
    pub bios: Box<[u8; BIOS_SIZE]>,
    pub ewram: Box<[u8; EWRAM_SIZE]>,
    pub iwram: Box<[u8; IWRAM_SIZE]>,
    pub rom: Vec<u8>,
    pub sram: Box<[u8; 64 * 1024]>,
    pub save: Save,

    pub ppu: Ppu,
    pub io: IoRegs,
    pub dma: Dma,
    pub apu: Apu,
    pub timers: Timers,
    pub gpio: Gpio,
    pub sched: Scheduler,

    /// Last value driven on the bus (open-bus reads return this).
    open_bus: u32,
    /// BIOS reads from outside BIOS return the last fetched BIOS opcode.
    bios_last_fetch: u32,
    /// Guard against re-entrant DMA (DMA writing to DMA control registers).
    dma_running: bool,
    /// Extra cycles charged by memory accesses during the current CPU
    /// instruction, above the 1-cycle-per-access baseline that gba-core's
    /// instruction constants assume. CPU resets before step, adds after.
    pub pending_extra_cycles: u32,
    /// Pipeline-sequential flag mirroring mesen's `_state.Pipeline.Mode &
    /// GbaAccessMode::Sequential`. Set by code fetches (ReadCode), cleared
    /// by data accesses (Read/Write) and branch flushes (ReloadPipeline).
    pub sequential: bool,
    /// Waitstate LUT — rebuilt on every WAITCNT write.
    pub waitstates: WaitStates,
}

impl Bus {
    pub fn new() -> Self {
        Self {
            bios: Box::new([0; BIOS_SIZE]),
            ewram: Box::new([0; EWRAM_SIZE]),
            iwram: Box::new([0; IWRAM_SIZE]),
            rom: Vec::new(),
            sram: Box::new([0xFF; 64 * 1024]),
            save: Save::new(),
            ppu: Ppu::new(),
            io: IoRegs::new(),
            dma: Dma::new(),
            apu: Apu::new(),
            timers: Timers::new(),
            gpio: Gpio::new(),
            sched: Scheduler::new(),
            open_bus: 0,
            bios_last_fetch: 0xE129F000, // matches real BIOS startup latch
            dma_running: false,
            pending_extra_cycles: 0,
            sequential: false,
            waitstates: WaitStates::new(),
        }
    }

    /// Called whenever WAITCNT is written. Regenerates the waitstate LUT.
    pub fn on_waitcnt_write(&mut self) {
        self.waitstates.write_waitcnt(self.io.waitcnt);
    }

    /// Charge cycles for a DATA access at `addr` (16-bit or 32-bit wide).
    /// Data accesses are always non-sequential and clear the pipeline
    /// Sequential flag. Returns extra cycles beyond the 1-cycle baseline.
    #[inline]
    fn charge_data(&mut self, addr: u32, is_word: bool) -> u32 {
        let mode = if is_word { WORD } else { 0 };
        let total = self.waitstates.get(addr, mode) as u32;
        self.sequential = false;
        total.saturating_sub(1)
    }

    /// Charge cycles for a CODE fetch at `addr` (halfword in Thumb,
    /// word in ARM). Sets Sequential for the next fetch. Returns extra
    /// cycles beyond the baseline.
    #[inline]
    fn charge_code(&mut self, addr: u32, is_word: bool) -> u32 {
        let mode_flags = (if self.sequential { SEQUENTIAL } else { 0 })
            | (if is_word { WORD } else { 0 });
        let total = self.waitstates.get(addr, mode_flags) as u32;
        self.sequential = true;
        total.saturating_sub(1)
    }

    pub fn load_bios(&mut self, data: &[u8]) {
        let n = data.len().min(BIOS_SIZE);
        self.bios[..n].copy_from_slice(&data[..n]);
    }

    pub fn load_rom(&mut self, data: Vec<u8>) {
        self.save.detect_from_rom(&data);
        // Auto-detect GPIO/RTC: Pokemon ROMs have the string "RTC_V" in ROM.
        let s = String::from_utf8_lossy(&data);
        if s.contains("RTC_V") {
            self.gpio.readable = true;
            self.gpio.control = 1;
        }
        self.rom = data;
    }

    // ---- 8-bit ----

    pub fn read8(&mut self, addr: u32, pc: u32) -> u8 {
        self.pending_extra_cycles += self.charge_data(addr, false);
        let region = (addr >> 24) & 0xF;
        // Save/SRAM region is 8-bit bus — read directly to preserve byte address.
        if region == 0xE || region == 0xF {
            return self.save.read8(addr);
        }
        let v = self.read32_inner(addr & !3, pc);
        (v >> ((addr & 3) * 8)) as u8
    }

    pub fn write8(&mut self, addr: u32, val: u8) {
        self.pending_extra_cycles += self.charge_data(addr, false);
        let region = (addr >> 24) & 0xF;
        match region {
            0x2 => self.ewram[(addr as usize) & (EWRAM_SIZE - 1)] = val,
            0x3 => self.iwram[(addr as usize) & (IWRAM_SIZE - 1)] = val,
            0x4 => {
                // HALTCNT at 0x04000301 is a special 8-bit register.
                if addr == 0x0400_0301 {
                    if val & 0x80 == 0 {
                        self.io.haltcnt = true;
                    }
                    return;
                }
                // For all other I/O, promote to 16-bit read-modify-write via bus paths.
                let aligned = addr & !1;
                let old = self.read_io16(aligned);
                let new_val = if addr & 1 == 0 {
                    (old & 0xFF00) | val as u16
                } else {
                    (old & 0x00FF) | ((val as u16) << 8)
                };
                self.write16_inner(aligned, new_val);
            }
            // Palette/VRAM/OAM ignore 8-bit writes or replicate to 16-bit on real HW.
            // Palette & VRAM (BG region) replicate the byte across both halves.
            0x5 => {
                let v16 = (val as u16) | ((val as u16) << 8);
                self.ppu.write_palette16(addr & !1, v16);
            }
            0x6 => {
                // OBJ tile region ignores 8-bit writes; BG region replicates.
                // Cutoff depends on bitmap vs tile mode; PPU decides.
                self.ppu.write_vram8(addr, val);
            }
            0x7 => { /* OAM ignores 8-bit writes */ }
            0xE | 0xF => self.save.write8(addr, val),
            _ => {}
        }
    }

    // ---- 16-bit ----

    pub fn read16(&mut self, addr: u32, pc: u32) -> u16 {
        self.pending_extra_cycles += self.charge_data(addr & !1, false);
        let v = self.read32_inner(addr & !3, pc);
        (v >> ((addr & 2) * 8)) as u16
    }

    /// Code fetch — 16-bit (Thumb). Uses Sequential flag for N-vs-S timing
    /// and sets Sequential after the access (mesen's ReadCode invariant).
    pub fn fetch16(&mut self, addr: u32) -> u16 {
        let addr = addr & !1;
        self.pending_extra_cycles += self.charge_code(addr, false);
        let v = self.read32_inner(addr & !3, addr);
        (v >> ((addr & 2) * 8)) as u16
    }

    /// Code fetch — 32-bit (ARM).
    pub fn fetch32(&mut self, addr: u32) -> u32 {
        let addr = addr & !3;
        self.pending_extra_cycles += self.charge_code(addr, true);
        self.read32_inner(addr, addr)
    }

    pub fn write16(&mut self, addr: u32, val: u16) {
        self.pending_extra_cycles += self.charge_data(addr & !1, false);
        self.write16_inner(addr, val);
    }

    fn write16_inner(&mut self, addr: u32, val: u16) {
        let addr = addr & !1;
        let region = (addr >> 24) & 0xF;
        match region {
            0x2 => {
                let i = (addr as usize) & (EWRAM_SIZE - 1);
                self.ewram[i..i + 2].copy_from_slice(&val.to_le_bytes());
            }
            0x3 => {
                let i = (addr as usize) & (IWRAM_SIZE - 1);
                self.iwram[i..i + 2].copy_from_slice(&val.to_le_bytes());
            }
            0x4 => {
                // Internal memory control at 0x04000800 (ignored write).
                if addr & 0xFFFF >= 0x0800 { return; }

                let reg = addr & 0x3FE;

                // DMA registers: 0x0B0-0x0DF.
                if reg >= 0x0B0 && reg <= 0x0DE {
                    let ch = ((reg - 0x0B0) / 12) as usize;
                    let off = (reg - 0x0B0) % 12;
                    match off {
                        0 => self.dma.write_src_lo(ch, val),
                        2 => self.dma.write_src_hi(ch, val),
                        4 => self.dma.write_dst_lo(ch, val),
                        6 => self.dma.write_dst_hi(ch, val),
                        8 => self.dma.write_count(ch, val),
                        10 => {
                            self.dma.write_control(ch, val);
                            if self.dma.channels[ch].enabled()
                                && self.dma.channels[ch].timing() == DmaTiming::Immediate
                                && !self.dma_running
                            {
                                self.run_immediate_dma();
                            }
                        }
                        _ => {}
                    }
                    return;
                }
                // Affine BG parameters: 0x020-0x03F.
                if reg >= 0x020 && reg <= 0x03F {
                    self.ppu.write_affine_param(reg as u16, val);
                    return;
                }
                // Blend/window registers.
                if reg >= 0x040 && reg <= 0x054 {
                    self.ppu.write_effect_reg(reg as u16, val);
                    return;
                }
                // Sound registers: 0x060-0x0A6.
                if reg >= 0x060 && reg <= 0x0A6 {
                    self.apu.write_reg(addr, val);
                    return;
                }
                // Timer registers: 0x100-0x10E.
                if reg >= 0x100 && reg <= 0x10E {
                    let i = ((reg - 0x100) / 4) as usize;
                    let off = (reg - 0x100) % 4;
                    if off == 0 {
                        self.timers.write_reload(i, val);
                    } else {
                        self.timers.write_control(i, val);
                    }
                    return;
                }
                self.io.write16(addr, val, &mut self.ppu, &mut self.sched);
                // WAITCNT @ 0x04000204: regenerate waitstate LUT so subsequent
                // ROM accesses use the game-configured waitstates (default
                // BIOS=0x4317 sets WS0 fast; some games tweak for prefetch).
                if reg == 0x204 {
                    self.waitstates.write_waitcnt(self.io.waitcnt);
                }
            }
            0x5 => self.ppu.write_palette16(addr, val),
            0x6 => self.ppu.write_vram16(addr, val),
            0x7 => self.ppu.write_oam16(addr, val),
            0x8..=0xD => {
                let off = (addr as usize) & 0x01FF_FFFF;
                // EEPROM writes.
                if self.save.save_type == crate::save::SaveType::Eeprom
                    && (region == 0xD || off >= 0x01FF_FF00)
                {
                    self.save.write8(addr, val as u8);
                    return;
                }
                // GPIO/RTC at 0x080000C4-0x080000C8.
                if off >= 0xC4 && off <= 0xC8 {
                    self.gpio.write16(addr, val);
                }
            }
            0xE | 0xF => {
                // Save region is 8-bit bus; 16-bit writes store low byte.
                self.save.write8(addr, val as u8);
            }
            _ => {}
        }
    }

    // ---- 32-bit ----

    pub fn read32(&mut self, addr: u32, pc: u32) -> u32 {
        self.pending_extra_cycles += self.charge_data(addr & !3, true);
        self.read32_inner(addr, pc)
    }

    fn read32_inner(&mut self, addr: u32, pc: u32) -> u32 {
        let addr = addr & !3;
        let region = (addr >> 24) & 0xF;
        let v = match region {
            0x0 | 0x1 => {
                if addr < 0x4000 {
                    // BIOS protection: reads only allowed when PC is in BIOS.
                    if pc < 0x4000 {
                        let i = addr as usize;
                        let v = u32::from_le_bytes(self.bios[i..i + 4].try_into().unwrap());
                        self.bios_last_fetch = v;
                        v
                    } else {
                        self.bios_last_fetch
                    }
                } else {
                    self.open_bus
                }
            }
            0x2 => {
                let i = (addr as usize) & (EWRAM_SIZE - 1);
                u32::from_le_bytes(self.ewram[i..i + 4].try_into().unwrap())
            }
            0x3 => {
                let i = (addr as usize) & (IWRAM_SIZE - 1);
                u32::from_le_bytes(self.iwram[i..i + 4].try_into().unwrap())
            }
            0x4 => {
                // 32-bit I/O reads: compose from two 16-bit reads.
                let lo = self.read_io16(addr) as u32;
                let hi = self.read_io16(addr + 2) as u32;
                lo | (hi << 16)
            }
            0x5 => self.ppu.read_palette32(addr),
            0x6 => self.ppu.read_vram32(addr),
            0x7 => self.ppu.read_oam32(addr),
            0x8..=0xD => {
                let off = (addr as usize) & 0x01FF_FFFF;
                // EEPROM: mapped at top of ROM address space.
                if self.save.save_type == crate::save::SaveType::Eeprom
                    && (region == 0xD || off >= 0x01FF_FF00)
                {
                    return self.save.read8(addr) as u32;
                }
                // GPIO/RTC at 0x080000C4-0x080000C8.
                if off >= 0xC4 && off <= 0xC8 && self.gpio.readable {
                    let lo = self.gpio.read16(addr) as u32;
                    let hi = self.gpio.read16(addr + 2) as u32;
                    lo | (hi << 16)
                } else
                if off + 4 <= self.rom.len() {
                    u32::from_le_bytes(self.rom[off..off + 4].try_into().unwrap())
                } else {
                    let lo = (addr >> 1) & 0xFFFF;
                    let hi = ((addr >> 1) + 1) & 0xFFFF;
                    lo | (hi << 16)
                }
            }
            0xE | 0xF => {
                let b = self.save.read8(addr) as u32;
                b * 0x01010101 // 8-bit bus mirrors byte across all lanes
            }
            _ => self.open_bus,
        };
        self.open_bus = v;
        v
    }

    pub fn write32(&mut self, addr: u32, val: u32) {
        let addr = addr & !3;
        self.pending_extra_cycles += self.charge_data(addr, true);
        // Handle FIFO 32-bit writes directly (they're special).
        let reg = addr & 0xFFF;
        if (addr >> 24) & 0xF == 4 && (reg == 0x0A0 || reg == 0x0A4) {
            self.apu.write_fifo_32(addr, val);
            return;
        }
        self.write16_inner(addr, val as u16);
        self.write16_inner(addr + 2, (val >> 16) as u16);
    }

    /// Read a 16-bit I/O register, routing to the correct subsystem.
    fn read_io16(&self, addr: u32) -> u16 {
        // Internal memory control at 0x04000800 (outside normal I/O mirror range).
        if addr & 0xFFFF == 0x0800 {
            return 0x0020; // low 16 of default 0x0D000020
        }
        if addr & 0xFFFF == 0x0802 {
            return 0x0D00; // high 16
        }

        let reg = addr & 0x3FE;
        match reg {
            // Sound registers.
            0x060..=0x0A6 => self.apu.read_reg(addr),

            // Timer registers.
            0x100 => self.timers.read_counter(0),
            0x102 => self.timers.read_control(0),
            0x104 => self.timers.read_counter(1),
            0x106 => self.timers.read_control(1),
            0x108 => self.timers.read_counter(2),
            0x10A => self.timers.read_control(2),
            0x10C => self.timers.read_counter(3),
            0x10E => self.timers.read_control(3),

            // DMA control (write-only src/dst/count, readable control).
            0x0BA => self.dma.channels[0].control,
            0x0C6 => self.dma.channels[1].control,
            0x0D2 => self.dma.channels[2].control,
            0x0DE => self.dma.channels[3].control,

            // Everything else goes through io.read16.
            _ => self.io.read16(reg as u32, &self.ppu),
        }
    }

    /// Advance the system by `cycles` master clocks. Drains scheduler events.
    pub fn tick(&mut self, cycles: u32) {
        self.sched.advance(cycles);

        // Tick timers FIRST so the APU sees FIFO transitions with their
        // exact cycle timestamps before it generates samples for this batch.
        // Samples that close before a FIFO pop's cycle must see the old DAC
        // latch; samples after see the new one. Without this interleave,
        // all pops stamp to the batch boundary and DAC phase leaks into PSG.
        let timer_overflows = self.timers.tick(cycles);
        for i in 0..4 {
            if timer_overflows[i].count > 0 && self.timers.t[i].irq_enabled() {
                self.io.if_ |= 1 << (3 + i);
            }
        }

        let sample_clock_base = self.apu.sample_clock;
        let ov0 = timer_overflows[0];
        let ov1 = timer_overflows[1];
        let period0 = self.timers.period_cycles(0);
        let period1 = self.timers.period_cycles(1);
        let mut k0: u32 = 0;
        let mut k1: u32 = 0;
        loop {
            let next0 = if k0 < ov0.count {
                Some(ov0.first_cycle.saturating_add(period0.saturating_mul(k0)))
            } else { None };
            let next1 = if k1 < ov1.count {
                Some(ov1.first_cycle.saturating_add(period1.saturating_mul(k1)))
            } else { None };
            let (cyc, i) = match (next0, next1) {
                (None, None) => break,
                (Some(c), None) => { k0 += 1; (c, 0) }
                (None, Some(c)) => { k1 += 1; (c, 1) }
                (Some(a), Some(b)) => if a <= b { k0 += 1; (a, 0) } else { k1 += 1; (b, 1) }
            };
            let cyc = cyc.min(cycles);
            self.apu.tick_until(sample_clock_base.wrapping_add(cyc));
            self.apu.on_timer_overflow(i);
            if self.apu.fifo_a_needs_data() || self.apu.fifo_b_needs_data() {
                self.check_sound_fifo_dma(i);
            }
        }

        for i in 2..4 {
            if timer_overflows[i].count > 0 {
                self.apu.on_timer_overflow(i);
            }
        }

        self.apu.tick_until(sample_clock_base.wrapping_add(cycles));

        while let Some(ev) = self.sched.pop_due() {
            use crate::scheduler::Event;
            match ev {
                Event::HDraw => {
                    // Start of new visible scanline (end of HBlank).
                    self.ppu.on_hdraw(&mut self.io, &mut self.sched);
                    // Check for VBlank DMA at start of line 160.
                    if self.ppu.vcount == 160 {
                        self.run_dma(DmaTiming::VBlank);
                    }
                }
                Event::HBlank => {
                    // End of visible period, entering HBlank.
                    self.ppu.on_hblank(&mut self.io, &mut self.sched);
                    // HBlank DMA fires during HBlank of visible lines.
                    if self.ppu.vcount < 160 {
                        self.run_dma(DmaTiming::HBlank);
                    }
                }
                Event::TimerOverflow(_) => {} // handled above
            }
        }
    }

    fn check_sound_fifo_dma(&mut self, timer_id: usize) {
        // Either DMA1 or DMA2 may target either FIFO. Match by destination
        // address, not channel number, and only fire when that FIFO has
        // actually dropped to half-empty — otherwise internal_src walks
        // forward 16× too fast.
        for ch in 1..=2 {
            if !self.dma.channels[ch].enabled() { continue; }
            if self.dma.channels[ch].timing() != DmaTiming::Special { continue; }

            let dst = self.dma.channels[ch].dst;
            let (fifo_addr, fifo_timer, needs_data) = match dst {
                0x0400_00A0 => (
                    0x0400_00A0,
                    if self.apu.soundcnt_h & (1 << 10) != 0 { 1 } else { 0 },
                    self.apu.fifo_a_needs_data(),
                ),
                0x0400_00A4 => (
                    0x0400_00A4,
                    if self.apu.soundcnt_h & (1 << 14) != 0 { 1 } else { 0 },
                    self.apu.fifo_b_needs_data(),
                ),
                _ => continue,
            };

            if fifo_timer != timer_id { continue; }
            if !needs_data { continue; }

            // Hardware fixed: 4 words (16 bytes) per FIFO request.
            let access = DmaBusAccess {
                ewram: &mut self.ewram[..],
                iwram: &mut self.iwram[..],
                palette: &mut self.ppu.palette[..],
                vram: &mut self.ppu.vram[..],
                oam: &mut self.ppu.oam[..],
                rom: &self.rom,
                sram: &mut self.sram[..],
            };

            let src = self.dma.channels[ch].internal_src;
            for i in 0..4u32 {
                let val = access.read32(src + i * 4);
                self.apu.write_fifo_32(fifo_addr, val);
            }
            self.dma.channels[ch].internal_src = src.wrapping_add(16);
        }
    }

    /// Run all DMA channels matching the given timing trigger.
    pub fn run_dma(&mut self, timing: DmaTiming) {
        self.dma_running = true;

        for ch in 0..4 {
            if !self.dma.channels[ch].enabled() || self.dma.channels[ch].timing() != timing {
                continue;
            }

            // Copy channel state to avoid borrow conflict with bus methods.
            let word_size: u32 = if self.dma.channels[ch].is_32bit() { 4 } else { 2 };
            let src_step = match self.dma.channels[ch].src_adj() {
                0 => word_size as i32, 1 => -(word_size as i32), 2 => 0, _ => word_size as i32,
            };
            let dst_step = match self.dma.channels[ch].dst_adj() {
                0 | 3 => word_size as i32, 1 => -(word_size as i32), 2 => 0, _ => word_size as i32,
            };
            let count = self.dma.channels[ch].internal_count;
            let mut src = self.dma.channels[ch].internal_src;
            let mut dst = self.dma.channels[ch].internal_dst;
            let is_32bit = self.dma.channels[ch].is_32bit();

            // Run the transfer through Bus methods (handles I/O writes correctly).
            for _ in 0..count {
                if is_32bit {
                    let val = self.read32(src, 0);
                    self.write32(dst, val);
                } else {
                    let val = self.read16(src, 0);
                    self.write16(dst, val);
                }
                src = (src as i32).wrapping_add(src_step) as u32;
                dst = (dst as i32).wrapping_add(dst_step) as u32;
            }

            // Write back channel state.
            self.dma.channels[ch].internal_src = src;
            self.dma.channels[ch].internal_dst = dst;
            if self.dma.channels[ch].dst_adj() == 3 {
                self.dma.channels[ch].internal_dst = self.dma.channels[ch].dst;
            }
            if !self.dma.channels[ch].repeat() {
                self.dma.channels[ch].control &= !0x8000;
            } else {
                let cnt = self.dma.channels[ch].count;
                self.dma.channels[ch].internal_count = if cnt == 0 {
                    if ch == 3 { 0x10000 } else { 0x4000 }
                } else { cnt };
            }

            if self.dma.channels[ch].irq() {
                self.io.if_ |= 1 << (8 + ch);
            }
        }

        self.dma_running = false;

        // A DMA transfer may have written to DMA control registers, enabling
        // new immediate-mode channels. Drain them now (non-recursively).
        if timing != DmaTiming::Immediate {
            let any_pending = (0..4).any(|ch| {
                self.dma.channels[ch].enabled()
                    && self.dma.channels[ch].timing() == DmaTiming::Immediate
            });
            if any_pending {
                self.run_immediate_dma();
            }
        }
    }

    /// Run immediate DMA transfers (called after DMA control write).
    pub fn run_immediate_dma(&mut self) {
        // Loop until no more immediate DMAs are pending (a transfer can
        // enable another channel by writing to DMA control registers).
        loop {
            let any = (0..4).any(|ch| {
                self.dma.channels[ch].enabled()
                    && self.dma.channels[ch].timing() == DmaTiming::Immediate
            });
            if !any { break; }
            self.run_dma(DmaTiming::Immediate);
        }
    }
}
