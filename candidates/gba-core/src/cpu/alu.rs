//! Barrel shifter + ALU primitives shared between ARM and Thumb.

use super::{Cpu, Cpsr};

#[inline]
pub fn lsl(v: u32, amt: u32, carry_in: bool) -> (u32, bool) {
    match amt {
        0 => (v, carry_in),
        1..=31 => (v << amt, (v >> (32 - amt)) & 1 != 0),
        32 => (0, v & 1 != 0),
        _ => (0, false),
    }
}

#[inline]
pub fn lsr(v: u32, amt: u32, carry_in: bool, imm_form: bool) -> (u32, bool) {
    // Immediate form: LSR #0 means LSR #32.
    let amt = if imm_form && amt == 0 { 32 } else { amt };
    match amt {
        0 => (v, carry_in),
        1..=31 => (v >> amt, (v >> (amt - 1)) & 1 != 0),
        32 => (0, v >> 31 != 0),
        _ => (0, false),
    }
}

#[inline]
pub fn asr(v: u32, amt: u32, carry_in: bool, imm_form: bool) -> (u32, bool) {
    let amt = if imm_form && amt == 0 { 32 } else { amt };
    match amt {
        0 => (v, carry_in),
        1..=31 => (((v as i32) >> amt) as u32, (v >> (amt - 1)) & 1 != 0),
        _ => {
            let sign = v >> 31 != 0;
            (if sign { 0xFFFF_FFFF } else { 0 }, sign)
        }
    }
}

#[inline]
pub fn ror(v: u32, amt: u32, carry_in: bool, imm_form: bool) -> (u32, bool) {
    if imm_form && amt == 0 {
        // RRX: rotate right one with carry-in.
        let out = (v >> 1) | ((carry_in as u32) << 31);
        return (out, v & 1 != 0);
    }
    if amt == 0 { return (v, carry_in); }
    let amt = amt & 31;
    if amt == 0 {
        // ROR by multiple of 32: value unchanged, carry = bit 31.
        (v, v >> 31 != 0)
    } else {
        let out = v.rotate_right(amt);
        (out, out >> 31 != 0)
    }
}

/// ADD with full flag computation. Returns (result, carry, overflow).
#[inline]
pub fn add_flags(a: u32, b: u32, carry_in: u32) -> (u32, bool, bool) {
    let r = (a as u64) + (b as u64) + (carry_in as u64);
    let res = r as u32;
    let carry = r > 0xFFFF_FFFF;
    // Overflow: operands same sign, result different sign.
    let overflow = ((a ^ res) & (b ^ res)) >> 31 != 0;
    (res, carry, overflow)
}

/// SUB with full flag computation. carry = NOT borrow (ARM convention).
#[inline]
pub fn sub_flags(a: u32, b: u32, carry_in: u32) -> (u32, bool, bool) {
    // a - b - !carry  ==  a + !b + carry
    add_flags(a, !b, carry_in)
}

impl Cpu {
    #[inline]
    pub fn alu_add_setflags(&mut self, a: u32, b: u32) -> u32 {
        let (r, c, v) = add_flags(a, b, 0);
        self.set_nz(r);
        self.cpsr.set(Cpsr::C, c);
        self.cpsr.set(Cpsr::V, v);
        r
    }

    #[inline]
    pub fn alu_sub_setflags(&mut self, a: u32, b: u32) -> u32 {
        let (r, c, v) = sub_flags(a, b, 1);
        self.set_nz(r);
        self.cpsr.set(Cpsr::C, c);
        self.cpsr.set(Cpsr::V, v);
        r
    }

    #[inline]
    pub fn alu_adc_setflags(&mut self, a: u32, b: u32) -> u32 {
        let (r, c, v) = add_flags(a, b, self.carry() as u32);
        self.set_nz(r);
        self.cpsr.set(Cpsr::C, c);
        self.cpsr.set(Cpsr::V, v);
        r
    }

    #[inline]
    pub fn alu_sbc_setflags(&mut self, a: u32, b: u32) -> u32 {
        let (r, c, v) = sub_flags(a, b, self.carry() as u32);
        self.set_nz(r);
        self.cpsr.set(Cpsr::C, c);
        self.cpsr.set(Cpsr::V, v);
        r
    }
}
