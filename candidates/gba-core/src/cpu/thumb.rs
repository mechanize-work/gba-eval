//! Thumb (16-bit) instruction decoder and executor.
//!
//! Thumb instruction formats (by bits 15:8 roughly):
//!   Format 1:  Move shifted register        (000xxyyy)
//!   Format 2:  Add/subtract                 (000110--)
//!   Format 3:  Move/compare/add/sub imm     (001xxxxx)
//!   Format 4:  ALU operations               (010000xx)
//!   Format 5:  Hi register ops / BX         (010001xx)
//!   Format 6:  PC-relative load             (01001xxx)
//!   Format 7:  Load/store register offset   (0101xx0x)
//!   Format 8:  Load/store sign-extended     (0101xx1x)
//!   Format 9:  Load/store imm offset        (011xxxxx)
//!   Format 10: Load/store halfword          (1000xxxx)
//!   Format 11: SP-relative load/store       (1001xxxx)
//!   Format 12: Load address (PC/SP + imm)   (1010xxxx)
//!   Format 13: Adjust stack pointer          (10110000)
//!   Format 14: Push/pop registers           (1011x10x)
//!   Format 15: Multiple load/store          (1100xxxx)
//!   Format 16: Conditional branch           (1101xxxx)
//!   Format 17: SWI                          (11011111)
//!   Format 18: Unconditional branch         (11100xxx)
//!   Format 19: Long branch with link        (1111xxxx)

use super::{Cpu, Cpsr, Mode, PC, LR, SP};
use super::alu;
use crate::bus::Bus;

impl Cpu {
    pub fn exec_thumb(&mut self, op: u16, bus: &mut Bus) -> u32 {
        let hi = op >> 8;

        match hi >> 5 {
            // 000: Format 1 (shifted register) or Format 2 (add/sub)
            0b000 => {
                if op >> 11 == 0b00011 {
                    self.thumb_add_sub(op)
                } else {
                    self.thumb_move_shifted(op)
                }
            }
            // 001: Format 3 (imm operations)
            0b001 => self.thumb_imm_op(op),
            // 010: Format 4 (ALU), 5 (Hi-reg/BX), 6 (PC-relative load)
            // Also catches 0101 (format 7/8) since bits 15:13 = 010.
            0b010 => {
                match op >> 12 {
                    0b0100 => {
                        // bits 15:10 distinguish format 4 vs 5.
                        if op >> 10 == 0b010000 {
                            self.thumb_alu(op, bus)
                        } else if op >> 10 == 0b010001 {
                            self.thumb_hireg_bx(op, bus)
                        } else {
                            // op >> 11 == 0b01001: Format 6 (PC-relative load)
                            self.thumb_pc_relative_load(op, bus)
                        }
                    }
                    0b0101 => {
                        // Format 7 (register offset load/store) or 8 (sign-extended).
                        // Bit 9 selects: 0 = format 7, 1 = format 8.
                        if op & (1 << 9) != 0 {
                            self.thumb_load_store_sign(op, bus)
                        } else {
                            self.thumb_load_store_reg(op, bus)
                        }
                    }
                    _ => 1, // shouldn't happen
                }
            }
            // 011: Format 9 (load/store imm offset)
            0b011 => self.thumb_load_store_imm(op, bus),
            // 100: Format 10/11
            0b100 => {
                if op >> 12 == 0b1000 {
                    self.thumb_load_store_halfword(op, bus)
                } else {
                    self.thumb_sp_relative(op, bus)
                }
            }
            // 101: Format 12/13/14
            0b101 => {
                if op >> 12 == 0b1010 {
                    self.thumb_load_address(op)
                } else if op >> 8 == 0b10110000 {
                    self.thumb_adjust_sp(op)
                } else if (op >> 12) == 0b1011 && (op >> 9) & 3 == 0b10 {
                    self.thumb_push_pop(op, bus)
                } else {
                    1 // unimplemented
                }
            }
            // 110: Format 15/16/17
            0b110 => {
                if op >> 12 == 0b1100 {
                    self.thumb_multiple(op, bus)
                } else if hi == 0b11011111 {
                    self.thumb_swi(op, bus)
                } else if op >> 12 == 0b1101 {
                    self.thumb_cond_branch(op, bus)
                } else {
                    1
                }
            }
            // 111: Format 18/19
            0b111 => {
                if op >> 11 == 0b11100 {
                    self.thumb_uncond_branch(op, bus)
                } else {
                    self.thumb_long_branch(op, bus)
                }
            }
            _ => 1,
        }
    }

