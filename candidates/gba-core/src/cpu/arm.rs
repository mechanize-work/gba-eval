//! ARM (32-bit) instruction decoder and executor.
//!
//! ARM7TDMI instruction classes (decode by bits 27-20 and 7-4):
//!   - Branch / Branch with Link
//!   - Branch and Exchange (BX)
//!   - Data Processing (AND, EOR, SUB, RSB, ADD, ADC, SBC, RSC, TST, TEQ, CMP, CMN, ORR, MOV, BIC, MVN)
//!   - Multiply / Multiply-Long
//!   - Single Data Transfer (LDR/STR)
//!   - Halfword/Signed Data Transfer (LDRH/STRH/LDRSB/LDRSH)
//!   - Block Data Transfer (LDM/STM)
//!   - Software Interrupt (SWI)
//!   - MRS / MSR (status register access)
//!   - Single Data Swap (SWP)

use super::{Cpu, Cpsr, Mode, PC, LR};
use super::alu;
use crate::bus::Bus;

impl Cpu {
    pub fn exec_arm(&mut self, op: u32, bus: &mut Bus) -> u32 {
        // Decode by bits 27:20 and 7:4.
        let bits27_20 = (op >> 20) & 0xFF;
        let bits7_4 = (op >> 4) & 0xF;

        // ---- Branch and Exchange (BX) ----
        if op & 0x0FFFFFF0 == 0x012FFF10 {
            return self.arm_bx(op, bus);
        }

        // ---- Software Interrupt ----
        if bits27_20 >> 4 == 0xF {
            return self.arm_swi(op, bus);
        }

        // ---- Branch / Branch with Link ----
        if bits27_20 >> 5 == 0x5 {
            return self.arm_branch(op, bus);
        }

        // ---- Block Data Transfer (LDM/STM) ----
        if bits27_20 >> 5 == 0x4 {
            return self.arm_block_transfer(op, bus);
        }

        // ---- Single Data Swap (SWP) ----
        if (bits27_20 & 0xFB) == 0x10 && bits7_4 == 0x9 {
            return self.arm_swap(op, bus);
        }

        // ---- Multiply / Multiply-Long ----
        if bits7_4 == 0x9 && (bits27_20 & 0xFC) == 0x00 {
            return self.arm_multiply(op);
        }
        if bits7_4 == 0x9 && (bits27_20 & 0xF8) == 0x08 {
            return self.arm_multiply_long(op);
        }

        // ---- Halfword / Signed transfer ----
        // bits 7:4 = 1xx1 and bit 25 = 0 (register offset) or various combos.
        if bits7_4 & 0x9 == 0x9 && bits7_4 & 0x6 != 0 && (bits27_20 & 0xE0) == 0x00 {
            return self.arm_halfword_transfer(op, bus);
        }

        // ---- MRS (read CPSR/SPSR) ----
        if (op & 0x0FBF_0FFF) == 0x010F_0000 {
            return self.arm_mrs(op);
        }

        // ---- MSR (write CPSR/SPSR) ----
        // Register form: 0001_0x10_xxxx_1111_0000_0000_xxxx
        // Immediate form: 0011_0x10_xxxx_1111_xxxx_xxxx_xxxx
        if (op & 0x0FB0_FFF0) == 0x0120_F000 || (op & 0x0FB0_F000) == 0x0320_F000 {
            return self.arm_msr(op);
        }

        // ---- Single Data Transfer (LDR/STR) ----
        // bits 27:26 = 01
        if (op >> 26) & 3 == 1 {
            return self.arm_single_transfer(op, bus);
        }

        // ---- Data Processing ----
        // bits 27:26 = 00
        if (op >> 26) & 3 == 0 {
            return self.arm_data_processing(op, bus);
        }

        // Undefined instruction — treat as NOP for now.
        1
    }

    // ================================================================
    //  Branch / Branch with Link
    // ================================================================

