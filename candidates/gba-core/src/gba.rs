//! Top-level GBA system. Owns CPU + Bus, drives the main loop.

use crate::bus::Bus;
use crate::cpu::{Cpu, Cpsr, Mode};
use crate::ppu::FB_SIZE;

const MAX_CYCLES_PER_FRAME: u64 = 280896 * 4;

pub struct Gba {
    pub cpu: Cpu,
    pub bus: Bus,
    frame_sequencer_counter: u32,
    frame_sequencer_step: u8,
}

impl Gba {
    pub fn new() -> Self {
        Self {
            cpu: Cpu::new(),
            bus: Bus::new(),
            frame_sequencer_counter: 0,
            frame_sequencer_step: 0,
        }
    }

    pub fn load_rom(&mut self, data: Vec<u8>) {
        self.bus.load_rom(data);
    }

    pub fn load_bios(&mut self, data: &[u8]) {
        self.bus.load_bios(data);

        self.cpu.r[15] = 0;
        self.cpu.cpsr = Cpsr::from_bits_retain(Mode::Supervisor as u32) | Cpsr::I | Cpsr::F;
        self.cpu.flush_pipeline(&mut self.bus);
    }

    /// The BIOS stub — 16 KiB ARM image with SWI handlers + IRQ wrapper.
    /// Embedded at compile time so skip_bios() can install it without an
    /// external file path.
    const BIOS_STUB: &'static [u8; 0x4000] =
        include_bytes!("../../../spec/gba_bios_stub.bin");

    pub fn skip_bios(&mut self) {
        self.cpu = Cpu::new();
        // Load the full BIOS stub so SWI dispatch works via real ARM
        // execution — no HLE needed.
        self.bus.load_bios(Self::BIOS_STUB);

        // Leave WAITCNT at hardware-reset default (0); games write
        // their preferred value during early init.
        self.cpu.flush_pipeline(&mut self.bus);
    }

    /// Late-init: enable VBlank IRQ in DISPSTAT once the game has set up
    /// IE bit 0 + IME + a valid handler. Some games enable IE/IME but
    /// forget to write DISPSTAT bit 3; this ensures VBlank IRQs fire.
    fn check_late_irq_init(&mut self) {
        if self.bus.ppu.dispstat & 0x8 != 0 { return; } // already enabled
        if self.bus.io.ie & 1 == 0 { return; } // game doesn't want VBlank
        if !self.bus.io.ime { return; }

        // Handler at [0x03007FFC] must point to valid code (RAM or ROM).
        let handler = u32::from_le_bytes([
            self.bus.iwram[0x7FFC], self.bus.iwram[0x7FFD],
            self.bus.iwram[0x7FFE], self.bus.iwram[0x7FFF],
        ]);
        if handler < 0x02000000 { return; }

        self.bus.ppu.dispstat |= 0x8;
    }

    pub fn run_frame(&mut self) {
        self.check_late_irq_init();
        // Wait for the NEXT VBlank transition (frame_ready going false→true).
        // First, clear frame_ready and wait until it's set.
        self.bus.ppu.frame_ready = false;
        let start = self.bus.sched.now;

        loop {
            if self.bus.sched.now - start > MAX_CYCLES_PER_FRAME as u64 { break; }

            let cycles;
            if self.cpu.halted {
                cycles = self.bus.sched.cycles_until_next().max(1);
                self.bus.tick(cycles);
            } else {
                self.bus.pending_extra_cycles = 0;
                let base = self.cpu.step(&mut self.bus);
                cycles = base + self.bus.pending_extra_cycles;
                self.bus.tick(cycles);
                if self.bus.io.haltcnt {
                    self.bus.io.haltcnt = false;
                    self.cpu.halted = true;
                }
            }

            // Break on frame_ready (VCOUNT=160).
            if self.bus.ppu.frame_ready {
                break;
            }

            // Frame sequencer for APU (512 Hz).
            self.frame_sequencer_counter += cycles;
            while self.frame_sequencer_counter >= 32768 {
                self.frame_sequencer_counter -= 32768;
                self.bus.apu.frame_sequencer_step(self.frame_sequencer_step);
                self.frame_sequencer_step = (self.frame_sequencer_step + 1) & 7;
            }

            // Wake CPU from halt if IRQ pending.
            if self.cpu.halted {
                let pending = self.bus.io.ie & self.bus.io.if_;
                if pending != 0 {
                    if self.cpu.intr_wait_flags != 0 {
                        if pending & self.cpu.intr_wait_flags != 0 {
                            self.cpu.halted = false;
                            // Clear matched bits in BIOS_IF (0x03007FF8),
                            // matching what the real BIOS IntrWait does.
                            let bios_if_off = 0x7FF8usize;
                            let bios_if = u16::from_le_bytes([
                                self.bus.iwram[bios_if_off],
                                self.bus.iwram[bios_if_off + 1],
                            ]);
                            let cleared = bios_if & !self.cpu.intr_wait_flags;
                            self.bus.iwram[bios_if_off] = cleared as u8;
                            self.bus.iwram[bios_if_off + 1] = (cleared >> 8) as u8;
                            self.cpu.intr_wait_flags = 0;
                        }
                    } else {
                        self.cpu.halted = false;
                    }
                }
            }

        }
    }