    // ================================================================
    //  Format 1: Move shifted register
    // ================================================================

    fn thumb_move_shifted(&mut self, op: u16) -> u32 {
        let shift_op = (op >> 11) & 3;
        let offset = ((op >> 6) & 0x1F) as u32;
        let rs = ((op >> 3) & 7) as usize;
        let rd = (op & 7) as usize;
        let carry = self.carry();

        let (result, new_carry) = match shift_op {
            0 => alu::lsl(self.r[rs], offset, carry),
            1 => alu::lsr(self.r[rs], offset, carry, true),
            2 => alu::asr(self.r[rs], offset, carry, true),
            _ => unreachable!(),
        };

        self.r[rd] = result;
        self.set_nz(result);
        self.cpsr.set(Cpsr::C, new_carry);
        1
    }

    // ================================================================
    //  Format 2: Add/subtract
    // ================================================================

    fn thumb_add_sub(&mut self, op: u16) -> u32 {
        let i_bit = op & (1 << 10) != 0;
        let sub = op & (1 << 9) != 0;
        let rn_or_imm = ((op >> 6) & 7) as u32;
        let rs = ((op >> 3) & 7) as usize;
        let rd = (op & 7) as usize;

        let operand = if i_bit { rn_or_imm } else { self.r[rn_or_imm as usize] };

        self.r[rd] = if sub {
            self.alu_sub_setflags(self.r[rs], operand)
        } else {
            self.alu_add_setflags(self.r[rs], operand)
        };
        1
    }

    // ================================================================
    //  Format 3: Immediate operations (MOV, CMP, ADD, SUB)
    // ================================================================

    fn thumb_imm_op(&mut self, op: u16) -> u32 {
        let opcode = (op >> 11) & 3;
        let rd = ((op >> 8) & 7) as usize;
        let imm = (op & 0xFF) as u32;

        match opcode {
            0 => {
                // MOV
                self.r[rd] = imm;
                self.set_nz(imm);
            }
            1 => {
                // CMP
                self.alu_sub_setflags(self.r[rd], imm);
            }
            2 => {
                // ADD
                self.r[rd] = self.alu_add_setflags(self.r[rd], imm);
            }
            3 => {
                // SUB
                self.r[rd] = self.alu_sub_setflags(self.r[rd], imm);
            }
            _ => unreachable!(),
        }
        1
    }

    // ================================================================
    //  Format 4: ALU operations
    // ================================================================

    fn thumb_alu(&mut self, op: u16, _bus: &mut Bus) -> u32 {
        let alu_op = (op >> 6) & 0xF;
        let rs = ((op >> 3) & 7) as usize;
        let rd = (op & 7) as usize;
        let a = self.r[rd];
        let b = self.r[rs];

        match alu_op {
            0x0 => { self.r[rd] = a & b; self.set_nz(self.r[rd]); }             // AND
            0x1 => { self.r[rd] = a ^ b; self.set_nz(self.r[rd]); }             // EOR
            0x2 => {                                                              // LSL
                let (v, c) = alu::lsl(a, b & 0xFF, self.carry());
                self.r[rd] = v; self.set_nz(v); self.cpsr.set(Cpsr::C, c);
            }
            0x3 => {                                                              // LSR
                let (v, c) = alu::lsr(a, b & 0xFF, self.carry(), false);
                self.r[rd] = v; self.set_nz(v); self.cpsr.set(Cpsr::C, c);
            }
            0x4 => {                                                              // ASR
                let (v, c) = alu::asr(a, b & 0xFF, self.carry(), false);
                self.r[rd] = v; self.set_nz(v); self.cpsr.set(Cpsr::C, c);
            }
            0x5 => { self.r[rd] = self.alu_adc_setflags(a, b); }                // ADC
            0x6 => { self.r[rd] = self.alu_sbc_setflags(a, b); }                // SBC
            0x7 => {                                                              // ROR
                let (v, c) = alu::ror(a, b & 0xFF, self.carry(), false);
                self.r[rd] = v; self.set_nz(v); self.cpsr.set(Cpsr::C, c);
            }
            0x8 => { let v = a & b; self.set_nz(v); }                           // TST
            0x9 => {                                                              // NEG
                self.r[rd] = self.alu_sub_setflags(0, b);
            }
            0xA => { self.alu_sub_setflags(a, b); }                             // CMP
            0xB => { self.alu_add_setflags(a, b); }                             // CMN
            0xC => { self.r[rd] = a | b; self.set_nz(self.r[rd]); }             // ORR
            0xD => {                                                              // MUL
                self.r[rd] = a.wrapping_mul(b);
                self.set_nz(self.r[rd]);
            }
            0xE => { self.r[rd] = a & !b; self.set_nz(self.r[rd]); }           // BIC
            0xF => { self.r[rd] = !b; self.set_nz(self.r[rd]); }               // MVN
            _ => unreachable!(),
        }

        // Register-shift ops (LSL/LSR/ASR/ROR by register) and MUL add an
        // I-cycle in mesen via Idle() at GbaCpu.Thumb.cpp:79/85/91/100/114.
        // Mesen's pattern: Idle() before the ALU op. Translating to our
        // return-cycles model, that's +1 cycle above baseline.
        if alu_op == 0xD {
            // MUL: variable m cycles based on Rd value scanned for leading
            // 0s/1s (ARM7TDMI behavior). +1 for the Idle call.
            let mul_val = a;
            let masked = mul_val & 0xFFFF_FF00;
            if masked == 0 || masked == 0xFFFF_FF00 { 1 }
            else if (mul_val & 0xFFFF_0000) == 0 || (mul_val & 0xFFFF_0000) == 0xFFFF_0000 { 2 }
            else if (mul_val & 0xFF00_0000) == 0 || (mul_val & 0xFF00_0000) == 0xFF00_0000 { 3 }
            else { 4 }
        } else if matches!(alu_op, 0x2 | 0x3 | 0x4 | 0x7) {
            // LSL / LSR / ASR / ROR by register: +1 I-cycle.
            2
        } else {
            1
        }
    }