    fn arm_branch(&mut self, op: u32, bus: &mut Bus) -> u32 {
        let link = op & (1 << 24) != 0;
        // Sign-extend 24-bit offset, shift left 2.
        let offset = ((op & 0x00FF_FFFF) as i32) << 8 >> 6; // sign-extend then <<2
        if link {
            // LR = address of instruction after BL = insn_addr + 4.
            // r[PC] = insn_addr + 8, so LR = PC - 4.
            self.r[LR] = self.r[PC].wrapping_sub(4);
        }
        // Target = PC + offset (PC = insn_addr + 8 per ARM convention).
        self.r[PC] = (self.r[PC] as i32).wrapping_add(offset) as u32;
        self.flush_pipeline(bus);
        3 // 2S + 1N
    }

    // ================================================================
    //  Branch and Exchange (BX Rn)
    // ================================================================

    fn arm_bx(&mut self, op: u32, bus: &mut Bus) -> u32 {
        let rn = (op & 0xF) as usize;
        let addr = self.r[rn];
        if addr & 1 != 0 {
            self.cpsr.insert(Cpsr::T);
        } else {
            self.cpsr.remove(Cpsr::T);
        }
        self.r[PC] = addr & !1;
        self.flush_pipeline(bus);
        3
    }

    // ================================================================
    //  Data Processing
    // ================================================================

    fn arm_data_processing(&mut self, op: u32, bus: &mut Bus) -> u32 {
        let i_bit = op & (1 << 25) != 0; // immediate operand
        let s_bit = op & (1 << 20) != 0; // set flags
        let opcode = (op >> 21) & 0xF;
        let rn = ((op >> 16) & 0xF) as usize;
        let rd = ((op >> 12) & 0xF) as usize;

        // Compute operand2 and shift carry-out.
        let carry_in = self.carry();
        let (op2, shift_carry) = if i_bit {
            let imm = op & 0xFF;
            let rot = ((op >> 8) & 0xF) * 2;
            if rot == 0 {
                (imm, carry_in)
            } else {
                let v = imm.rotate_right(rot);
                (v, v >> 31 != 0)
            }
        } else {
            self.arm_shift_operand(op, carry_in, bus)
        };

        // When the shift is register-specified (not immediate), reading PC
        // as Rn returns PC+12 (the prefetch pipeline advances during the
        // extra internal cycle for the register shift).
        let reg_shift = !i_bit && (op >> 4) & 1 != 0;
        let a = if reg_shift && rn == 15 {
            self.r[rn].wrapping_add(4)
        } else {
            self.r[rn]
        };

        let (result, write_rd) = match opcode {
            0x0 => (a & op2, true),                                         // AND
            0x1 => (a ^ op2, true),                                         // EOR
            0x2 => {                                                        // SUB
                let r = if s_bit { self.alu_sub_setflags(a, op2) } else { a.wrapping_sub(op2) };
                (r, true)
            }
            0x3 => {                                                        // RSB
                let r = if s_bit { self.alu_sub_setflags(op2, a) } else { op2.wrapping_sub(a) };
                (r, true)
            }
            0x4 => {                                                        // ADD
                let r = if s_bit { self.alu_add_setflags(a, op2) } else { a.wrapping_add(op2) };
                (r, true)
            }
            0x5 => {                                                        // ADC
                let r = if s_bit { self.alu_adc_setflags(a, op2) } else {
                    a.wrapping_add(op2).wrapping_add(self.carry() as u32)
                };
                (r, true)
            }
            0x6 => {                                                        // SBC
                let r = if s_bit { self.alu_sbc_setflags(a, op2) } else {
                    a.wrapping_sub(op2).wrapping_sub(1 - self.carry() as u32)
                };
                (r, true)
            }
            0x7 => {                                                        // RSC
                let r = if s_bit { self.alu_sbc_setflags(op2, a) } else {
                    op2.wrapping_sub(a).wrapping_sub(1 - self.carry() as u32)
                };
                (r, true)
            }
            0x8 => (a & op2, false),                                        // TST
            0x9 => (a ^ op2, false),                                        // TEQ
            0xA => {                                                        // CMP (always sets flags)
                self.alu_sub_setflags(a, op2);
                let cycles = if !i_bit && (op >> 4) & 1 != 0 { 2 } else { 1 };
                return cycles;
            }
            0xB => {                                                        // CMN (always sets flags)
                self.alu_add_setflags(a, op2);
                let cycles = if !i_bit && (op >> 4) & 1 != 0 { 2 } else { 1 };
                return cycles;
            }
            0xC => (a | op2, true),                                         // ORR
            0xD => (op2, true),                                             // MOV
            0xE => (a & !op2, true),                                        // BIC
            0xF => (!op2, true),                                            // MVN
            _ => unreachable!(),
        };

        if s_bit {
            match opcode {
                // Logical ops: N, Z from result, C from shifter, V unchanged.
                0x0 | 0x1 | 0x8 | 0x9 | 0xC | 0xD | 0xE | 0xF => {
                    self.set_nz(result);
                    self.cpsr.set(Cpsr::C, shift_carry);
                }
                // Arithmetic ops: flags already set by alu_ helpers above.
                _ => {}
            }
        }

        if write_rd {
            self.r[rd] = result;
            if rd == PC {
                if s_bit {
                    // MOVS/SUBS to PC restores CPSR from SPSR (exception return).
                    // Set IRQ delay to prevent immediate re-dispatch.
                    self.irq_delay = 1;
                    let spsr = self.spsr_bits();
                    let new_mode = Mode::from_bits(spsr);
                    self.switch_mode(new_mode);
                    self.cpsr = Cpsr::from_bits_retain(spsr & !0x1F);
                }
                self.flush_pipeline(bus);
                return 3;
            }
        }

        // Register-specified shift = +1I cycle.
        if !i_bit && (op >> 4) & 1 != 0 { 2 } else { 1 }
    }