    /// Run for at least `target_cycles` CPU cycles, ignoring frame
    /// boundaries. Useful for audio comparisons where frame-boundary
    /// exit timing would interfere with sample alignment.
    pub fn run_cycles(&mut self, target_cycles: u64) {
        self.check_late_irq_init();
        let start = self.bus.sched.now;

        while self.bus.sched.now - start < target_cycles {
            let cycles;
            if self.cpu.halted {
                let until_event = self.bus.sched.cycles_until_next().max(1);
                let until_target = (target_cycles - (self.bus.sched.now - start)) as u32;
                cycles = until_event.min(until_target.max(1));
                self.bus.tick(cycles);
            } else {
                self.bus.pending_extra_cycles = 0;
                let base = self.cpu.step(&mut self.bus);
                cycles = base + self.bus.pending_extra_cycles;
                self.bus.tick(cycles);
                if self.bus.io.haltcnt {
                    self.bus.io.haltcnt = false;
                    self.cpu.halted = true;
                }
            }

            self.frame_sequencer_counter += cycles;
            while self.frame_sequencer_counter >= 32768 {
                self.frame_sequencer_counter -= 32768;
                self.bus.apu.frame_sequencer_step(self.frame_sequencer_step);
                self.frame_sequencer_step = (self.frame_sequencer_step + 1) & 7;
            }

            if self.cpu.halted {
                let pending = self.bus.io.ie & self.bus.io.if_;
                if pending != 0 {
                    if self.cpu.intr_wait_flags != 0 {
                        if pending & self.cpu.intr_wait_flags != 0 {
                            self.cpu.halted = false;
                            let bios_if_off = 0x7FF8usize;
                            let bios_if = u16::from_le_bytes([
                                self.bus.iwram[bios_if_off],
                                self.bus.iwram[bios_if_off + 1],
                            ]);
                            let cleared = bios_if & !self.cpu.intr_wait_flags;
                            self.bus.iwram[bios_if_off] = cleared as u8;
                            self.bus.iwram[bios_if_off + 1] = (cleared >> 8) as u8;
                            self.cpu.intr_wait_flags = 0;
                        }
                    } else {
                        self.cpu.halted = false;
                    }
                }
            }
        }
    }

    pub fn framebuffer(&self) -> &[u32; FB_SIZE] {
        &self.bus.ppu.framebuffer
    }

    pub fn set_keyinput(&mut self, keys: u16) {
        self.bus.io.keyinput = keys;
    }

    /// Execute exactly one instruction (or one halt-idle). Returns cycles
    /// consumed. Useful for per-instruction divergence diffing against
    /// another emulator.
    pub fn step_one(&mut self) -> u32 {
        self.check_late_irq_init();
        let cycles;
        if self.cpu.halted {
            cycles = self.bus.sched.cycles_until_next().max(1);
            self.bus.tick(cycles);
        } else {
            self.bus.pending_extra_cycles = 0;
            let base = self.cpu.step(&mut self.bus);
            cycles = base + self.bus.pending_extra_cycles;
            self.bus.tick(cycles);
            if self.bus.io.haltcnt {
                self.bus.io.haltcnt = false;
                self.cpu.halted = true;
            }
        }
        self.frame_sequencer_counter += cycles;
        while self.frame_sequencer_counter >= 32768 {
            self.frame_sequencer_counter -= 32768;
            self.bus.apu.frame_sequencer_step(self.frame_sequencer_step);
            self.frame_sequencer_step = (self.frame_sequencer_step + 1) & 7;
        }
        if self.cpu.halted {
            let pending = self.bus.io.ie & self.bus.io.if_;
            if pending != 0 && self.cpu.intr_wait_flags == 0 {
                self.cpu.halted = false;
            }
        }
        cycles
    }

    pub fn pc(&self) -> u32 { self.cpu.r[15] }
    pub fn master_clock(&self) -> u64 { self.bus.sched.now }
}