    // ================================================================
    //  Format 5: Hi register ops / BX
    // ================================================================

    fn thumb_hireg_bx(&mut self, op: u16, bus: &mut Bus) -> u32 {
        let alu_op = (op >> 8) & 3;
        let h1 = ((op >> 7) & 1) as usize; // high bit for rd
        let h2 = ((op >> 6) & 1) as usize; // high bit for rs
        let rs = ((op >> 3) & 7) as usize | (h2 << 3);
        let rd = (op & 7) as usize | (h1 << 3);

        match alu_op {
            0 => { // ADD
                self.r[rd] = self.r[rd].wrapping_add(self.r[rs]);
                if rd == PC { self.r[PC] &= !1; self.flush_pipeline(bus); return 3; }
            }
            1 => { // CMP
                self.alu_sub_setflags(self.r[rd], self.r[rs]);
            }
            2 => { // MOV
                self.r[rd] = self.r[rs];
                if rd == PC { self.r[PC] &= !1; self.flush_pipeline(bus); return 3; }
            }
            3 => { // BX
                let addr = self.r[rs];
                if addr & 1 != 0 {
                    self.cpsr.insert(Cpsr::T);
                } else {
                    self.cpsr.remove(Cpsr::T);
                }
                self.r[PC] = addr & !1;
                self.flush_pipeline(bus);
                return 3;
            }
            _ => unreachable!(),
        }
        1
    }

    // ================================================================
    //  Format 6: PC-relative load
    // ================================================================

    fn thumb_pc_relative_load(&mut self, op: u16, bus: &mut Bus) -> u32 {
        let rd = ((op >> 8) & 7) as usize;
        let imm = (op & 0xFF) as u32 * 4;
        // PC reads as instruction_addr + 4, word-aligned for this instruction.
        let addr = (self.r[PC] & !3).wrapping_add(imm);
        self.r[rd] = bus.read32(addr, self.r[PC]);
        3
    }

    // ================================================================
    //  Format 7: Load/store register offset
    // ================================================================

    fn thumb_load_store_reg(&mut self, op: u16, bus: &mut Bus) -> u32 {
        let load = op & (1 << 11) != 0;
        let byte = op & (1 << 10) != 0;
        let ro = ((op >> 6) & 7) as usize;
        let rb = ((op >> 3) & 7) as usize;
        let rd = (op & 7) as usize;
        let addr = self.r[rb].wrapping_add(self.r[ro]);

        if load {
            self.r[rd] = if byte {
                bus.read8(addr, self.r[PC]) as u32
            } else {
                let raw = bus.read32(addr, self.r[PC]);
                raw.rotate_right((addr & 3) * 8)
            };
            3
        } else {
            if byte {
                bus.write8(addr, self.r[rd] as u8);
            } else {
                bus.write32(addr, self.r[rd]);
            }
            2
        }
    }

    // ================================================================
    //  Format 8: Load/store sign-extended byte/halfword
    // ================================================================