    /// Compute shifted register operand for data processing.
    fn arm_shift_operand(&mut self, op: u32, carry_in: bool, _bus: &Bus) -> (u32, bool) {
        let rm = (op & 0xF) as usize;
        let shift_type = (op >> 5) & 3;
        let by_reg = (op >> 4) & 1 != 0;

        let val = if by_reg && rm == 15 {
            // Register-shifted: Rm=PC reads PC+12 (pipeline artifact —
            // the extra internal cycle for reading Rs advances the prefetch).
            self.r[rm].wrapping_add(4)
        } else {
            self.r[rm]
        };
        let amount = if by_reg {
            let rs = ((op >> 8) & 0xF) as usize;
            self.r[rs] & 0xFF
        } else {
            (op >> 7) & 0x1F
        };

        match shift_type {
            0 => alu::lsl(val, amount, carry_in),
            1 => alu::lsr(val, amount, carry_in, !by_reg),
            2 => alu::asr(val, amount, carry_in, !by_reg),
            3 => alu::ror(val, amount, carry_in, !by_reg),
            _ => unreachable!(),
        }
    }

    // ================================================================
    //  Single Data Transfer (LDR / STR)
    // ================================================================

    fn arm_single_transfer(&mut self, op: u32, bus: &mut Bus) -> u32 {
        let i_bit = op & (1 << 25) != 0;  // offset is register (confusingly, opposite of DP)
        let p_bit = op & (1 << 24) != 0;  // pre-indexed
        let u_bit = op & (1 << 23) != 0;  // add offset
        let b_bit = op & (1 << 22) != 0;  // byte transfer
        let w_bit = op & (1 << 21) != 0;  // writeback
        let l_bit = op & (1 << 20) != 0;  // load

        let rn = ((op >> 16) & 0xF) as usize;
        let rd = ((op >> 12) & 0xF) as usize;

        let offset = if i_bit {
            // Shifted register offset.
            let rm = (op & 0xF) as usize;
            let shift_type = (op >> 5) & 3;
            let amount = (op >> 7) & 0x1F;
            let carry = self.carry();
            let (shifted, _) = match shift_type {
                0 => alu::lsl(self.r[rm], amount, carry),
                1 => alu::lsr(self.r[rm], amount, carry, true),
                2 => alu::asr(self.r[rm], amount, carry, true),
                3 => alu::ror(self.r[rm], amount, carry, true),
                _ => unreachable!(),
            };
            shifted
        } else {
            op & 0xFFF
        };

        let base = self.r[rn];
        let addr = if u_bit { base.wrapping_add(offset) } else { base.wrapping_sub(offset) };
        let effective = if p_bit { addr } else { base };

        if l_bit {
            let val = if b_bit {
                bus.read8(effective, self.r[PC]) as u32
            } else {
                let raw = bus.read32(effective, self.r[PC]);
                // Misaligned LDR rotates.
                let rot = (effective & 3) * 8;
                raw.rotate_right(rot)
            };
            self.r[rd] = val;
            if rd == PC {
                self.flush_pipeline(bus);
            }
        } else {
            let val = if rd == PC { self.r[PC].wrapping_add(4) } else { self.r[rd] };
            if b_bit {
                bus.write8(effective, val as u8);
            } else {
                bus.write32(effective, val);
            }
        }

        // Writeback.
        if !p_bit || w_bit {
            if rn != rd || !l_bit {
                self.r[rn] = addr;
            }
        }

        if l_bit { if rd == PC { 5 } else { 3 } } else { 2 }
    }

