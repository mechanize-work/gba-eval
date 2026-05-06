//! ARM7TDMI. Two instruction sets (ARM 32-bit, Thumb 16-bit), seven modes,
//! banked registers, 3-stage pipeline (we model the PC offset only).

mod arm;
mod thumb;
mod alu;

use crate::bus::Bus;
use bitflags::bitflags;

pub const SP: usize = 13;
pub const LR: usize = 14;
pub const PC: usize = 15;

bitflags! {
    #[derive(Clone, Copy, Debug, Default)]
    pub struct Cpsr: u32 {
        const N = 1 << 31;  // negative
        const Z = 1 << 30;  // zero
        const C = 1 << 29;  // carry
        const V = 1 << 28;  // overflow
        const I = 1 << 7;   // IRQ disable
        const F = 1 << 6;   // FIQ disable
        const T = 1 << 5;   // Thumb state
        // bits 4:0 = mode
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Mode {
    User       = 0b10000,
    Fiq        = 0b10001,
    Irq        = 0b10010,
    Supervisor = 0b10011,
    Abort      = 0b10111,
    Undefined  = 0b11011,
    System     = 0b11111,
}

impl Mode {
    fn from_bits(b: u32) -> Self {
        match b & 0x1F {
            0b10000 => Mode::User,
            0b10001 => Mode::Fiq,
            0b10010 => Mode::Irq,
            0b10011 => Mode::Supervisor,
            0b10111 => Mode::Abort,
            0b11011 => Mode::Undefined,
            0b11111 => Mode::System,
            // Real HW behaviour for invalid modes is unpredictable; collapse to System.
            _ => Mode::System,
        }
    }

    pub fn bank(self) -> usize {
        match self {
            Mode::User | Mode::System => 0,
            Mode::Fiq => 1,
            Mode::Irq => 2,
            Mode::Supervisor => 3,
            Mode::Abort => 4,
            Mode::Undefined => 5,
        }
    }
}

pub struct Cpu {
    /// r0-r15 as currently visible. r15 holds PC+8 (ARM) or PC+4 (Thumb)
    /// at the point an instruction reads it — we maintain this invariant
    /// by adding the offset after every fetch and re-flushing on branch.
    pub r: [u32; 16],
    pub cpsr: Cpsr,
    mode: Mode,

    /// Banked r13/r14 for each mode (User/Sys share index 0).
    pub banked_r13: [u32; 6],
    pub banked_r14: [u32; 6],
    /// FIQ additionally banks r8-r12.
    banked_fiq_r8_12: [u32; 5],
    saved_usr_r8_12: [u32; 5],
    /// One SPSR per privileged mode (index by bank, [0] unused).
    spsr: [Cpsr; 6],

    /// Two-slot pipeline of fetched-but-not-executed instructions.
    pipe: [u32; 2],
    /// Set by flush_pipeline; cleared by step after exec.
    pipeline_flushed: bool,

    /// Interrupt flags that IntrWait/VBlankIntrWait are waiting on.
    pub intr_wait_flags: u16,

    pub irq_delay: u8,
    pub cycles: u64,
    pub halted: bool,
    pub irq_count: u32,
    #[cfg(feature = "swi-trace")]
    pub swi_calls: [u32; 256],
    // Histogram of IF bits at the moment of IRQ-take. IRQs can stack
    // (HBlank pending while VBlank is being serviced) so the IF value
    // may have multiple bits — what matters here is which bit gates
    // through IE. Lets us split irq_count into source contributions
    // without per-IRQ logging overhead.
    #[cfg(feature = "swi-trace")]
    pub irq_by_source: [u32; 14],
}

impl Cpu {
    pub fn new() -> Self {
        let mut c = Self {
            r: [0; 16],
            cpsr: Cpsr::from_bits_retain(Mode::Supervisor as u32) | Cpsr::I | Cpsr::F,
            mode: Mode::Supervisor,
            banked_r13: [0; 6],
            banked_r14: [0; 6],
            banked_fiq_r8_12: [0; 5],
            saved_usr_r8_12: [0; 5],
            spsr: [Cpsr::default(); 6],
            pipe: [0; 2],
            pipeline_flushed: false,
            intr_wait_flags: 0,
            irq_delay: 0,
            irq_count: 0,
            cycles: 0,
            halted: false,
            #[cfg(feature = "swi-trace")]
            swi_calls: [0; 256],
            #[cfg(feature = "swi-trace")]
            irq_by_source: [0; 14],
        };
        // Skip-BIOS defaults (overwritten if a real BIOS is loaded and we
        // start at 0x0 instead).
        c.r[SP] = 0x03007F00;
        c.banked_r13[Mode::Irq.bank()] = 0x03007FA0;
        c.banked_r13[Mode::Supervisor.bank()] = 0x03007FE0;
        c.r[PC] = 0x08000000;
        c.cpsr = Cpsr::from_bits_retain(Mode::System as u32);
        c.mode = Mode::System;
        c
    }

    #[inline] pub fn cpsr_bits(&self) -> u32 { self.cpsr.bits() | self.mode as u32 }
    #[inline] pub fn thumb(&self) -> bool { self.cpsr.contains(Cpsr::T) }
    #[inline] pub fn carry(&self) -> bool { self.cpsr.contains(Cpsr::C) }

    pub fn set_cpsr_bits(&mut self, v: u32, mask: u32) {
        let cur = self.cpsr_bits();
        let new = (cur & !mask) | (v & mask);
        // Mode change?
        if mask & 0x1F != 0 {
            let new_mode = Mode::from_bits(new);
            self.switch_mode(new_mode);
        }
        self.cpsr = Cpsr::from_bits_retain(new & !0x1F);
    }

    pub fn spsr_bits(&self) -> u32 {
        self.spsr[self.mode.bank()].bits()
    }

    pub fn set_spsr_bits(&mut self, v: u32, mask: u32) {
        let b = self.mode.bank();
        if b == 0 { return; } // User/System has no SPSR
        let cur = self.spsr[b].bits();
        self.spsr[b] = Cpsr::from_bits_retain((cur & !mask) | (v & mask));
    }

    fn switch_mode(&mut self, new: Mode) {
        if new == self.mode { return; }
        let old_bank = self.mode.bank();
        let new_bank = new.bank();

        // Save current r13/r14 into old bank, load from new bank.
        if old_bank != new_bank {
            self.banked_r13[old_bank] = self.r[13];
            self.banked_r14[old_bank] = self.r[14];
            self.r[13] = self.banked_r13[new_bank];
            self.r[14] = self.banked_r14[new_bank];
        }

        // FIQ banks r8-r12.
        let was_fiq = self.mode == Mode::Fiq;
        let is_fiq = new == Mode::Fiq;
        if was_fiq != is_fiq {
            if is_fiq {
                self.saved_usr_r8_12.copy_from_slice(&self.r[8..13]);
                self.r[8..13].copy_from_slice(&self.banked_fiq_r8_12);
            } else {
                self.banked_fiq_r8_12.copy_from_slice(&self.r[8..13]);
                self.r[8..13].copy_from_slice(&self.saved_usr_r8_12);
            }
        }

        self.mode = new;
    }

    /// Refill the pipeline after PC was modified.
    pub fn flush_pipeline(&mut self, bus: &mut Bus) {
        self.pipeline_flushed = true;
        // Branch target: first fetch is non-sequential. Mirror mesen's
        // ReloadPipeline which clears Sequential before the fetch.
        bus.sequential = false;
        // Three code fetches per branch (target, target+step, and a
        // trailing sequential fetch). The two-slot pipeline here defers
        // the trailing fetch to the next step()'s refill; cumulative
        // cycle counts match.
        if self.thumb() {
            self.r[PC] &= !1;
            self.pipe[0] = bus.fetch16(self.r[PC]) as u32;
            self.pipe[1] = bus.fetch16(self.r[PC].wrapping_add(2)) as u32;
            self.r[PC] = self.r[PC].wrapping_add(4);
            // 3rd fetch — drive the bus for its cycle cost, discard result.
            // Next step() will re-fetch into pipe[1] via the normal refill
            // path; since prefetch isn't modeled, we're not double-servicing
            // any cached buffer. The fetch DOES update Sequential, which
            // lines up with mesen's post-ProcessPipeline state.
            let _ = bus.fetch16(self.r[PC]);
        } else {
            self.r[PC] &= !3;
            self.pipe[0] = bus.fetch32(self.r[PC]);
            self.pipe[1] = bus.fetch32(self.r[PC].wrapping_add(4));
            self.r[PC] = self.r[PC].wrapping_add(8);
            let _ = bus.fetch32(self.r[PC]);
        }
    }

    #[inline]
    fn check_cond(&self, cond: u32) -> bool {
        let f = self.cpsr;
        match cond {
            0x0 => f.contains(Cpsr::Z),                                      // EQ
            0x1 => !f.contains(Cpsr::Z),                                     // NE
            0x2 => f.contains(Cpsr::C),                                      // CS
            0x3 => !f.contains(Cpsr::C),                                     // CC
            0x4 => f.contains(Cpsr::N),                                      // MI
            0x5 => !f.contains(Cpsr::N),                                     // PL
            0x6 => f.contains(Cpsr::V),                                      // VS
            0x7 => !f.contains(Cpsr::V),                                     // VC
            0x8 => f.contains(Cpsr::C) && !f.contains(Cpsr::Z),              // HI
            0x9 => !f.contains(Cpsr::C) || f.contains(Cpsr::Z),              // LS
            0xA => f.contains(Cpsr::N) == f.contains(Cpsr::V),               // GE
            0xB => f.contains(Cpsr::N) != f.contains(Cpsr::V),               // LT
            0xC => !f.contains(Cpsr::Z) && f.contains(Cpsr::N) == f.contains(Cpsr::V), // GT
            0xD => f.contains(Cpsr::Z) || f.contains(Cpsr::N) != f.contains(Cpsr::V),  // LE
            0xE => true,                                                     // AL
            _ => false, // 0xF: NV (used for some ARMv5 unconditional ops, none on v4T)
        }
    }

    /// Execute one instruction. Returns cycles consumed.
    pub fn step(&mut self, bus: &mut Bus) -> u32 {
        if self.halted {
            // GBA wakes from halt when IE & IF != 0, regardless of IME/CPSR.I.
            if (bus.io.ie & bus.io.if_) != 0 {
                self.halted = false;
            } else {
                return 1;
            }
        }

        // IRQ check: IE & IF matched, IME set, CPSR.I clear.
        // Skip for 1 instruction after returning from IRQ (real HW has a 1-insn delay).
        if self.irq_delay > 0 {
            self.irq_delay -= 1;
        } else if (bus.io.ie & bus.io.if_) != 0 && bus.io.ime && !self.cpsr.contains(Cpsr::I) {
            #[cfg(feature = "swi-trace")]
            {
                // Lowest set bit in (IE & IF) — that's what the handler will
                // service first. Multiple bits CAN be set (HBlank stacked
                // behind VBlank) but the entry is for the priority winner.
                let pending = bus.io.ie & bus.io.if_;
                let bit = pending.trailing_zeros() as usize;
                if bit < 14 {
                    self.irq_by_source[bit] += 1;
                }
            }
            self.take_irq(bus);
            return 4;
        }

        let opcode = self.pipe[0];
        self.pipe[0] = self.pipe[1];

        // Reset flag before exec — only instructions that branch will set it.
        self.pipeline_flushed = false;

        // Advance the pipeline AFTER execution so that during exec,
        // r[PC] = insn_addr + 8 (ARM) or insn_addr + 4 (Thumb).
        if self.thumb() {
            let cycles = self.exec_thumb(opcode as u16, bus);
            if !self.pipeline_flushed {
                self.pipe[1] = bus.fetch16(self.r[PC]) as u32;
                self.r[PC] = self.r[PC].wrapping_add(2);
            }
            cycles
        } else {
            let cycles = if self.check_cond(opcode >> 28) {
                self.exec_arm(opcode, bus)
            } else {
                1
            };
            if !self.pipeline_flushed {
                self.pipe[1] = bus.fetch32(self.r[PC]);
                self.r[PC] = self.r[PC].wrapping_add(4);
            }
            cycles
        }
    }

    fn take_irq(&mut self, bus: &mut Bus) {
        {
            // Jump to IRQ vector at 0x18. Our BIOS stub handles the dispatch.
            // IRQ is checked BEFORE the current instruction executes, so we
            // need to return to the CURRENT instruction (not the next one).
            // LR_irq = insn_addr + 4. SUBS PC, LR, #4 → insn_addr.
            let ret = if self.thumb() { self.r[PC] } else { self.r[PC].wrapping_sub(4) };
            let old_cpsr = self.cpsr_bits();
            self.irq_count += 1;
            self.switch_mode(Mode::Irq);
            self.spsr[Mode::Irq.bank()] = Cpsr::from_bits_retain(old_cpsr);
            self.r[LR] = ret;
            self.cpsr.remove(Cpsr::T);
            self.cpsr.insert(Cpsr::I);
            self.r[PC] = 0x18;
            self.flush_pipeline(bus);
        }
    }

    #[inline]
    pub fn set_nz(&mut self, v: u32) {
        self.cpsr.set(Cpsr::N, v & 0x8000_0000 != 0);
        self.cpsr.set(Cpsr::Z, v == 0);
    }
}