    fn thumb_load_store_sign(&mut self, op: u16, bus: &mut Bus) -> u32 {
        let h_flag = op & (1 << 11) != 0;
        let sign = op & (1 << 10) != 0;
        let ro = ((op >> 6) & 7) as usize;
        let rb = ((op >> 3) & 7) as usize;
        let rd = (op & 7) as usize;
        let addr = self.r[rb].wrapping_add(self.r[ro]);

        match (sign, h_flag) {
            (false, false) => { // STRH
                bus.write16(addr, self.r[rd] as u16);
                return 2;
            }
            (false, true) => { // LDRH
                self.r[rd] = bus.read16(addr, self.r[PC]) as u32;
            }
            (true, false) => { // LDRSB
                self.r[rd] = bus.read8(addr, self.r[PC]) as i8 as i32 as u32;
            }
            (true, true) => { // LDRSH
                self.r[rd] = bus.read16(addr, self.r[PC]) as i16 as i32 as u32;
            }
        }
        3
    }

    // ================================================================
    //  Format 9: Load/store with immediate offset
    // ================================================================

    fn thumb_load_store_imm(&mut self, op: u16, bus: &mut Bus) -> u32 {
        let byte = op & (1 << 12) != 0;
        let load = op & (1 << 11) != 0;
        let offset = ((op >> 6) & 0x1F) as u32;
        let rb = ((op >> 3) & 7) as usize;
        let rd = (op & 7) as usize;

        let addr = self.r[rb].wrapping_add(if byte { offset } else { offset * 4 });

        if load {
            self.r[rd] = if byte {
                bus.read8(addr, self.r[PC]) as u32
            } else {
                let raw = bus.read32(addr, self.r[PC]);
                raw.rotate_right((addr & 3) * 8)
            };
            3
        } else {
            if byte {
                bus.write8(addr, self.r[rd] as u8);
            } else {
                bus.write32(addr, self.r[rd]);
            }
            2
        }
    }

    // ================================================================
    //  Format 10: Load/store halfword with immediate offset
    // ================================================================

    fn thumb_load_store_halfword(&mut self, op: u16, bus: &mut Bus) -> u32 {
        let load = op & (1 << 11) != 0;
        let offset = ((op >> 6) & 0x1F) as u32 * 2;
        let rb = ((op >> 3) & 7) as usize;
        let rd = (op & 7) as usize;
        let addr = self.r[rb].wrapping_add(offset);

        if load {
            self.r[rd] = bus.read16(addr, self.r[PC]) as u32;
            3
        } else {
            bus.write16(addr, self.r[rd] as u16);
            2
        }
    }

    // ================================================================
    //  Format 11: SP-relative load/store
    // ================================================================

    fn thumb_sp_relative(&mut self, op: u16, bus: &mut Bus) -> u32 {
        let load = op & (1 << 11) != 0;
        let rd = ((op >> 8) & 7) as usize;
        let offset = (op & 0xFF) as u32 * 4;
        let addr = self.r[SP].wrapping_add(offset);

        if load {
            self.r[rd] = bus.read32(addr, self.r[PC]);
            3
        } else {
            bus.write32(addr, self.r[rd]);
            2
        }
    }

    // ================================================================
    //  Format 12: Load address (ADD rd, PC/SP, #imm)
    // ================================================================

    fn thumb_load_address(&mut self, op: u16) -> u32 {
        let sp = op & (1 << 11) != 0;
        let rd = ((op >> 8) & 7) as usize;
        let imm = (op & 0xFF) as u32 * 4;

        self.r[rd] = if sp {
            self.r[SP].wrapping_add(imm)
        } else {
            // PC reads as instruction_addr + 4, word-aligned.
            (self.r[PC] & !3).wrapping_add(imm)
        };
        1
    }

    // ================================================================
    //  Format 13: Adjust stack pointer
    // ================================================================

    fn thumb_adjust_sp(&mut self, op: u16) -> u32 {
        let offset = (op & 0x7F) as u32 * 4;
        if op & 0x80 != 0 {
            self.r[SP] = self.r[SP].wrapping_sub(offset);
        } else {
            self.r[SP] = self.r[SP].wrapping_add(offset);
        }
        1
    }

    // ================================================================
    //  Format 14: Push/pop registers
    // ================================================================