    // ================================================================
    //  Halfword / Signed Data Transfer (LDRH/STRH/LDRSB/LDRSH)
    // ================================================================

    fn arm_halfword_transfer(&mut self, op: u32, bus: &mut Bus) -> u32 {
        let p_bit = op & (1 << 24) != 0;
        let u_bit = op & (1 << 23) != 0;
        let imm_offset = op & (1 << 22) != 0;
        let w_bit = op & (1 << 21) != 0;
        let l_bit = op & (1 << 20) != 0;

        let rn = ((op >> 16) & 0xF) as usize;
        let rd = ((op >> 12) & 0xF) as usize;
        let sh = (op >> 5) & 3;

        let offset = if imm_offset {
            ((op >> 4) & 0xF0) | (op & 0xF)
        } else {
            self.r[(op & 0xF) as usize]
        };

        let base = self.r[rn];
        let addr = if u_bit { base.wrapping_add(offset) } else { base.wrapping_sub(offset) };
        let effective = if p_bit { addr } else { base };

        if l_bit {
            let val = match sh {
                1 => bus.read16(effective, self.r[PC]) as u32,                     // LDRH
                2 => bus.read8(effective, self.r[PC]) as i8 as i32 as u32,         // LDRSB
                3 => bus.read16(effective, self.r[PC]) as i16 as i32 as u32,       // LDRSH
                _ => 0,
            };
            self.r[rd] = val;
            if rd == PC { self.flush_pipeline(bus); }
        } else {
            // STRH.
            bus.write16(effective, self.r[rd] as u16);
        }

        if !p_bit || w_bit {
            if rn != rd || !l_bit {
                self.r[rn] = addr;
            }
        }

        if l_bit { 3 } else { 2 }
    }

    // ================================================================
    //  Block Data Transfer (LDM / STM)
    // ================================================================

    fn arm_block_transfer(&mut self, op: u32, bus: &mut Bus) -> u32 {
        let p_bit = op & (1 << 24) != 0;  // pre/post
        let u_bit = op & (1 << 23) != 0;  // up/down
        let s_bit = op & (1 << 22) != 0;  // PSR / force user banks
        let w_bit = op & (1 << 21) != 0;  // writeback
        let l_bit = op & (1 << 20) != 0;  // load

        let rn = ((op >> 16) & 0xF) as usize;
        let rlist = op & 0xFFFF;

        let count = rlist.count_ones();
        if count == 0 {
            // Edge case: empty rlist on ARM7TDMI loads/stores PC and increments by 0x40.
            return 1;
        }

        let addr = self.r[rn];
        let start = if u_bit {
            if p_bit { addr.wrapping_add(4) } else { addr }
        } else {
            let block = count.wrapping_mul(4);
            if p_bit { addr.wrapping_sub(block) } else { addr.wrapping_sub(block).wrapping_add(4) }
        };
        let final_addr = if u_bit {
            addr.wrapping_add(count.wrapping_mul(4))
        } else {
            addr.wrapping_sub(count.wrapping_mul(4))
        };

        let mut cur = start;

        // If S bit and load with PC: restore CPSR from SPSR.
        // If S bit and no PC in rlist: use user-mode registers.
        let user_bank = s_bit && (!l_bit || rlist & (1 << 15) == 0);
        let restore_cpsr = s_bit && l_bit && rlist & (1 << 15) != 0;

        // For user_bank mode, temporarily switch to User/System to access User registers.
        let saved_mode = self.mode;
        if user_bank && self.mode != Mode::User && self.mode != Mode::System {
            self.switch_mode(Mode::User);
        }

        for i in 0..16u32 {
            if rlist & (1 << i) == 0 { continue; }
            let reg = i as usize;
            if l_bit {
                self.r[reg] = bus.read32(cur, self.r[PC]);
            } else {
                let val = if reg == PC { self.r[PC].wrapping_add(4) } else { self.r[reg] };
                bus.write32(cur, val);
            }
            cur = cur.wrapping_add(4);
        }

        // Restore original mode if we switched for user_bank access.
        if user_bank && saved_mode != Mode::User && saved_mode != Mode::System {
            self.switch_mode(saved_mode);
        }

        if w_bit {
            // LDM: if Rn is in the register list, the loaded value takes priority
            // over writeback (writeback is suppressed).
            if l_bit && rlist & (1 << rn as u32) != 0 {
                // Don't writeback — loaded value already in r[rn].
            } else {
                self.r[rn] = final_addr;
            }
        }

        if l_bit && rlist & (1 << 15) != 0 {
            if restore_cpsr {
                let spsr = self.spsr_bits();
                let new_mode = Mode::from_bits(spsr);
                self.switch_mode(new_mode);
                self.cpsr = Cpsr::from_bits_retain(spsr & !0x1F);
            }
            self.flush_pipeline(bus);
            return count + 4; // rough timing
        }

        count + if l_bit { 2 } else { 1 }
    }

    // ================================================================
    //  Multiply
    // ================================================================

    fn arm_multiply(&mut self, op: u32) -> u32 {
        let accumulate = op & (1 << 21) != 0;
        let set_flags = op & (1 << 20) != 0;
        let rd = ((op >> 16) & 0xF) as usize;
        let rn = ((op >> 12) & 0xF) as usize;
        let rs = ((op >> 8) & 0xF) as usize;
        let rm = (op & 0xF) as usize;

        let result = self.r[rm].wrapping_mul(self.r[rs]);
        let result = if accumulate { result.wrapping_add(self.r[rn]) } else { result };
        self.r[rd] = result;

        if set_flags {
            self.set_nz(result);
            // C is "meaningless" on ARM7TDMI multiply.
        }

        // ARM7TDMI multiply timing: m cycles based on leading-bits of Rs.
        //   m=1: Rs[31:8] all 0 or all 1
        //   m=2: Rs[31:16] all 0 or all 1
        //   m=3: Rs[31:24] all 0 or all 1
        //   m=4: otherwise
        // MUL = m cycles; MLA = m+1 cycles. Both include the S fetch.
        let rs_val = self.r[rs];
        let masked = rs_val & 0xFFFF_FF00;
        let m = if masked == 0 || masked == 0xFFFF_FF00 { 1 }
            else if (rs_val & 0xFFFF_0000) == 0 || (rs_val & 0xFFFF_0000) == 0xFFFF_0000 { 2 }
            else if (rs_val & 0xFF00_0000) == 0 || (rs_val & 0xFF00_0000) == 0xFF00_0000 { 3 }
            else { 4 };
        if accumulate { m + 1 } else { m }
    }