    fn thumb_push_pop(&mut self, op: u16, bus: &mut Bus) -> u32 {
        let load = op & (1 << 11) != 0;
        let pclr = op & (1 << 8) != 0;
        let rlist = op & 0xFF;

        if load {
            // POP
            let mut addr = self.r[SP];
            for i in 0..8u16 {
                if rlist & (1 << i) != 0 {
                    self.r[i as usize] = bus.read32(addr, self.r[PC]);
                    addr = addr.wrapping_add(4);
                }
            }
            if pclr {
                self.r[PC] = bus.read32(addr, self.r[PC]) & !1;
                addr = addr.wrapping_add(4);
                self.flush_pipeline(bus);
            }
            self.r[SP] = addr;
            let count = rlist.count_ones() + pclr as u32;
            if pclr { count + 4 } else { count + 2 }
        } else {
            // PUSH
            let count = rlist.count_ones() + pclr as u32;
            let mut addr = self.r[SP].wrapping_sub(count * 4);
            self.r[SP] = addr;
            for i in 0..8u16 {
                if rlist & (1 << i) != 0 {
                    bus.write32(addr, self.r[i as usize]);
                    addr = addr.wrapping_add(4);
                }
            }
            if pclr {
                bus.write32(addr, self.r[LR]);
            }
            count + 1
        }
    }

    // ================================================================
    //  Format 15: Multiple load/store (LDMIA / STMIA)
    // ================================================================

    fn thumb_multiple(&mut self, op: u16, bus: &mut Bus) -> u32 {
        let load = op & (1 << 11) != 0;
        let rb = ((op >> 8) & 7) as usize;
        let rlist = op & 0xFF;
        let mut addr = self.r[rb];

        if load {
            for i in 0..8u16 {
                if rlist & (1 << i) != 0 {
                    self.r[i as usize] = bus.read32(addr, self.r[PC]);
                    addr = addr.wrapping_add(4);
                }
            }
            if rlist & (1 << rb as u16) == 0 {
                self.r[rb] = addr;
            }
        } else {
            for i in 0..8u16 {
                if rlist & (1 << i) != 0 {
                    bus.write32(addr, self.r[i as usize]);
                    addr = addr.wrapping_add(4);
                }
            }
            self.r[rb] = addr;
        }

        rlist.count_ones() + if load { 2 } else { 1 }
    }

    // ================================================================
    //  Format 16: Conditional branch
    // ================================================================

    fn thumb_cond_branch(&mut self, op: u16, bus: &mut Bus) -> u32 {
        let cond = ((op >> 8) & 0xF) as u32;
        if !self.check_cond(cond) { return 1; }

        let offset = ((op & 0xFF) as i8 as i32) * 2;
        self.r[PC] = (self.r[PC] as i32).wrapping_add(offset) as u32;
        self.flush_pipeline(bus);
        3
    }

    // ================================================================
    //  Format 17: SWI
    // ================================================================

    fn thumb_swi(&mut self, _op: u16, bus: &mut Bus) -> u32 {
        let ret = self.r[PC].wrapping_sub(2);
        let old_cpsr = self.cpsr_bits();
        self.switch_mode(Mode::Supervisor);
        self.spsr[Mode::Supervisor.bank()] = Cpsr::from_bits_retain(old_cpsr);
        self.r[LR] = ret;
        self.cpsr.remove(Cpsr::T);
        self.cpsr.insert(Cpsr::I);
        self.r[PC] = 0x08;
        self.flush_pipeline(bus);
        3
    }

    // ================================================================
    //  Format 18: Unconditional branch
    // ================================================================

    fn thumb_uncond_branch(&mut self, op: u16, bus: &mut Bus) -> u32 {
        let offset = ((op & 0x7FF) as u32) << 1;
        // Sign-extend from bit 11.
        let offset = if offset & (1 << 11) != 0 { offset | 0xFFFFF000 } else { offset };
        self.r[PC] = (self.r[PC] as i32).wrapping_add(offset as i32) as u32;
        self.flush_pipeline(bus);
        3
    }

    // ================================================================
    //  Format 19: Long branch with link (two-instruction sequence)
    // ================================================================

    fn thumb_long_branch(&mut self, op: u16, bus: &mut Bus) -> u32 {
        let hi = op & (1 << 11) != 0;

        if !hi {
            // First instruction: LR = PC + (offset << 12)
            let offset = (op & 0x7FF) as u32;
            let offset = if offset & 0x400 != 0 {
                offset | 0xFFFFF800  // sign-extend
            } else {
                offset
            };
            self.r[LR] = self.r[PC].wrapping_add(offset << 12);
            1
        } else {
            // Second instruction: temp = next_insn_addr; PC = LR + (offset << 1); LR = temp | 1
            let offset = ((op & 0x7FF) as u32) << 1;
            let next = self.r[PC].wrapping_sub(2);
            self.r[PC] = self.r[LR].wrapping_add(offset);
            self.r[LR] = next | 1;
            self.flush_pipeline(bus);
            3
        }
    }
}