    fn arm_multiply_long(&mut self, op: u32) -> u32 {
        let signed = op & (1 << 22) != 0;
        let accumulate = op & (1 << 21) != 0;
        let set_flags = op & (1 << 20) != 0;
        let rdhi = ((op >> 16) & 0xF) as usize;
        let rdlo = ((op >> 12) & 0xF) as usize;
        let rs = ((op >> 8) & 0xF) as usize;
        let rm = (op & 0xF) as usize;

        let result: u64 = if signed {
            (self.r[rm] as i32 as i64).wrapping_mul(self.r[rs] as i32 as i64) as u64
        } else {
            (self.r[rm] as u64).wrapping_mul(self.r[rs] as u64)
        };

        let result = if accumulate {
            let acc = ((self.r[rdhi] as u64) << 32) | self.r[rdlo] as u64;
            result.wrapping_add(acc)
        } else {
            result
        };

        self.r[rdhi] = (result >> 32) as u32;
        self.r[rdlo] = result as u32;

        if set_flags {
            self.cpsr.set(Cpsr::N, result >> 63 != 0);
            self.cpsr.set(Cpsr::Z, result == 0);
        }

        if accumulate { 4 } else { 3 }
    }

    // ================================================================
    //  MRS / MSR
    // ================================================================

    fn arm_mrs(&mut self, op: u32) -> u32 {
        let rd = ((op >> 12) & 0xF) as usize;
        let use_spsr = op & (1 << 22) != 0;
        self.r[rd] = if use_spsr { self.spsr_bits() } else { self.cpsr_bits() };
        1
    }

    fn arm_msr(&mut self, op: u32) -> u32 {
        let use_spsr = op & (1 << 22) != 0;
        let i_bit = op & (1 << 25) != 0;

        let val = if i_bit {
            let imm = op & 0xFF;
            let rot = ((op >> 8) & 0xF) * 2;
            imm.rotate_right(rot)
        } else {
            self.r[(op & 0xF) as usize]
        };

        // Field mask: bits 19:16 determine which byte fields to write.
        let mut mask = 0u32;
        if op & (1 << 19) != 0 { mask |= 0xFF00_0000; } // flags
        if op & (1 << 16) != 0 { mask |= 0x0000_00FF; } // control (only in privileged mode)

        if use_spsr {
            self.set_spsr_bits(val, mask);
        } else {
            self.set_cpsr_bits(val, mask);
        }

        1
    }

    // ================================================================
    //  Single Data Swap (SWP)
    // ================================================================

    fn arm_swap(&mut self, op: u32, bus: &mut Bus) -> u32 {
        let byte = op & (1 << 22) != 0;
        let rn = ((op >> 16) & 0xF) as usize;
        let rd = ((op >> 12) & 0xF) as usize;
        let rm = (op & 0xF) as usize;

        let addr = self.r[rn];
        if byte {
            let old = bus.read8(addr, self.r[PC]) as u32;
            bus.write8(addr, self.r[rm] as u8);
            self.r[rd] = old;
        } else {
            let old = bus.read32(addr, self.r[PC]);
            bus.write32(addr, self.r[rm]);
            // Misaligned SWP rotates the read like LDR.
            let rot = (addr & 3) * 8;
            self.r[rd] = old.rotate_right(rot);
        }

        4
    }

    // ================================================================
    //  Software Interrupt (SWI)
    // ================================================================

    fn arm_swi(&mut self, _op: u32, bus: &mut Bus) -> u32 {
        let ret = self.r[PC].wrapping_sub(4);
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
}
