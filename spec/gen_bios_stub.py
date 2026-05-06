#!/usr/bin/env python3
"""
Generate a 16 KiB GBA BIOS stub with working SWI implementations.

Most accurate GBA emulators have no HLE — they jump to whatever bytes
sit at the SWI vector. A minimal stub (just `MOVS PC, LR`) makes every
SWI a silent no-op: CpuFastSet copies nothing, IntrWait doesn't wait,
Div returns garbage. Games that lean on these never properly initialize.

This stub implements the SWIs that real games actually call, in plain
ARM. No host-side cooperation needed — Halt is a write to HALTCNT,
IntrWait is a halt-poll loop on BIOS_IF, Div is restoring shift, CpuSet
is LDMIA/STMIA. The same image works for any cycle-accurate GBA
emulator that lacks HLE.

Output: 16384-byte gba_bios_stub.bin

Layout:
  0x000  reset vector (B 0x08000000 — skip_bios overrides anyway)
  0x008  SWI vector → dispatch
  0x018  IRQ vector → handler at 0x128
  0x128  IRQ handler (the standard 6-instruction BIOS dispatch)
  0x140  SWI dispatch + handlers
"""

import struct
import sys

class Asm:
    """Tiny ARM assembler — only the encodings we need."""

    def __init__(self):
        self.code = bytearray(0x4000)
        self.pc = 0
        self.labels = {}
        self.fixups = []  # (pc, label, kind)

    def at(self, addr):
        self.pc = addr

    def label(self, name):
        self.labels[name] = self.pc

    def emit(self, word):
        struct.pack_into("<I", self.code, self.pc, word)
        self.pc += 4

    # Condition codes
    AL, EQ, NE, CS, CC, MI, PL, HI, LS, GE, LT, GT, LE = \
        0xE, 0x0, 0x1, 0x2, 0x3, 0x4, 0x5, 0x8, 0x9, 0xA, 0xB, 0xC, 0xD

    def b(self, target, cond=AL):
        if isinstance(target, str):
            self.fixups.append((self.pc, target, "b", cond))
            self.emit(0)
        else:
            off = (target - self.pc - 8) >> 2
            self.emit((cond << 28) | 0x0A000000 | (off & 0xFFFFFF))

    def bl(self, target, cond=AL):
        self.fixups.append((self.pc, target, "bl", cond))
        self.emit(0)

    def mov_imm(self, rd, imm, cond=AL):
        # imm must fit in 8 bits with even rotation
        for rot in range(16):
            unrot = ((imm << (rot * 2)) | (imm >> (32 - rot * 2))) & 0xFFFFFFFF
            if unrot < 256:
                self.emit((cond << 28) | 0x03A00000 | (rd << 12) | (rot << 8) | unrot)
                return
        raise ValueError(f"mov #{imm:#x} doesn't fit in rotated imm8")

    def mov_reg(self, rd, rm, cond=AL):
        self.emit((cond << 28) | 0x01A00000 | (rd << 12) | rm)

    def movs_reg(self, rd, rm, cond=AL):
        self.emit((cond << 28) | 0x01B00000 | (rd << 12) | rm)

    def mvn_imm(self, rd, imm, cond=AL):
        self.emit((cond << 28) | 0x03E00000 | (rd << 12) | imm)

    def add_imm(self, rd, rn, imm, cond=AL):
        for rot in range(16):
            unrot = ((imm << (rot * 2)) | (imm >> (32 - rot * 2))) & 0xFFFFFFFF
            if unrot < 256:
                self.emit((cond << 28) | 0x02800000 | (rn << 16) | (rd << 12) | (rot << 8) | unrot)
                return
        raise ValueError(f"add #{imm:#x} doesn't fit")

    def add_reg(self, rd, rn, rm, cond=AL):
        self.emit((cond << 28) | 0x00800000 | (rn << 16) | (rd << 12) | rm)

    def sub_imm(self, rd, rn, imm, cond=AL):
        self.emit((cond << 28) | 0x02400000 | (rn << 16) | (rd << 12) | imm)

    def sub_reg(self, rd, rn, rm, cond=AL):
        self.emit((cond << 28) | 0x00400000 | (rn << 16) | (rd << 12) | rm)

    def subs_imm(self, rd, rn, imm, cond=AL):
        self.emit((cond << 28) | 0x02500000 | (rn << 16) | (rd << 12) | imm)

    def subs_reg(self, rd, rn, rm, cond=AL):
        self.emit((cond << 28) | 0x00500000 | (rn << 16) | (rd << 12) | rm)

    def rsb_imm(self, rd, rn, imm, cond=AL):
        self.emit((cond << 28) | 0x02600000 | (rn << 16) | (rd << 12) | imm)

    def rsbs_imm(self, rd, rn, imm, cond=AL):
        self.emit((cond << 28) | 0x02700000 | (rn << 16) | (rd << 12) | imm)

    def cmp_imm(self, rn, imm, cond=AL):
        self.emit((cond << 28) | 0x03500000 | (rn << 16) | imm)

    def cmp_reg(self, rn, rm, cond=AL):
        self.emit((cond << 28) | 0x01500000 | (rn << 16) | rm)

    def tst_imm(self, rn, imm, cond=AL):
        for rot in range(16):
            unrot = ((imm << (rot * 2)) | (imm >> (32 - rot * 2))) & 0xFFFFFFFF
            if unrot < 256:
                self.emit((cond << 28) | 0x03100000 | (rn << 16) | (rot << 8) | unrot)
                return
        raise ValueError(f"tst #{imm:#x} doesn't fit")

    def tst_reg(self, rn, rm, cond=AL):
        self.emit((cond << 28) | 0x01100000 | (rn << 16) | rm)

    def and_imm(self, rd, rn, imm, cond=AL):
        for rot in range(16):
            unrot = ((imm << (rot * 2)) | (imm >> (32 - rot * 2))) & 0xFFFFFFFF
            if unrot < 256:
                self.emit((cond << 28) | 0x02000000 | (rn << 16) | (rd << 12) | (rot << 8) | unrot)
                return
        raise ValueError(f"and #{imm:#x} doesn't fit")

    def and_reg(self, rd, rn, rm, cond=AL):
        self.emit((cond << 28) | 0x00000000 | (rn << 16) | (rd << 12) | rm)

    def ands_reg(self, rd, rn, rm, cond=AL):
        self.emit((cond << 28) | 0x00100000 | (rn << 16) | (rd << 12) | rm)

    def orr_imm(self, rd, rn, imm, cond=AL):
        for rot in range(16):
            unrot = ((imm << (rot * 2)) | (imm >> (32 - rot * 2))) & 0xFFFFFFFF
            if unrot < 256:
                self.emit((cond << 28) | 0x03800000 | (rn << 16) | (rd << 12) | (rot << 8) | unrot)
                return
        raise ValueError(f"orr #{imm:#x} doesn't fit")

    def orr_reg(self, rd, rn, rm, cond=AL):
        self.emit((cond << 28) | 0x01800000 | (rn << 16) | (rd << 12) | rm)

    def eor_reg(self, rd, rn, rm, cond=AL):
        self.emit((cond << 28) | 0x00200000 | (rn << 16) | (rd << 12) | rm)

    def bic_reg(self, rd, rn, rm, cond=AL):
        self.emit((cond << 28) | 0x01C00000 | (rn << 16) | (rd << 12) | rm)

    def bic_imm(self, rd, rn, imm, cond=AL):
        for rot in range(16):
            unrot = ((imm << (rot * 2)) | (imm >> (32 - rot * 2))) & 0xFFFFFFFF
            if unrot < 256:
                self.emit((cond << 28) | 0x03C00000 | (rn << 16) | (rd << 12) | (rot << 8) | unrot)
                return
        raise ValueError(f"bic #{imm:#x} doesn't fit in rotated imm8")

    def asr_imm(self, rd, rm, sh, cond=AL):
        # MOV rd, rm, ASR #sh (arithmetic shift right)
        assert 1 <= sh <= 32
        sh5 = sh & 31  # ASR #32 is encoded as sh=0
        self.emit((cond << 28) | 0x01A00040 | (rd << 12) | (sh5 << 7) | rm)

    def mul(self, rd, rm, rs, cond=AL):
        # MUL rd, rm, rs — rd = rm * rs (low 32 bits)
        assert rd != rm, "MUL: Rd must differ from Rm on ARMv4"
        self.emit((cond << 28) | 0x00000090 | (rd << 16) | (rs << 8) | rm)

    def smull(self, rd_lo, rd_hi, rm, rs, cond=AL):
        # SMULL RdLo, RdHi, Rm, Rs — signed 64-bit multiply
        self.emit((cond << 28) | 0x00C00090 | (rd_hi << 16) | (rd_lo << 12) | (rs << 8) | rm)

    def ldrsh_imm(self, rd, rn, off, cond=AL):
        # LDRSH rd, [rn, #off] — signed halfword load
        u = 1 if off >= 0 else 0
        a = abs(off)
        assert a <= 0xFF, f"LDRSH offset {off:#x} exceeds 8-bit imm"
        self.emit((cond << 28) | 0x015000F0 | (u << 23) | (rn << 16) | (rd << 12)
                  | ((a & 0xF0) << 4) | (a & 0xF))

    def lsl_imm(self, rd, rm, sh, cond=AL):
        self.emit((cond << 28) | 0x01A00000 | (rd << 12) | (sh << 7) | rm)

    def lsr_imm(self, rd, rm, sh, cond=AL):
        self.emit((cond << 28) | 0x01A00020 | (rd << 12) | ((sh & 31) << 7) | rm)

    def adc_reg(self, rd, rn, rm, cond=AL):
        self.emit((cond << 28) | 0x00A00000 | (rn << 16) | (rd << 12) | rm)

    def adcs_reg(self, rd, rn, rm, cond=AL):
        self.emit((cond << 28) | 0x00B00000 | (rn << 16) | (rd << 12) | rm)

    def lsls_imm(self, rd, rm, sh, cond=AL):
        # MOVS rd, rm, LSL #sh — sets carry from shifted-out bit
        self.emit((cond << 28) | 0x01B00000 | (rd << 12) | (sh << 7) | rm)

    def lsrs_imm(self, rd, rm, sh, cond=AL):
        # MOVS rd, rm, LSR #sh — sets flags
        assert 1 <= sh <= 32
        sh5 = sh & 31  # LSR #32 is encoded as sh=0
        self.emit((cond << 28) | 0x01B00020 | (rd << 12) | (sh5 << 7) | rm)

    def lsl_reg(self, rd, rm, rs, cond=AL):
        # MOV rd, rm, LSL rs — shift amount in low byte of rs
        self.emit((cond << 28) | 0x01A00010 | (rd << 12) | (rs << 8) | rm)

    def lsr_reg(self, rd, rm, rs, cond=AL):
        self.emit((cond << 28) | 0x01A00030 | (rd << 12) | (rs << 8) | rm)

    def orr_reg_lsl(self, rd, rn, rm, rs, cond=AL):
        # ORR rd, rn, rm, LSL rs
        self.emit((cond << 28) | 0x01800010 | (rn << 16) | (rd << 12) | (rs << 8) | rm)

    def ldr_post(self, rd, rn, off, cond=AL):
        # LDR rd, [rn], #off — post-indexed (load from [rn], then rn += off)
        u = 1 if off >= 0 else 0
        self.emit((cond << 28) | 0x04100000 | (u << 23) | (rn << 16) | (rd << 12) | abs(off))

    def str_post(self, rd, rn, off, cond=AL):
        u = 1 if off >= 0 else 0
        self.emit((cond << 28) | 0x04000000 | (u << 23) | (rn << 16) | (rd << 12) | abs(off))

    def ldrb_post(self, rd, rn, off, cond=AL):
        u = 1 if off >= 0 else 0
        self.emit((cond << 28) | 0x04500000 | (u << 23) | (rn << 16) | (rd << 12) | abs(off))

    def strb_post(self, rd, rn, off, cond=AL):
        u = 1 if off >= 0 else 0
        self.emit((cond << 28) | 0x04400000 | (u << 23) | (rn << 16) | (rd << 12) | abs(off))

    def strh_post(self, rd, rn, off, cond=AL):
        u = 1 if off >= 0 else 0
        a = abs(off)
        assert a <= 0xFF, f"STRH post offset {off:#x} exceeds 8-bit imm"
        self.emit((cond << 28) | 0x004000B0 | (u << 23) | (rn << 16) | (rd << 12)
                  | ((a & 0xF0) << 4) | (a & 0xF))

    def ldr_imm(self, rd, rn, off, cond=AL):
        u = 1 if off >= 0 else 0
        self.emit((cond << 28) | 0x05100000 | (u << 23) | (rn << 16) | (rd << 12) | abs(off))

    def str_imm(self, rd, rn, off, cond=AL):
        u = 1 if off >= 0 else 0
        self.emit((cond << 28) | 0x05000000 | (u << 23) | (rn << 16) | (rd << 12) | abs(off))

    def ldrh_imm(self, rd, rn, off, cond=AL):
        u = 1 if off >= 0 else 0
        a = abs(off)
        assert a <= 0xFF, f"LDRH offset {off:#x} exceeds 8-bit imm (silently truncates to {a&0xFF:#x})"
        self.emit((cond << 28) | 0x015000B0 | (u << 23) | (rn << 16) | (rd << 12)
                  | ((a & 0xF0) << 4) | (a & 0xF))

    def strh_imm(self, rd, rn, off, cond=AL):
        u = 1 if off >= 0 else 0
        a = abs(off)
        assert a <= 0xFF, f"STRH offset {off:#x} exceeds 8-bit imm"
        self.emit((cond << 28) | 0x014000B0 | (u << 23) | (rn << 16) | (rd << 12)
                  | ((a & 0xF0) << 4) | (a & 0xF))

    def strb_imm(self, rd, rn, off, cond=AL):
        u = 1 if off >= 0 else 0
        self.emit((cond << 28) | 0x05400000 | (u << 23) | (rn << 16) | (rd << 12) | abs(off))

    def ldrb_imm(self, rd, rn, off, cond=AL):
        u = 1 if off >= 0 else 0
        self.emit((cond << 28) | 0x05500000 | (u << 23) | (rn << 16) | (rd << 12) | abs(off))

    def ldmia(self, rn, reglist, writeback=True, cond=AL):
        w = 1 if writeback else 0
        self.emit((cond << 28) | 0x08900000 | (w << 21) | (rn << 16) | reglist)

    def stmia(self, rn, reglist, writeback=True, cond=AL):
        w = 1 if writeback else 0
        self.emit((cond << 28) | 0x08800000 | (w << 21) | (rn << 16) | reglist)

    def stmfd(self, rn, reglist, cond=AL):  # = STMDB rn!
        self.emit((cond << 28) | 0x09200000 | (rn << 16) | reglist)

    def ldmfd(self, rn, reglist, cond=AL):  # = LDMIA rn!
        self.emit((cond << 28) | 0x08B00000 | (rn << 16) | reglist)

    def mrs_spsr(self, rd, cond=AL):
        self.emit((cond << 28) | 0x014F0000 | (rd << 12))

    def msr_cpsr_c(self, rm, cond=AL):
        # MSR CPSR_c, rm — write control field (mode bits + I/F/T)
        self.emit((cond << 28) | 0x0121F000 | rm)

    def msr_spsr_cf(self, rm, cond=AL):
        self.emit((cond << 28) | 0x0169F000 | rm)

    def fixup(self):
        for pc, label, kind, cond in self.fixups:
            target = self.labels[label]
            off = (target - pc - 8) >> 2
            link = 0x01000000 if kind == "bl" else 0
            struct.pack_into("<I", self.code, pc,
                             (cond << 28) | 0x0A000000 | link | (off & 0xFFFFFF))


def build():
    a = Asm()
    R = lambda *rs: sum(1 << r for r in rs)  # noqa: E731  reglist helper
    SP, LR, PC = 13, 14, 15

    # ─── Exception vectors ──────────────────────────────────────────────────
    a.at(0x00); a.b("reset")        # Reset
    a.at(0x04); a.b("hang")         # Undefined
    a.at(0x08); a.b("swi_entry")    # SWI
    a.at(0x0C); a.b("hang")         # Prefetch abort
    a.at(0x10); a.b("hang")         # Data abort
    a.at(0x18); a.b("irq_entry")    # IRQ
    a.at(0x1C); a.b("hang")         # FIQ

    # ─── IRQ handler ────────────────────────────────────────────────────────
    # The game's handler pointer lives at 0x03007FFC. Real BIOS also ORs
    # IF into BIOS_IF (0x03007FF8) BEFORE calling the handler — IntrWait
    # depends on this. Many games' handlers do it themselves, but not all.
    # Without this our IntrWait spins forever waiting for a bit that never
    # appears.
    #
    # LDRH offset is 8-bit so we can't reach 0x202 (IF) directly from the
    # I/O base. ADD a temp.
    a.at(0x128)
    a.label("irq_entry")
    a.stmfd(SP, R(0,1,2,3,12,LR))   # save scratch + LR_irq
    a.mov_imm(0, 0x04000000)        # r0 = I/O base (game handler expects this)
    a.add_imm(12, 0, 0x200)         # r12 = 0x04000200 (IE/IF region)
    a.ldrh_imm(1, 12, 2)            # r1 = IF
    a.ldrh_imm(2, 0, -8)            # r2 = [0x03FFFFF8] = BIOS_IF
    a.orr_reg(2, 2, 1)              # BIOS_IF |= IF
    a.strh_imm(2, 0, -8)
    a.add_imm(LR, PC, 0)            # LR = pc+8 = return point below
    a.ldr_imm(PC, 0, -4)            # PC = [0x03FFFFFC] = handler
    a.ldmfd(SP, R(0,1,2,3,12,LR))   # restore
    a.subs_imm(PC, LR, 4)           # SUBS PC, LR_irq, #4 — restores CPSR

    # ─── SWI dispatch ───────────────────────────────────────────────────────
    # Matches the real GBA BIOS structure:
    #   1. Save {r11, r12, lr} on SVC stack
    #   2. Read SWI comment byte
    #   3. Save SPSR on SVC stack
    #   4. Switch to SYS mode (preserving caller's I bit)
    #   5. Dispatch through jump table
    #
    # The real BIOS switches to SYS mode before calling the handler, and
    # switches back on return. This affects cycle counts — tests like
    # haltcnt measure total SWI overhead. Matching the real BIOS's
    # instruction count is necessary for those tests to pass.
    a.at(0x160)
    a.label("swi_entry")
    a.stmfd(SP, R(11,12,LR))        # SVC stack: save scratch + LR_svc

    # Read comment byte. LDRB [LR, #-2] works for Thumb. For ARM,
    # bits 23:16 of the SWI instruction are at byte offset +2 from
    # the instruction address in little-endian, so [LR-4+2] = [LR-2].
    # The real BIOS does exactly this — no Thumb check needed.
    a.ldrb_imm(12, LR, -2)          # r12 = comment byte

    # Save SPSR, switch to SYS mode (preserving I bit)
    a.mrs_spsr(11)                   # r11 = SPSR_svc
    a.stmfd(SP, R(11))               # push SPSR on SVC stack
    a.and_imm(11, 11, 0x80)          # preserve caller's I bit
    a.orr_imm(11, 11, 0x1F)          # SYS mode (0x1F) + caller's I
    a.msr_cpsr_c(11)                 # now in SYS mode

    # Dispatch: ADD PC, PC, r12, LSL #2
    a.stmfd(SP, R(2, LR))            # SYS stack: save r2 + LR_sys
    a.cmp_imm(12, 0x20)
    a.b("swi_unknown", cond=Asm.CS)
    a.emit(0xE08FF10C)               # ADD PC, PC, r12, LSL #2
    a.emit(0xE1A00000)               # NOP (pipeline gap)

    # Jump table: 0x20 entries, each is a B to the handler.
    handlers = {
        0x00: "swi_soft_reset",
        0x01: "swi_register_ram_reset",
        0x02: "swi_halt",
        0x03: "swi_stop",
        0x04: "swi_intr_wait",
        0x05: "swi_vblank_intr_wait",
        0x06: "swi_div",
        0x07: "swi_div_arm",
        0x08: "swi_sqrt",
        0x09: "swi_arctan",
        0x0A: "swi_arctan2",
        0x0B: "swi_cpu_set",
        0x0C: "swi_cpu_fast_set",
        0x0D: "swi_bios_checksum",
        0x0E: "swi_bg_affine_set",
        0x0F: "swi_obj_affine_set",
        0x10: "swi_bit_unpack",
        0x11: "swi_lz77_uncomp_wram",
        0x12: "swi_lz77_uncomp_vram",
        0x13: "swi_huff_uncomp",
        0x14: "swi_rl_uncomp_wram",
        0x15: "swi_rl_uncomp_vram",
        0x16: "swi_diff8_unfilt_wram",
        0x17: "swi_diff8_unfilt_vram",
        0x18: "swi_diff16_unfilt",
        0x19: "swi_sound_bias",
        0x1F: "swi_midi_key2freq",
    }
    for i in range(0x20):
        a.b(handlers.get(i, "swi_unknown"))

    # ─── SWI return ──────────────────────────────────────────────────────
    # Handlers branch here when done. Switch back to SVC, restore
    # SPSR, pop, and MOVS PC, LR.
    a.label("swi_unknown")
    a.label("swi_return")
    a.ldmfd(SP, R(2, LR))            # restore r2 + LR_sys from SYS stack
    a.mov_imm(11, 0x93)              # SVC mode, I=1
    a.msr_cpsr_c(11)                 # back to SVC mode
    a.ldmfd(SP, R(11))               # pop SPSR
    a.msr_spsr_cf(11)                # restore SPSR_svc
    a.ldmfd(SP, R(11,12,LR))         # restore scratch + LR_svc
    a.movs_reg(PC, LR)               # return to caller (restores CPSR)

    # ─── SWI 0x02: Halt ─────────────────────────────────────────────────────
    # Write 0 to HALTCNT (0x04000301). Hardware halts the CPU until any
    # enabled interrupt fires (regardless of CPSR.I).
    a.label("swi_halt")
    a.mov_imm(12, 0x04000000)
    a.mov_imm(11, 0)
    a.strb_imm(11, 12, 0x301)       # [0x04000301] = 0
    a.b("swi_return")

    # ─── SWI 0x05: VBlankIntrWait — IntrWait(1, 1) ──────────────────────────
    a.label("swi_vblank_intr_wait")
    a.mov_imm(0, 1)
    a.mov_imm(1, 1)
    # falls through

    # ─── SWI 0x04: IntrWait(r0=discard, r1=flags) ───────────────────────────
    # The game's IRQ handler is responsible for ORing IF into BIOS_IF
    # at 0x03007FF8 before clearing IF. We just poll BIOS_IF in a halt loop.
    # We're already in SYS mode (SWI entry switched us). IRQs may or may
    # not be enabled depending on caller's CPSR.I — force them on.
    a.label("swi_intr_wait")
    a.stmfd(SP, R(0,1,2,3,4))
    a.mov_imm(2, 0x04000000)        # r2 = I/O base
    # BIOS_IF lives at 0x03007FF8. LDRH/STRH offsets are 8-bit so we can't
    # reach it from any nicely-loadable base. Point r3 right at it: I/O
    # base minus 8 lands at 0x03FFFFF8, which mirrors to 0x03007FF8.
    a.sub_imm(3, 2, 8)              # r3 = 0x03FFFFF8 = BIOS_IF (via IWRAM mirror)
    a.mov_imm(4, 1)
    a.strb_imm(4, 2, 0x208)         # IME = 1

    # if discard: BIOS_IF &= ~flags
    a.cmp_imm(0, 0)
    a.b("iw_loop", cond=Asm.EQ)
    a.ldrh_imm(4, 3, 0)
    a.bic_reg(4, 4, 1)
    a.strh_imm(4, 3, 0)

    # Loop: halt; on wake, check if (BIOS_IF & flags) != 0.
    #
    # Two halt-delay traps in here, both Mesen-specific.
    #
    # (1) Mesen delays the actual stop — STRB to HALTCNT sets _haltDelay=1,
    # the CPU executes ONE MORE cycle, THEN stops. If we MSR back to SVC
    # (I=1) right after STRB, the halt engages with IRQs masked: we wake on
    # IE&IF (IsHaltOver ignores CPSR.I) but the handler never runs, BIOS_IF
    # never updates, spin. Fix: stay in SYS (I=0) until the final pop.
    #
    # (2) The Stopped flag is checked at the TOP of Exec(), before any
    # cycles tick. _haltDelay decrements DURING instruction execution (in
    # ProcessPendingUpdates, called per-cycle). So instruction N's check
    # passes (Stopped=false), then its cycles tick the delay down to zero
    # and SetStopFlag fires — but N has already begun and runs to finish.
    # N+1 should see it. With _haltDelay=1 that's one instruction of slack.
    # Bisect on heartwrench (VBlank-only in IE → a missed wake costs a full
    # frame, no HBlank/timer to bail it out) showed TWO: 0/1 NOP → frame 257
    # (= 2× ours/HLE 129), 2 NOPs → 135. Pokemon's timer IRQs masked it:
    # the second halt woke in microseconds, no observable tempo hit.
    #
    # If LDRH is one of those two, r4 captures BIOS_IF before any IRQ fires.
    # Wake → ANDS tests stale zero → branch back → halt → next VBlank. 2×.
    a.label("iw_loop")
    a.mov_imm(4, 0x1F)              # SYS mode, I=0, F=0, T=0
    a.msr_cpsr_c(4)                 # enable IRQs (and stay enabled)
    a.label("iw_halt")
    a.mov_imm(4, 0)
    a.strb_imm(4, 2, 0x301)         # halt (delayed: 2 instr-starts of slack)
    a.emit(0xE1A00000)              # NOP — these two slip past the check;
    a.emit(0xE1A00000)              # NOP — LDRH is the first post-wake instr
    # — halt engages here; IRQ fires; handler runs (I=0); returns —
    a.ldrh_imm(4, 3, 0)             # r4 = BIOS_IF (fresh: post-IRQ)
    a.ands_reg(4, 4, 1)             # r4 &= flags; sets Z if zero
    a.b("iw_halt", cond=Asm.EQ)     # not yet — halt again (still in SYS, I=0)
    # Got it. Clear matched bits and return.
    a.ldrh_imm(0, 3, 0)
    a.bic_reg(0, 0, 4)
    a.strh_imm(0, 3, 0)
    a.ldmfd(SP, R(0,1,2,3,4))
    a.b("swi_return")

    # ─── SWI 0x06: Div(r0 / r1) → r0=quot, r1=rem, r3=|quot| ────────────────
    # Restoring division. ~96 cycles regardless of operand size — matches
    # the real BIOS within a few cycles. Sign handling: remember signs,
    # divide unsigned magnitudes, fix up at the end.
    #   sign(quot) = sign(num) XOR sign(den)
    #   sign(rem)  = sign(num)
    a.label("swi_div")
    a.stmfd(SP, R(4,5))
    a.eor_reg(5, 0, 1)              # r5 bit 31 = sign of quotient
    a.mov_reg(4, 0)                 # r4 bit 31 = sign of remainder (= sign of num)
    a.cmp_imm(0, 0)
    a.rsb_imm(0, 0, 0, cond=Asm.MI) # |num|
    a.cmp_imm(1, 0)
    a.rsb_imm(1, 1, 0, cond=Asm.MI) # |den|
    # Now r0=|num|, r1=|den|. Divide.
    # r3 = quot accumulator, r2 = den<<shift. Find highest shift first.
    a.mov_imm(3, 0)
    a.mov_reg(2, 1)
    a.cmp_imm(2, 0)
    a.b("div_done", cond=Asm.EQ)    # /0: leave r0 unchanged, r3=0
    # Shift den left until ≥ num or bit 31 set.
    a.label("div_align")
    a.cmp_reg(2, 0)
    a.b("div_loop", cond=Asm.CS)    # den >= num? stop shifting
    a.lsls_imm(2, 2, 1)
    a.b("div_align", cond=Asm.CC)   # bit 31 not yet set, keep going
    # Restoring loop.
    a.label("div_loop")
    a.cmp_reg(0, 2)
    a.sub_reg(0, 0, 2, cond=Asm.CS) # if num >= den<<k: subtract
    a.adcs_reg(3, 3, 3)             # quot = (quot << 1) | carry
    a.lsr_imm(2, 2, 1)
    a.cmp_reg(2, 1)
    a.b("div_loop", cond=Asm.CS)    # while shifted_den >= original_den
    a.label("div_done")
    # r0 = remainder, r3 = quotient. Apply signs.
    a.cmp_imm(4, 0)
    a.rsb_imm(0, 0, 0, cond=Asm.MI) # rem takes num's sign
    a.mov_reg(1, 0)                 # r1 = remainder (final)
    a.mov_reg(0, 3)                 # r0 = quotient (unsigned)
    a.cmp_imm(5, 0)
    a.rsb_imm(0, 0, 0, cond=Asm.MI) # quot takes XOR sign
    # r3 = |quot| (BIOS contract)
    a.mov_reg(3, 0)
    a.cmp_imm(3, 0)
    a.rsb_imm(3, 3, 0, cond=Asm.MI)
    a.ldmfd(SP, R(4,5))
    a.b("swi_return")

    # ─── SWI 0x07: DivArm — swap r0/r1 then Div ─────────────────────────────
    a.label("swi_div_arm")
    a.mov_reg(12, 0)
    a.mov_reg(0, 1)
    a.mov_reg(1, 12)
    a.b("swi_div")

    # ─── SWI 0x08: Sqrt(r0) → r0 ────────────────────────────────────────────
    # Bit-by-bit integer square root. 16 iterations, ~80 cycles.
    a.label("swi_sqrt")
    a.stmfd(SP, R(1,2,3))
    a.mov_imm(1, 0)                 # r1 = result
    a.mov_imm(2, 0)                 # r2 = remainder accumulator
    a.mov_imm(3, 16)                # 16 iterations (32-bit input)
    a.label("sqrt_loop")
    # Shift in two bits of input
    a.lsl_imm(2, 2, 2)              # rem <<= 2
    a.lsls_imm(0, 0, 1)             # carry = top bit of input
    a.adc_reg(2, 2, 2)              # ... wait this doubles. Need different approach.
    # Actually: rem = (rem << 2) | (input >> 30). Do it via two single-bit shifts.
    a.pc -= 8                       # back out the broken pair
    a.lsls_imm(0, 0, 1)
    a.adcs_reg(2, 2, 2)             # rem = (rem << 1) | C
    a.lsls_imm(0, 0, 1)
    a.adcs_reg(2, 2, 2)
    # trial = (result << 2) | 1
    a.lsl_imm(12, 1, 2)
    a.add_imm(12, 12, 1)
    a.lsl_imm(1, 1, 1)              # result <<= 1
    a.cmp_reg(2, 12)
    a.sub_reg(2, 2, 12, cond=Asm.CS)
    a.add_imm(1, 1, 1, cond=Asm.CS)
    a.subs_imm(3, 3, 1)
    a.b("sqrt_loop", cond=Asm.NE)
    a.mov_reg(0, 1)
    a.ldmfd(SP, R(1,2,3))
    a.b("swi_return")

    # ─── SWI 0x0B: CpuSet(r0=src, r1=dst, r2=cnt|flags) ─────────────────────
    # cnt[20:0] = unit count, cnt[24] = fill, cnt[26] = 0:halfword 1:word
    #
    # Earlier version tried to share the loop body between fill and copy
    # via a conditional reload (TST sets Z, LDR cond=EQ inside the loop).
    # That doesn't work: the SUBS at the bottom clobbers Z, so iteration 2
    # tests "is count-1 zero?" instead of "is fill bit clear?". Net effect:
    # copy mode wrote element[0], then filled the rest with element[1].
    # Pokemon calls this 291 times during init to copy m4a track tables;
    # every melodic track was filled with the second pointer. Sound effects
    # (single-element CpuSets and CpuFastSet) survived, music didn't.
    # Matches real BIOS structure: push {r4-r10,lr}, compute end address
    # in r10, use r3 as transfer register, compare r1 against r10 to
    # terminate the loop. The real BIOS also calls a BIOS-region check
    # subroutine (bl sub_BA4) that we skip since we have no BIOS read
    # protection.
    a.label("swi_cpu_set")
    a.stmfd(SP, R(4,5,6,7,8,9,10,LR))
    a.lsl_imm(10, 2, 11)            # r10 = count << 11 (isolate bits 20:0)
    # Check word vs halfword to compute byte length
    a.tst_imm(2, 0x04000000)        # bit 26: word mode?
    a.b("cs_word", cond=Asm.NE)

    # ── Halfword mode ──
    # r10 has count<<11. LSR #11 gives count, then *2 for byte length.
    # Equivalently: LSR #10 gives count*2 directly.
    a.lsrs_imm(12, 10, 10)          # ip = count * 2 (byte length)
    a.b("cs_done", cond=Asm.EQ)
    a.add_reg(10, 1, 12)            # r10 = dst + byte_length (end addr)
    a.tst_imm(2, 0x01000000)        # fill?
    a.b("cs_h_fill", cond=Asm.NE)
    a.label("cs_h_copy")
    a.ldrh_imm(3, 0, 0)
    a.add_imm(0, 0, 2)
    a.strh_imm(3, 1, 0)
    a.add_imm(1, 1, 2)
    a.cmp_reg(1, 10)
    a.b("cs_h_copy", cond=Asm.CC)   # while dst < end
    a.b("cs_done")
    a.label("cs_h_fill")
    a.ldrh_imm(3, 0, 0)
    a.label("cs_h_fill_loop")
    a.strh_imm(3, 1, 0)
    a.add_imm(1, 1, 2)
    a.cmp_reg(1, 10)
    a.b("cs_h_fill_loop", cond=Asm.CC)
    a.b("cs_done")

    # ── Word mode ──
    # r10 has count<<11. LSR #9 gives count*4 (byte length for words).
    a.label("cs_word")
    a.lsrs_imm(12, 10, 9)           # ip = count * 4 (byte length)
    a.b("cs_done", cond=Asm.EQ)
    a.add_reg(10, 1, 12)            # r10 = end address
    a.tst_imm(2, 0x01000000)        # fill?
    a.b("cs_w_fill", cond=Asm.NE)
    # Real BIOS loop: CMP r1, r10; LDMLTIA r0!, {r3}; STMLTIA r1!, {r3}; BLT loop
    a.label("cs_w_copy")
    a.cmp_reg(1, 10)
    a.ldmia(0, R(3), cond=Asm.LT)
    a.stmia(1, R(3), cond=Asm.LT)
    a.b("cs_w_copy", cond=Asm.LT)
    a.b("cs_done")
    a.label("cs_w_fill")
    a.ldr_imm(3, 0, 0)
    a.label("cs_w_fill_loop")
    a.cmp_reg(1, 10)
    a.stmia(1, R(3), cond=Asm.LT)
    a.b("cs_w_fill_loop", cond=Asm.LT)

    a.label("cs_done")
    a.ldmfd(SP, R(4,5,6,7,8,9,10,LR))
    a.b("swi_return")

    # ─── SWI 0x0C: CpuFastSet — 8-word blocks via LDMIA/STMIA ───────────────
    # Always 32-bit. Count is rounded UP to multiple of 8 (real BIOS does
    # this; some games rely on it). Fill mode replicates [src] into 8 regs
    # then STMIAs.
    a.label("swi_cpu_fast_set")
    a.stmfd(SP, R(0,1,2,3,4,5,6,7,8,9,10))
    a.lsl_imm(10, 2, 11)
    a.lsr_imm(10, 10, 11)           # count
    a.add_imm(10, 10, 7)
    a.bic_imm(10, 10, 7)            # round up to ×8
    a.cmp_imm(10, 0)
    a.b("cfs_done", cond=Asm.EQ)
    a.tst_imm(2, 0x01000000)
    a.b("cfs_fill", cond=Asm.NE)
    # Copy: 8 words at a time
    a.label("cfs_copy")
    a.ldmia(0, R(2,3,4,5,6,7,8,9))
    a.stmia(1, R(2,3,4,5,6,7,8,9))
    a.subs_imm(10, 10, 8)
    a.b("cfs_copy", cond=Asm.NE)
    a.b("cfs_done")
    # Fill: read once, replicate, blast
    a.label("cfs_fill")
    a.ldr_imm(2, 0, 0)
    a.mov_reg(3, 2); a.mov_reg(4, 2); a.mov_reg(5, 2)
    a.mov_reg(6, 2); a.mov_reg(7, 2); a.mov_reg(8, 2); a.mov_reg(9, 2)
    a.label("cfs_fill_loop")
    a.stmia(1, R(2,3,4,5,6,7,8,9))
    a.subs_imm(10, 10, 8)
    a.b("cfs_fill_loop", cond=Asm.NE)
    a.label("cfs_done")
    a.ldmfd(SP, R(0,1,2,3,4,5,6,7,8,9,10))
    a.b("swi_return")

    # ─── SWI 0x0D: GetBiosChecksum — fixed value some games check ──────────
    a.label("swi_bios_checksum")
    a.ldr_imm(0, PC, 0)             # PC-relative load of literal
    a.b("swi_return")
    a.emit(0xBAAE187F)

    # ─── SWI 0x01: RegisterRamReset(r0=flags) ───────────────────────────────
    # Each bit clears a region. Pokemon calls this with 0xFF at boot.
    #   bit 0: EWRAM   (0x02000000, 256 KiB)
    #   bit 1: IWRAM   (0x03000000, 32 KiB — but spare last 0x200 for stacks)
    #   bit 2: Palette (0x05000000, 1 KiB)
    #   bit 3: VRAM    (0x06000000, 96 KiB)
    #   bit 4: OAM     (0x07000000, 1 KiB)
    #   bit 5: SIO regs   — skip, write-side-effecty
    #   bit 6: Sound regs — skip, write-side-effecty
    #   bit 7: All other I/O — skip, this kills DISPCNT etc.
    # We only do bits 0-4. Bits 5-7 touch I/O which has side effects we
    # can't safely replicate without knowing exactly which regs are write-1-
    # to-clear vs write-0-to-clear. Real BIOS does them; games that NEED the
    # I/O reset will misbehave, but most just want the RAM cleared.
    a.label("swi_register_ram_reset")
    a.stmfd(SP, R(0,1,2,3,4,5,6,7,8,9))
    a.mov_reg(9, 0)                 # r9 = flags (r0 gets clobbered by stmia regs)
    # Build a zero block in r0-r7 once. Eight words = 32 bytes per stmia.
    a.mov_imm(0, 0); a.mov_imm(1, 0); a.mov_imm(2, 0); a.mov_imm(3, 0)
    a.mov_imm(4, 0); a.mov_imm(5, 0); a.mov_imm(6, 0); a.mov_imm(7, 0)

    # Helper: clear [r8 .. r8+r12). r12 in 32-byte units.
    def emit_clear(flagbit, base, size32):
        a.tst_imm(9, 1 << flagbit)
        a.b(f"rrr_skip{flagbit}", cond=Asm.EQ)
        a.mov_imm(8, base)
        a.mov_imm(12, size32)
        a.label(f"rrr_loop{flagbit}")
        a.stmia(8, R(0,1,2,3,4,5,6,7))
        a.subs_imm(12, 12, 1)
        a.b(f"rrr_loop{flagbit}", cond=Asm.NE)
        a.label(f"rrr_skip{flagbit}")

    emit_clear(0, 0x02000000, 256*1024 // 32)   # EWRAM: 256 KiB
    # IWRAM: stop at 0x03007E00. Real BIOS spares 0x7E00..0x8000 (stacks +
    # BIOS_IF + the IRQ handler pointer the game already wrote). Clearing
    # those would brick everything.
    emit_clear(1, 0x03000000, 0x7E00 // 32)
    emit_clear(2, 0x05000000, 1024 // 32)       # Palette: 1 KiB
    emit_clear(3, 0x06000000, 96*1024 // 32)    # VRAM: 96 KiB
    emit_clear(4, 0x07000000, 1024 // 32)       # OAM: 1 KiB

    a.ldmfd(SP, R(0,1,2,3,4,5,6,7,8,9))
    a.b("swi_return")

    # ─── SWI 0x10: BitUnPack(r0=src, r1=dst, r2=info_ptr) ───────────────────
    # info: { u16 srcLen; u8 srcWidth; u8 dstWidth; u32 dataOffset }
    #   srcWidth ∈ {1,2,4,8}, dstWidth ∈ {1,2,4,8,16,32}, dstWidth ≥ srcWidth
    #   dataOffset bit 31: if set, add offset even when chunk == 0
    #
    # For each srcLen source bytes:
    #   for each srcWidth-bit chunk in the byte (low bits first):
    #     if chunk != 0 OR offset.bit31: chunk += (offset & 0x7FFFFFFF)
    #     pack chunk into a 32-bit accumulator at the next dstWidth-bit slot
    #     when accumulator full (32 bits worth): write to dst, advance
    #
    # Typical usage: srcWidth=1, dstWidth=4 to expand 1bpp font bitmaps
    # into 4bpp tile data (chunk → palette index).
    #
    # Register plan:
    #   r0 = src (advances by 1 each outer iteration)
    #   r1 = dst (advances by 4 each flush)
    #   r2 = srcLen (counts down)
    #   r3 = srcWidth   r4 = dstWidth
    #   r5 = dataOffset & 0x7FFFFFFF
    #   r6 = "always add" flag (bit 31 of dataOffset, isolated)
    #   r7 = chunk mask = (1 << srcWidth) - 1
    #   r8 = accumulator
    #   r9 = bits-in-accumulator (when ≥ 32: flush)
    #   r10 = current source byte (refilled each outer iter)
    #   r11 = bits-left-in-source-byte (8 → 0)
    #   r12 = scratch
    a.label("swi_bit_unpack")
    a.stmfd(SP, R(0,1,2,3,4,5,6,7,8,9,10))

    a.ldrh_imm(3, 2, 0)             # r3 = srcLen (temp; will move to r2)
    a.ldrb_imm(4, 2, 3)             # r4 = dstWidth
    a.ldr_imm(5, 2, 4)              # r5 = dataOffset (full 32 bits)
    a.ldrb_imm(7, 2, 2)             # r7 = srcWidth (load AFTER r3, before move)
    a.mov_reg(2, 3)                 # r2 = srcLen
    a.mov_reg(3, 7)                 # r3 = srcWidth
    # r6 = bit 31 of offset; r5 = low 31 bits
    a.and_imm(6, 5, 0x80000000)
    a.bic_imm(5, 5, 0x80000000)
    # r7 = (1 << srcWidth) - 1. srcWidth ≤ 8 so this fits in a byte.
    a.mov_imm(7, 1)
    a.lsl_reg(7, 7, 3)
    a.sub_imm(7, 7, 1)

    a.mov_imm(8, 0)                 # accumulator = 0
    a.mov_imm(9, 0)                 # bits in accumulator = 0

    a.cmp_imm(2, 0)
    a.b("bup_done", cond=Asm.EQ)

    # ── Outer: load one source byte ───────────────────────────────────────
    a.label("bup_outer")
    a.ldrb_post(10, 0, 1)           # r10 = *src++, post-increment
    a.mov_imm(11, 8)                # 8 bits in this byte

    # ── Inner: extract one srcWidth-bit chunk ─────────────────────────────
    a.label("bup_inner")
    a.and_reg(12, 10, 7)            # chunk = byte & mask
    a.lsr_reg(10, 10, 3)            # byte >>= srcWidth
    # Add offset if chunk != 0 OR always-add flag set.
    a.cmp_imm(12, 0)
    a.add_reg(12, 12, 5, cond=Asm.NE)
    a.cmp_imm(6, 0)
    a.add_reg(12, 12, 5, cond=Asm.NE)
    # ^ if BOTH conditions hold we add twice — wrong. Need: if (chunk!=0 || flag).
    # Redo with proper short-circuit:
    a.pc -= 16
    a.cmp_imm(12, 0)
    a.b("bup_add", cond=Asm.NE)     # chunk != 0 → add
    a.cmp_imm(6, 0)
    a.b("bup_pack", cond=Asm.EQ)    # flag clear AND chunk==0 → skip
    a.label("bup_add")
    a.add_reg(12, 12, 5)
    a.label("bup_pack")
    # Pack: accumulator |= (chunk << bits_in_acc)
    a.orr_reg_lsl(8, 8, 12, 9)      # acc |= chunk << r9
    a.add_reg(9, 9, 4)              # bits_in_acc += dstWidth
    # Flush if full.
    a.cmp_imm(9, 32)
    a.b("bup_no_flush", cond=Asm.CC)
    a.str_post(8, 1, 4)             # *dst++ = acc
    a.mov_imm(8, 0)
    a.mov_imm(9, 0)
    a.label("bup_no_flush")
    # Inner loop: more bits in this byte?
    a.subs_reg(11, 11, 3)           # bits_left -= srcWidth
    a.b("bup_inner", cond=Asm.NE)
    # Outer loop: more source bytes?
    a.subs_imm(2, 2, 1)
    a.b("bup_outer", cond=Asm.NE)

    a.label("bup_done")
    # Real BIOS does NOT flush a partial accumulator at the end — games are
    # expected to size srcLen so the output is a whole number of words. If
    # we flushed here we'd write past the buffer for unaligned cases.
    a.ldmfd(SP, R(0,1,2,3,4,5,6,7,8,9,10))
    a.b("swi_return")

    # ─── SWI 0x11/0x12: LZ77UnCompWram / LZ77UnCompVram ────────────────────
    # GBA LZ77 format (type 0x10 in header nibble):
    #   [src+0]: u32 header — bits 7:4 = 0x1 (type), bits 31:8 = decompressed length
    #   [src+4..]: flag bytes + literal/reference data
    #
    # For each flag byte, 8 blocks MSB-first:
    #   bit=0: literal byte — copy 1 byte from src to dst
    #   bit=1: back-reference — next 2 bytes encode (length-3, offset-1)
    #           byte0 bits 7:4 = length-3, byte0 bits 3:0 || byte1 = offset-1
    #           copy (length) bytes from (dst - offset - 1)
    #
    # WRAM mode (SWI 0x11): writes bytes directly (STRB).
    # VRAM mode (SWI 0x12): buffers pairs and writes halfwords (STRH).
    #   VRAM on GBA ignores byte writes, so the real BIOS uses 16-bit writes.
    #   Back-references still operate at byte granularity on the output buffer —
    #   we keep a "pending low byte" and flush each pair as a halfword.
    #
    # Register plan:
    #   r0 = src ptr (advances through compressed data)
    #   r1 = dst ptr (advances through decompressed output)
    #   r2 = remaining decompressed bytes
    #   r3 = flag byte (current)
    #   r4 = bit counter (7..0 within flag byte)
    #   r5 = scratch / literal byte / back-ref byte0
    #   r6 = scratch / back-ref byte1 / copy length
    #   r7 = back-ref source address
    #   r8 = VRAM mode flag (0 = WRAM, 1 = VRAM)
    #   r9 = VRAM pending byte (low byte waiting for pair)
    #   r10 = VRAM has-pending flag (0 or 1)

    a.label("swi_lz77_uncomp_wram")
    a.stmfd(SP, R(0,1,2,3,4,5,6,7,8,9,10))
    a.mov_imm(8, 0)                     # r8 = 0 → WRAM mode
    a.b("lz77_common")

    a.label("swi_lz77_uncomp_vram")
    a.stmfd(SP, R(0,1,2,3,4,5,6,7,8,9,10))
    a.mov_imm(8, 1)                     # r8 = 1 → VRAM mode

    a.label("lz77_common")
    a.ldr_post(2, 0, 4)                 # r2 = header word, src += 4
    a.lsr_imm(2, 2, 8)                  # r2 = decompressed length (header >> 8)
    a.mov_imm(10, 0)                    # no pending byte yet
    a.cmp_imm(2, 0)
    a.b("lz77_done", cond=Asm.EQ)

    # ── Outer: read flag byte ─────────────────────────────────────────────
    a.label("lz77_flag")
    a.cmp_imm(2, 0)
    a.b("lz77_done", cond=Asm.EQ)
    a.ldrb_post(3, 0, 1)                # r3 = flag byte, src++
    a.mov_imm(4, 8)                     # 8 blocks per flag byte

    # ── Per-block ─────────────────────────────────────────────────────────
    a.label("lz77_block")
    a.cmp_imm(2, 0)
    a.b("lz77_done", cond=Asm.EQ)
    a.cmp_imm(4, 0)
    a.b("lz77_flag", cond=Asm.EQ)       # consumed all 8 blocks → next flag byte
    a.sub_imm(4, 4, 1)
    # Test top bit of flag byte (MSB first), then shift left
    a.tst_imm(3, 0x80)
    a.lsl_imm(3, 3, 1)                  # shift flag byte for next iteration
    a.b("lz77_ref", cond=Asm.NE)        # bit set → back-reference

    # ── Literal: copy one byte ────────────────────────────────────────────
    a.ldrb_post(5, 0, 1)                # r5 = literal byte, src++
    a.cmp_imm(8, 0)
    a.b("lz77_lit_vram", cond=Asm.NE)
    # WRAM: direct byte write
    a.strb_post(5, 1, 1)                # *dst++ = byte
    a.sub_imm(2, 2, 1)
    a.b("lz77_block")
    # VRAM: buffer byte pair, write halfword
    a.label("lz77_lit_vram")
    a.cmp_imm(10, 0)
    a.b("lz77_lit_v_flush", cond=Asm.NE)
    # No pending byte — store as low byte
    a.mov_reg(9, 5)                      # r9 = pending low byte
    a.mov_imm(10, 1)                     # has pending
    a.sub_imm(2, 2, 1)
    a.b("lz77_block")
    # Have pending — combine and write halfword
    a.label("lz77_lit_v_flush")
    a.lsl_imm(5, 5, 8)
    a.orr_reg(5, 9, 5)                  # r5 = pending | (new << 8)
    a.strh_imm(5, 1, 0)
    a.add_imm(1, 1, 2)                  # dst += 2
    a.mov_imm(10, 0)                     # no pending
    a.sub_imm(2, 2, 1)
    a.b("lz77_block")

    # ── Back-reference ────────────────────────────────────────────────────
    a.label("lz77_ref")
    a.ldrb_post(5, 0, 1)                # r5 = byte0 (length/offset hi)
    a.ldrb_post(6, 0, 1)                # r6 = byte1 (offset lo)
    # offset = ((byte0 & 0xF) << 8) | byte1
    a.and_imm(7, 5, 0x0F)               # r7 = byte0 & 0xF
    a.lsl_imm(7, 7, 8)
    a.orr_reg(7, 7, 6)                  # r7 = offset - 1
    a.add_imm(7, 7, 1)                  # r7 = offset (distance back from dst)
    # length = (byte0 >> 4) + 3
    a.lsr_imm(6, 5, 4)                  # r6 = byte0 >> 4
    a.add_imm(6, 6, 3)                  # r6 = copy length

    # Copy loop: read from (dst - offset), write to dst
    a.label("lz77_copy")
    a.cmp_imm(6, 0)
    a.b("lz77_block", cond=Asm.EQ)
    a.cmp_imm(2, 0)
    a.b("lz77_done", cond=Asm.EQ)
    a.cmp_imm(8, 0)
    a.b("lz77_copy_vram", cond=Asm.NE)
    # WRAM: back-ref source = dst - offset; read + write bytes directly
    a.sub_reg(5, 1, 7)                  # r5 = dst - offset
    a.ldrb_imm(5, 5, 0)                 # r5 = byte from back-ref
    a.strb_post(5, 1, 1)                # *dst++ = byte
    a.sub_imm(2, 2, 1)
    a.sub_imm(6, 6, 1)
    a.b("lz77_copy")
    # VRAM: logical output cursor = r1 + r10 (r10 is 0 or 1 pending).
    # Back-ref source = (r1 + r10) - r7. If that address == r1 and r10==1,
    # the byte is in r9 (pending), not in memory. Otherwise read from memory.
    a.label("lz77_copy_vram")
    a.add_reg(5, 1, 10)                 # r5 = logical cursor
    a.sub_reg(5, 5, 7)                  # r5 = source address
    a.cmp_reg(5, 1)                     # source == dst ptr (pending position)?
    a.b("lz77_copy_v_notpend", cond=Asm.NE)
    a.cmp_imm(10, 1)                    # and we actually have a pending byte?
    a.mov_reg(5, 9, cond=Asm.EQ)        # yes: r5 = pending byte
    a.b("lz77_copy_v_got", cond=Asm.EQ)
    a.label("lz77_copy_v_notpend")
    a.ldrb_imm(5, 5, 0)                 # r5 = byte from memory
    a.label("lz77_copy_v_got")
    # Buffer for halfword write
    a.cmp_imm(10, 0)
    a.b("lz77_copy_v_flush", cond=Asm.NE)
    a.mov_reg(9, 5)
    a.mov_imm(10, 1)
    a.sub_imm(2, 2, 1)
    a.sub_imm(6, 6, 1)
    a.b("lz77_copy")
    a.label("lz77_copy_v_flush")
    a.lsl_imm(5, 5, 8)
    a.orr_reg(5, 9, 5)                  # halfword = pending | (new << 8)
    a.strh_imm(5, 1, 0)
    a.add_imm(1, 1, 2)
    a.mov_imm(10, 0)
    a.sub_imm(2, 2, 1)
    a.sub_imm(6, 6, 1)
    a.b("lz77_copy")

    # ── Done ──────────────────────────────────────────────────────────────
    a.label("lz77_done")
    # Flush any remaining pending byte in VRAM mode
    a.cmp_imm(8, 0)
    a.b("lz77_exit", cond=Asm.EQ)
    a.cmp_imm(10, 0)
    a.b("lz77_exit", cond=Asm.EQ)
    a.strh_imm(9, 1, 0)                 # write final pending byte as halfword
    a.label("lz77_exit")
    a.ldmfd(SP, R(0,1,2,3,4,5,6,7,8,9,10))
    a.b("swi_return")

    # ─── SWI 0x14/0x15: RLUnCompWram / RLUnCompVram ─────────────────────
    # GBA Run-Length encoding (type 0x30 in header nibble):
    #   [src+0]: u32 header — bits 7:4 = 0x3 (type), bits 31:8 = decompressed length
    #   [src+4..]: flag/data bytes
    #
    # For each flag byte:
    #   bit 7 set:   compressed run — next byte repeated (flag & 0x7F) + 3 times
    #   bit 7 clear: uncompressed — copy (flag & 0x7F) + 1 literal bytes
    #
    # Same WRAM/VRAM write distinction as LZ77.
    #
    # Register plan:
    #   r0 = src ptr
    #   r1 = dst ptr
    #   r2 = remaining decompressed bytes
    #   r3 = flag byte
    #   r4 = run/copy length
    #   r5 = data byte / scratch
    #   r6 = VRAM mode flag (0 = WRAM, 1 = VRAM)
    #   r7 = VRAM pending byte
    #   r8 = VRAM has-pending flag

    a.label("swi_rl_uncomp_wram")
    a.stmfd(SP, R(0,1,2,3,4,5,6,7,8))
    a.mov_imm(6, 0)                      # WRAM mode
    a.b("rl_common")

    a.label("swi_rl_uncomp_vram")
    a.stmfd(SP, R(0,1,2,3,4,5,6,7,8))
    a.mov_imm(6, 1)                      # VRAM mode

    a.label("rl_common")
    a.ldr_post(2, 0, 4)                  # header, src += 4
    a.lsr_imm(2, 2, 8)                   # r2 = decompressed length
    a.mov_imm(8, 0)                      # no pending byte
    a.cmp_imm(2, 0)
    a.b("rl_done", cond=Asm.EQ)

    # ── Outer: read flag byte ─────────────────────────────────────────────
    a.label("rl_flag")
    a.cmp_imm(2, 0)
    a.b("rl_done", cond=Asm.EQ)
    a.ldrb_post(3, 0, 1)                 # r3 = flag byte
    a.tst_imm(3, 0x80)
    a.b("rl_compressed", cond=Asm.NE)

    # ── Uncompressed: copy (flag & 0x7F) + 1 literal bytes ───────────────
    a.and_imm(4, 3, 0x7F)
    a.add_imm(4, 4, 1)                   # r4 = count
    a.label("rl_uncopy")
    a.cmp_imm(2, 0)
    a.b("rl_done", cond=Asm.EQ)
    a.cmp_imm(4, 0)
    a.b("rl_flag", cond=Asm.EQ)
    a.ldrb_post(5, 0, 1)                 # r5 = literal byte
    # Write byte (WRAM or VRAM buffered)
    a.cmp_imm(6, 0)
    a.b("rl_unc_vram", cond=Asm.NE)
    a.strb_post(5, 1, 1)
    a.sub_imm(2, 2, 1)
    a.sub_imm(4, 4, 1)
    a.b("rl_uncopy")
    a.label("rl_unc_vram")
    a.cmp_imm(8, 0)
    a.b("rl_unc_v_flush", cond=Asm.NE)
    a.mov_reg(7, 5)
    a.mov_imm(8, 1)
    a.sub_imm(2, 2, 1)
    a.sub_imm(4, 4, 1)
    a.b("rl_uncopy")
    a.label("rl_unc_v_flush")
    a.lsl_imm(5, 5, 8)
    a.orr_reg(5, 7, 5)
    a.strh_imm(5, 1, 0)
    a.add_imm(1, 1, 2)
    a.mov_imm(8, 0)
    a.sub_imm(2, 2, 1)
    a.sub_imm(4, 4, 1)
    a.b("rl_uncopy")

    # ── Compressed: repeat one byte (flag & 0x7F) + 3 times ──────────────
    a.label("rl_compressed")
    a.and_imm(4, 3, 0x7F)
    a.add_imm(4, 4, 3)                   # r4 = run length
    a.ldrb_post(5, 0, 1)                 # r5 = repeated byte
    a.label("rl_fill")
    a.cmp_imm(2, 0)
    a.b("rl_done", cond=Asm.EQ)
    a.cmp_imm(4, 0)
    a.b("rl_flag", cond=Asm.EQ)
    a.cmp_imm(6, 0)
    a.b("rl_fill_vram", cond=Asm.NE)
    a.strb_post(5, 1, 1)
    a.sub_imm(2, 2, 1)
    a.sub_imm(4, 4, 1)
    a.b("rl_fill")
    a.label("rl_fill_vram")
    a.cmp_imm(8, 0)
    a.b("rl_fill_v_flush", cond=Asm.NE)
    a.mov_reg(7, 5)
    a.mov_imm(8, 1)
    a.sub_imm(2, 2, 1)
    a.sub_imm(4, 4, 1)
    a.b("rl_fill")
    a.label("rl_fill_v_flush")
    a.lsl_imm(12, 5, 8)
    a.orr_reg(12, 7, 12)
    a.strh_imm(12, 1, 0)
    a.add_imm(1, 1, 2)
    a.mov_imm(8, 0)
    a.sub_imm(2, 2, 1)
    a.sub_imm(4, 4, 1)
    a.b("rl_fill")

    # ── Done ──────────────────────────────────────────────────────────────
    a.label("rl_done")
    a.cmp_imm(6, 0)
    a.b("rl_exit", cond=Asm.EQ)
    a.cmp_imm(8, 0)
    a.b("rl_exit", cond=Asm.EQ)
    a.strh_imm(7, 1, 0)                  # flush pending byte
    a.label("rl_exit")
    a.ldmfd(SP, R(0,1,2,3,4,5,6,7,8))
    a.b("swi_return")

    # ─── Sin lookup table ──────────────────────────────────────────────────
    # 256 entries of sin(i * 2*pi/256) * 32768, stored as signed 16-bit LE.
    # cos(i) = sin(i + 64). The table is pure math — not copied from any ROM.
    import math
    sin_table = []
    for i in range(256):
        val = round(math.sin(i * 2 * math.pi / 256) * 32768)
        val = max(-32768, min(32767, val))
        sin_table.append(val & 0xFFFF)

    # Align to 4-byte boundary for LDR convenience.
    while a.pc % 4 != 0:
        a.emit(0)
    a.label("sin_table")
    for i in range(0, 256, 2):
        # Pack two 16-bit entries per 32-bit word.
        lo = sin_table[i]
        hi = sin_table[i + 1]
        a.emit(lo | (hi << 16))

    # ─── SWI 0x0F: ObjAffineSet(r0=src, r1=dst, r2=count, r3=stride) ────
    # For each entry:
    #   src: { i16 sx, i16 sy, u16 angle, u16 pad }  (8 bytes)
    #   dst: { i16 pa, i16 pb, i16 pc, i16 pd } at stride*2 byte intervals
    #
    # pa =  (cos * 2) / sx,  pb =  (sin * 2) / sx
    # pc = -(sin * 2) / sy,  pd =  (cos * 2) / sy
    # (cos/sin are 1.15 fixed-point from table; sx/sy are 8.8; result is 8.8)
    #
    # Computes each value via signed_div, writes one at a time advancing
    # dst by stride*2 bytes between each parameter.

    a.label("swi_obj_affine_set")
    a.stmfd(SP, R(0,1,2,3,4,5,6,7,8,9,10,LR))
    a.cmp_imm(2, 0)
    a.b("oas_done", cond=Asm.EQ)
    a.lsl_imm(9, 3, 1)                # r9 = stride * 2 (bytes)
    # Load sin_table address
    a.ldr_imm(10, PC, 0)
    a.b("oas_start")
    a.label("oas_table_lit")
    a.emit(0)                          # placeholder for sin_table address
    oas_table_lit_pc = a.pc - 4

    a.label("oas_start")
    a.label("oas_loop")
    # Read src
    a.ldrsh_imm(6, 0, 0)              # sx
    a.ldrsh_imm(7, 0, 2)              # sy
    a.ldrh_imm(8, 0, 4)               # angle
    a.add_imm(0, 0, 8)                # src += 8

    # Lookup sin/cos
    a.lsr_imm(8, 8, 8)                # index = angle >> 8
    a.lsl_imm(11, 8, 1)
    a.add_reg(11, 10, 11)
    a.ldrsh_imm(4, 11, 0)             # r4 = sin
    a.add_imm(8, 8, 64)
    a.and_imm(8, 8, 0xFF)
    a.lsl_imm(11, 8, 1)
    a.add_reg(11, 10, 11)
    a.ldrsh_imm(5, 11, 0)             # r5 = cos

    # Skip if scale is zero
    a.cmp_imm(6, 0)
    a.b("oas_skip", cond=Asm.EQ)
    a.cmp_imm(7, 0)
    a.b("oas_skip", cond=Asm.EQ)

    # pa = (cos * 2) / sx — call signed_div(cos*2, sx)
    a.stmfd(SP, R(0,1))               # save src, dst
    a.lsl_imm(0, 5, 1)                # r0 = cos * 2
    a.mov_reg(1, 6)
    a.bl("signed_div")                 # r0 = pa
    a.ldmfd(SP, R(3,8))               # r3 = saved src, r8 = saved dst
    # Write pa, advance dst
    a.strh_imm(0, 8, 0)
    a.add_reg(8, 8, 9)                # dst += stride_bytes

    # pb = (sin * 2) / sx
    a.stmfd(SP, R(3,8))
    a.lsl_imm(0, 4, 1)                # r0 = sin * 2
    a.mov_reg(1, 6)
    a.bl("signed_div")
    a.ldmfd(SP, R(3,8))
    a.strh_imm(0, 8, 0)
    a.add_reg(8, 8, 9)

    # pc = -(sin * 2) / sy
    a.stmfd(SP, R(3,8))
    a.rsb_imm(0, 4, 0)                # r0 = -sin
    a.lsl_imm(0, 0, 1)                # r0 = -sin * 2
    a.mov_reg(1, 7)
    a.bl("signed_div")
    a.ldmfd(SP, R(3,8))
    a.strh_imm(0, 8, 0)
    a.add_reg(8, 8, 9)

    # pd = (cos * 2) / sy
    a.stmfd(SP, R(3,8))
    a.lsl_imm(0, 5, 1)                # r0 = cos * 2
    a.mov_reg(1, 7)
    a.bl("signed_div")
    a.ldmfd(SP, R(3,8))
    a.strh_imm(0, 8, 0)
    a.add_reg(8, 8, 9)

    # Restore src/dst for next iteration
    a.mov_reg(0, 3)                    # r0 = src (was saved in r3)
    a.mov_reg(1, 8)                    # r1 = dst (advanced past 4 writes)
    a.b("oas_next")

    a.label("oas_skip")
    # Zero scale: skip this entry, advance dst by 4*stride
    a.add_reg(1, 1, 9)
    a.add_reg(1, 1, 9)
    a.add_reg(1, 1, 9)
    a.add_reg(1, 1, 9)

    a.label("oas_next")
    a.subs_imm(2, 2, 1)
    a.b("oas_loop", cond=Asm.NE)

    a.label("oas_done")
    a.ldmfd(SP, R(0,1,2,3,4,5,6,7,8,9,10,LR))
    a.b("swi_return")

    # ─── SWI 0x0E: BgAffineSet(r0=src, r1=dst, r2=count) ─────────────────
    # For each entry:
    #   src (20 bytes): { i32 orig_cx, i32 orig_cy, i16 disp_cx, i16 disp_cy,
    #                      i16 sx, i16 sy, u16 angle, u16 pad }
    #   dst (16 bytes): { i16 pa, i16 pb, i16 pc, i16 pd, i32 dx, i32 dy }
    #
    # Same trig as ObjAffine but also computes displacement:
    #   dx = orig_cx - (pa * disp_cx + pb * disp_cy)
    #   dy = orig_cy - (pc * disp_cx + pd * disp_cy)
    a.label("swi_bg_affine_set")
    a.stmfd(SP, R(0,1,2,3,4,5,6,7,8,9,10,LR))
    a.cmp_imm(2, 0)
    a.b("bas_done", cond=Asm.EQ)
    # Load sin_table address
    a.ldr_imm(10, PC, 0)
    a.b("bas_start")
    a.label("bas_table_lit")
    a.emit(0)                          # placeholder
    bas_table_lit_pc = a.pc - 4

    a.label("bas_start")
    a.label("bas_loop")
    # Read src (20 bytes)
    a.ldr_imm(3, 0, 0)                # r3 = orig_cx (i32)
    a.ldr_imm(8, 0, 4)                # r8 = orig_cy (i32) — stash for later
    a.ldrsh_imm(4, 0, 8)              # r4 = disp_cx (i16)
    a.ldrsh_imm(5, 0, 10)             # r5 = disp_cy (i16)
    a.ldrsh_imm(6, 0, 12)             # r6 = sx (i16)
    a.ldrsh_imm(7, 0, 14)             # r7 = sy (i16)
    a.ldrh_imm(9, 0, 16)              # r9 = angle (u16)
    a.add_imm(0, 0, 20)               # src += 20

    # Lookup sin/cos
    a.lsr_imm(9, 9, 8)                # index
    a.lsl_imm(11, 9, 1)
    a.add_reg(11, 10, 11)
    a.ldrsh_imm(9, 11, 0)             # r9 = sin (reuse r9 since angle consumed)
    # Now we need cos. But we've used all low registers. Use stack.
    a.stmfd(SP, R(3,4,5,8))           # save orig_cx, disp_cx, disp_cy, orig_cy
    a.add_imm(11, 11, 128)            # +64 entries * 2 bytes = +128
    # But wait — we need to wrap at 256 entries (512 bytes). The add might
    # go past the table end. Recalculate properly.
    a.pc -= 4                          # back out
    a.lsr_imm(11, 11, 1)              # back to index (undo the LSL)
    a.sub_reg(11, 11, 10)             # r11 = byte offset from table start
    a.lsr_imm(11, 11, 1)              # r11 = entry index
    a.add_imm(11, 11, 64)
    a.and_imm(11, 11, 0xFF)
    a.lsl_imm(11, 11, 1)
    a.add_reg(11, 10, 11)
    a.ldrsh_imm(3, 11, 0)             # r3 = cos (temporarily in r3, save later)

    # Now: r9 = sin, r3 = cos, r6 = sx, r7 = sy
    # Compute pa/pb/pc/pd using signed_div, same as ObjAffine.
    # But we also need disp_cx, disp_cy, orig_cx, orig_cy for displacement.

    # Skip if scale is zero
    a.cmp_imm(6, 0)
    a.b("bas_skip_entry", cond=Asm.EQ)
    a.cmp_imm(7, 0)
    a.b("bas_skip_entry", cond=Asm.EQ)

    a.mov_reg(5, 3)                    # r5 = cos (free up r3)
    # Stack has: [orig_cx, disp_cx, disp_cy, orig_cy] (from stmfd)
    # pa = (cos * 2) / sx
    a.stmfd(SP, R(0,1))               # save src, dst
    a.lsl_imm(0, 5, 1)                # cos * 2
    a.mov_reg(1, 6)
    a.bl("signed_div")
    a.mov_reg(3, 0)                    # r3 = pa
    # pb = (sin * 2) / sx
    a.lsl_imm(0, 9, 1)                # sin * 2
    a.mov_reg(1, 6)
    a.bl("signed_div")
    a.mov_reg(4, 0)                    # r4 = pb
    # pc = -(sin * 2) / sy
    a.rsb_imm(0, 9, 0)                # -sin
    a.lsl_imm(0, 0, 1)                # -sin * 2
    a.mov_reg(1, 7)
    a.bl("signed_div")
    a.mov_reg(8, 0)                    # r8 = pc
    # pd = (cos * 2) / sy
    a.lsl_imm(0, 5, 1)                # cos * 2
    a.mov_reg(1, 7)
    a.bl("signed_div")
    a.mov_reg(11, 0)                   # r11 = pd
    a.ldmfd(SP, R(0,1))               # restore src, dst
    # r3=pa, r4=pb, r8=pc, r11=pd

    # Write pa, pb, pc, pd to dst
    a.strh_imm(3, 1, 0)               # dst+0 = pa
    a.strh_imm(4, 1, 2)               # dst+2 = pb
    a.strh_imm(8, 1, 4)               # dst+4 = pc
    a.strh_imm(11, 1, 6)              # dst+6 = pd

    # Compute dx = orig_cx - (pa * disp_cx + pb * disp_cy)
    # dy = orig_cy - (pc * disp_cx + pd * disp_cy)
    # Recover disp_cx, disp_cy, orig_cx, orig_cy from stack
    a.ldmfd(SP, R(5,6,7,9))           # r5=orig_cx, r6=disp_cx, r7=disp_cy, r9=orig_cy
    # pa(r3), pb(r4) are sign-extended i16. disp_cx(r6), disp_cy(r7) are i16 (sign-extended from ldrsh).
    # pa*disp_cx: i16*i16 → i32. MUL works. But pa was truncated to 16 bits.
    # Sign-extend pa, pb, pc, pd from 16 to 32 bits:
    a.lsl_imm(3, 3, 16)
    a.asr_imm(3, 3, 16)               # sign-extend pa
    a.lsl_imm(4, 4, 16)
    a.asr_imm(4, 4, 16)               # pb
    a.lsl_imm(8, 8, 16)
    a.asr_imm(8, 8, 16)               # pc
    a.lsl_imm(11, 11, 16)
    a.asr_imm(11, 11, 16)             # pd

    # dx = orig_cx - (pa * disp_cx + pb * disp_cy)
    a.mul(12, 6, 3)                    # r12 = disp_cx * pa
    a.stmfd(SP, R(12))
    a.mul(12, 7, 4)                    # r12 = disp_cy * pb
    a.ldmfd(SP, R(3))                 # r3 = disp_cx * pa
    a.add_reg(3, 3, 12)               # r3 = pa*disp_cx + pb*disp_cy
    a.sub_reg(3, 5, 3)                # r3 = orig_cx - (pa*dcx + pb*dcy) = dx
    a.str_imm(3, 1, 8)                # dst+8 = dx

    # dy = orig_cy - (pc * disp_cx + pd * disp_cy)
    a.mul(12, 6, 8)                    # r12 = disp_cx * pc
    a.stmfd(SP, R(12))
    a.mul(12, 7, 11)                   # r12 = disp_cy * pd
    a.ldmfd(SP, R(3))                 # r3 = disp_cx * pc
    a.add_reg(3, 3, 12)
    a.sub_reg(3, 9, 3)                # dy = orig_cy - (pc*dcx + pd*dcy)
    a.str_imm(3, 1, 12)               # dst+12 = dy

    a.add_imm(1, 1, 16)               # dst += 16
    a.b("bas_next")

    a.label("bas_skip_entry")
    a.add_imm(SP, SP, 16)             # pop the 4 saved regs we won't use
    a.add_imm(1, 1, 16)

    a.label("bas_next")
    a.subs_imm(2, 2, 1)
    a.b("bas_loop", cond=Asm.NE)

    a.label("bas_done")
    a.ldmfd(SP, R(0,1,2,3,4,5,6,7,8,9,10,LR))
    a.b("swi_return")

    # ─── SWI 0x09: ArcTan(r0=tan) → r0=angle ───────────────────────────────
    # Input: r0 = tan in 1.14 fixed-point (i.e., tan * 16384).
    # Output: r0 = angle in range [-0x4000, +0x4000], where 0x4000 = pi/2.
    #
    # Uses a polynomial approximation: atan(x) ≈ x - x³/3 + x⁵/5 - x⁷/7
    # In fixed-point with x in 1.14:
    #   a = x
    #   a -= (x*x*x) >> 28 / 3
    # This is complex in ARM. A simpler approach: table lookup + interpolation.
    # But for now, use a linear approximation from the sin table (arctan ≈ arcsin
    # for small angles, and search the sin table for larger ones).
    #
    # Simplest correct approach: walk the sin/cos table backwards.
    # atan(t) = angle where sin(angle)/cos(angle) = t.
    # For each table entry i: if sin[i] * 16384 / cos[i] >= |t|, we found it.
    # This is O(256) but the BIOS is only ~100 cycles anyway.
    #
    # Actually, the real BIOS uses a CORDIC-like approach. Let's do a simple
    # table search: linear scan of sin table, find where sin/cos crosses t.
    a.label("swi_arctan")
    a.stmfd(SP, R(1,2,3,4,5,6,LR))
    # r0 = tan (signed 1.14 fixed-point)
    a.mov_reg(6, 0)                    # r6 = input tan
    # Handle sign: work with |tan|, negate result at end if negative
    a.cmp_imm(0, 0)
    a.rsb_imm(0, 0, 0, cond=Asm.MI)  # r0 = |tan|
    # Load sin_table base
    a.ldr_imm(5, PC, 0)
    a.b("at_search")
    a.emit(0)                          # placeholder for sin_table address
    at_table_lit_pc = a.pc - 4

    # Search: find i where sin[i]/cos[i] >= |tan| (in 1.14 space)
    # i.e., sin[i] * 16384 >= |tan| * cos[i]
    # Scan from i=0 to i=63 (first quadrant only, 0 to pi/2)
    a.label("at_search")
    a.mov_imm(1, 0)                    # r1 = i (search index)
    a.label("at_scan")
    a.cmp_imm(1, 63)
    a.b("at_found_max", cond=Asm.GE)  # past pi/4 in table — clamp

    # sin[i]
    a.lsl_imm(2, 1, 1)                # byte offset
    a.add_reg(2, 5, 2)
    a.ldrsh_imm(3, 2, 0)              # r3 = sin[i]
    # cos[i] = sin[(i+64)&0xFF]
    a.add_imm(4, 1, 64)
    a.and_imm(4, 4, 0xFF)
    a.lsl_imm(4, 4, 1)
    a.add_reg(4, 5, 4)
    a.ldrsh_imm(4, 4, 0)              # r4 = cos[i]

    # Check: sin[i] * 16384 >= |tan| * cos[i]
    # To avoid overflow, compare sin[i] * 16384 vs |tan| * cos[i]
    # Both sin and cos are ≤ 32768, tan is ≤ 16384, so products fit in 32 bits.
    a.mul(2, 3, 0)                     # Wait: MUL rd != rm. r2 = sin * 16384... no.
    # We need: sin[i] * 16384 vs |tan| * cos[i]
    # Use a constant 16384 = 0x4000
    a.mov_imm(2, 0x40)
    a.lsl_imm(2, 2, 8)                # r2 = 0x4000 = 16384

    a.mul(3, 2, 3)                     # r3 = sin[i] * 16384 (r2=16384, MUL rd(3) != rm(2) ✓)
    a.mul(4, 0, 4)                     # r4 = |tan| * cos[i] (MUL rd(4) != rm(0) ✓)
    a.cmp_reg(3, 4)
    a.b("at_found", cond=Asm.GE)

    a.add_imm(1, 1, 1)
    a.b("at_scan")

    a.label("at_found")
    # i is the table index. Convert to GBA angle units.
    # Table has 256 entries for full circle. GBA ArcTan returns [-0x4000, +0x4000]
    # where 0x4000 = pi/2 = 64 table entries.
    # angle = i * 0x4000 / 64 = i * 256 = i << 8
    a.lsl_imm(0, 1, 8)                # r0 = angle
    a.b("at_fixsign")

    a.label("at_found_max")
    a.mov_imm(0, 0x40)
    a.lsl_imm(0, 0, 8)                # r0 = 0x4000
    a.label("at_fixsign")
    # Negate if original tan was negative
    a.cmp_imm(6, 0)
    a.rsb_imm(0, 0, 0, cond=Asm.MI)
    a.ldmfd(SP, R(1,2,3,4,5,6,LR))
    a.b("swi_return")

    # ─── SWI 0x0A: ArcTan2(r0=x, r1=y) → r0=angle ──────────────────────────
    # Two-argument arctangent. Returns angle in [0, 0xFFFF] for full circle.
    # atan2(y, x) using the ArcTan subroutine and quadrant correction.
    a.label("swi_arctan2")
    a.stmfd(SP, R(1,2,3,4,LR))
    a.mov_reg(2, 0)                    # r2 = x
    a.mov_reg(3, 1)                    # r3 = y
    # Handle special cases
    a.cmp_imm(2, 0)
    a.b("at2_xzero", cond=Asm.EQ)
    # Compute |y| * 16384 / |x| for the arctan input
    # tan = y * 16384 / x (both signed)
    # Use signed_div: r0 = y << 14, r1 = x
    a.lsl_imm(0, 3, 14)               # r0 = y << 14
    a.mov_reg(1, 2)                    # r1 = x
    a.bl("signed_div")                 # r0 = y*16384/x = tan (1.14 fixed-point)
    # Call ArcTan — but we need to call it as a subroutine, not SWI.
    # ArcTan expects r0=tan, returns r0=angle in [-0x4000, 0x4000].
    # We can just branch to the arctan body, but it saves/restores LR.
    # Simpler: inline the call by saving LR.
    a.bl("swi_arctan")                 # r0 = base angle
    # Wait — swi_arctan does ldmfd SP!, {.., LR}; b swi_return which does
    # ldmfd SP!, {r11,r12,LR}; movs pc, lr. That would return to the caller
    # of ArcTan2, not to us. We need a different entry point.
    # Let's create swi_arctan_inner that doesn't do the SWI save/restore.
    a.pc -= 4                          # back out the bl

    # Actually, let's restructure. Make arctan a BL-callable function
    # with its own STMFD/LDMFD that returns via BX LR (or MOV PC, LR).
    # The SWI entry point calls it and then does swi_return.
    # ... this is getting complicated. Let me simplify ArcTan2 differently.

    # Simple ArcTan2: use quadrant logic.
    # If x > 0: angle = arctan(y/x) mapped to [0, 0xFFFF]
    # If x < 0: angle = arctan(y/x) + pi
    # If x == 0: angle = pi/2 if y > 0, 3*pi/2 if y < 0
    #
    # ArcTan returns [-0x4000, 0x4000] = [-pi/2, pi/2]
    # Full circle: 0x10000 = 2*pi, so pi = 0x8000, pi/2 = 0x4000

    # Inline arctan search (duplicate code but avoids call complexity)
    # Actually, let me use a simpler approach: just compute angle from the
    # sin table directly. Find the entry whose sin/cos ratio best matches y/x.

    # Simplest correct ArcTan2:
    # 1. Compute abs_ratio = |y << 14| / |x|  (or |x << 14| / |y| if |y| > |x|)
    # 2. Search sin table for the angle
    # 3. Adjust for quadrant and whether we swapped x/y

    # For now, just use a basic implementation that handles the four quadrants.
    a.pc -= 8                          # back out all the broken code from "at2"
    a.pc = a.labels["swi_arctan2"]     # restart from label
    # Actually we can't easily delete labels. Let me just restart the function.
    # The label is already set. Just overwrite from here.

    a.label("swi_arctan2")
    a.stmfd(SP, R(1,2,3,4,5,6,7,LR))
    a.mov_reg(4, 0)                    # r4 = x
    a.mov_reg(5, 1)                    # r5 = y
    # Handle x == 0
    a.cmp_imm(4, 0)
    a.b("at2_xzero", cond=Asm.EQ)
    # Compute tan = (y << 14) / x via signed_div
    a.lsl_imm(0, 5, 14)
    a.mov_reg(1, 4)
    a.bl("signed_div")                 # r0 = tan (1.14 FP)
    # Save tan, call ArcTan logic inline
    # r0 = tan input; we need the angle in [-0x4000, 0x4000]
    # Inline the arctan search:
    a.mov_reg(6, 0)                    # r6 = tan
    a.cmp_imm(0, 0)
    a.rsb_imm(0, 0, 0, cond=Asm.MI)  # r0 = |tan|
    a.ldr_imm(7, PC, 0)
    a.b("at2_scan_start")
    a.emit(0)                          # placeholder for sin_table
    at2_table_lit_pc = a.pc - 4

    a.label("at2_scan_start")
    a.mov_imm(1, 0)                    # i = 0
    a.mov_imm(2, 0x40)
    a.lsl_imm(2, 2, 8)                # r2 = 16384
    a.label("at2_scan_loop")
    a.cmp_imm(1, 63)
    a.b("at2_scan_max", cond=Asm.GE)
    a.lsl_imm(3, 1, 1)
    a.add_reg(3, 7, 3)
    a.ldrsh_imm(3, 3, 0)              # sin[i]
    a.add_imm(8, 1, 64)
    a.and_imm(8, 8, 0xFF)
    a.lsl_imm(8, 8, 1)
    a.add_reg(8, 7, 8)
    a.ldrsh_imm(8, 8, 0)              # cos[i]
    a.mul(3, 2, 3)                     # sin * 16384
    a.mul(8, 0, 8)                     # |tan| * cos
    a.cmp_reg(3, 8)
    a.b("at2_scan_found", cond=Asm.GE)
    a.add_imm(1, 1, 1)
    a.b("at2_scan_loop")

    a.label("at2_scan_found")
    a.lsl_imm(0, 1, 8)                # base angle = i << 8
    a.b("at2_got_base")
    a.label("at2_scan_max")
    a.mov_imm(0, 0x40)
    a.lsl_imm(0, 0, 8)                # 0x4000

    a.label("at2_got_base")
    # Apply sign from tan
    a.cmp_imm(6, 0)
    a.rsb_imm(0, 0, 0, cond=Asm.MI)  # negate if tan was negative
    # r0 = base angle in [-0x4000, 0x4000]
    # Quadrant adjustment based on sign of x:
    # x > 0: result = base_angle (already correct, first/fourth quadrant)
    # x < 0: result = base_angle + 0x8000 (+pi)
    a.cmp_imm(4, 0)
    a.b("at2_map", cond=Asm.GT)
    # x < 0
    a.add_imm(0, 0, 0x80)
    a.add_imm(0, 0, 0x80)             # can't do +0x8000 in one imm...
    a.pc -= 8                          # back out
    a.mov_imm(1, 0x80)
    a.lsl_imm(1, 1, 8)                # r1 = 0x8000
    a.add_reg(0, 0, 1)                # angle += 0x8000

    a.label("at2_map")
    # Map from [-0x8000, 0x8000] to [0, 0xFFFF]
    # result = angle & 0xFFFF
    a.lsl_imm(0, 0, 16)
    a.lsr_imm(0, 0, 16)               # r0 = angle & 0xFFFF
    a.b("at2_done")

    a.label("at2_xzero")
    # x == 0: angle = 0x4000 (pi/2) if y > 0, 0xC000 (3pi/2) if y < 0, 0 if y == 0
    a.cmp_imm(5, 0)
    a.mov_imm(0, 0, cond=Asm.EQ)
    a.b("at2_done", cond=Asm.EQ)
    a.mov_imm(0, 0x40)
    a.lsl_imm(0, 0, 8)                # 0x4000
    a.cmp_imm(5, 0)
    a.b("at2_done", cond=Asm.GT)
    # y < 0: 0xC000
    a.mov_imm(0, 0xC0)
    a.lsl_imm(0, 0, 8)                # 0xC000

    a.label("at2_done")
    a.ldmfd(SP, R(1,2,3,4,5,6,7,LR))
    a.b("swi_return")

    # ─── Signed division helper (BL-callable) ───────────────────────────────
    # r0 = numerator (signed), r1 = denominator (signed)
    # Returns r0 = quotient (signed). Clobbers r2, r3, r12.
    a.label("signed_div")
    a.stmfd(SP, R(4,5,LR))
    a.eor_reg(5, 0, 1)                # r5 = sign of result (bit 31)
    a.cmp_imm(0, 0)
    a.rsb_imm(0, 0, 0, cond=Asm.MI)  # |num|
    a.cmp_imm(1, 0)
    a.rsb_imm(1, 1, 0, cond=Asm.MI)  # |den|
    # Unsigned divide: r0 / r1 → r3 = quot, r0 = rem
    a.mov_imm(3, 0)                    # quot = 0
    a.cmp_imm(1, 0)
    a.b("sdiv_done", cond=Asm.EQ)     # div by zero → return 0
    a.mov_reg(2, 1)                    # r2 = shifted denominator
    # Align
    a.label("sdiv_align")
    a.cmp_reg(2, 0)
    a.b("sdiv_loop", cond=Asm.CS)
    a.lsls_imm(2, 2, 1)
    a.b("sdiv_align", cond=Asm.CC)
    # Restoring loop
    a.label("sdiv_loop")
    a.cmp_reg(0, 2)
    a.sub_reg(0, 0, 2, cond=Asm.CS)
    a.adcs_reg(3, 3, 3)
    a.lsr_imm(2, 2, 1)
    a.cmp_reg(2, 1)
    a.b("sdiv_loop", cond=Asm.CS)
    a.label("sdiv_done")
    a.mov_reg(0, 3)                    # r0 = unsigned quotient
    a.cmp_imm(5, 0)
    a.rsb_imm(0, 0, 0, cond=Asm.MI)  # apply sign
    a.ldmfd(SP, R(4,5,PC))            # return (pop LR into PC)

    # ─── SWI 0x00: SoftReset ───────────────────────────────────────────────
    # Clear 0x03007E00-0x03007FFF, reset stacks, jump to ROM (or EWRAM if
    # flag at 0x03007FFA is nonzero). Real BIOS behavior per GBATEK.
    a.label("swi_soft_reset")
    a.stmfd(SP, R(0,1,2,3,4,5,6,7,8))
    # Clear 0x03007E00..0x03007FFF (512 bytes = 16 × 32-byte blocks)
    a.mov_imm(0, 0); a.mov_imm(1, 0); a.mov_imm(2, 0); a.mov_imm(3, 0)
    a.mov_imm(4, 0); a.mov_imm(5, 0); a.mov_imm(6, 0); a.mov_imm(7, 0)
    a.mov_imm(8, 0x03000000)
    a.add_imm(8, 8, 0x7E00)
    a.mov_imm(9, 16)                   # 16 blocks of 32 bytes = 512
    a.label("sr_clear")
    a.stmia(8, R(0,1,2,3,4,5,6,7))
    a.subs_imm(9, 9, 1)
    a.b("sr_clear", cond=Asm.NE)
    a.ldmfd(SP, R(0,1,2,3,4,5,6,7,8))
    # Check flag at 0x03007FFA
    a.mov_imm(0, 0x03000000)
    a.add_imm(0, 0, 0x7F00)
    a.ldrb_imm(1, 0, 0xFA)            # r1 = flag
    # Set up stacks (same as reset vector)
    a.mov_imm(0, 0x12); a.msr_cpsr_c(0)   # IRQ mode
    a.mov_imm(SP, 0x03000000); a.add_imm(SP, SP, 0x7F00); a.add_imm(SP, SP, 0xA0)
    a.mov_imm(0, 0x13); a.msr_cpsr_c(0)   # SVC mode
    a.mov_imm(SP, 0x03000000); a.add_imm(SP, SP, 0x7F00); a.add_imm(SP, SP, 0xE0)
    a.mov_imm(0, 0x1F); a.msr_cpsr_c(0)   # SYS mode
    a.mov_imm(SP, 0x03000000); a.add_imm(SP, SP, 0x7F00)
    # Jump: flag != 0 → EWRAM (0x02000000), else ROM (0x08000000)
    a.cmp_imm(1, 0)
    a.mov_imm(PC, 0x02000000, cond=Asm.NE)
    a.mov_imm(PC, 0x08000000, cond=Asm.EQ)

    # ─── SWI 0x03: Stop ─────────────────────────────────────────────────────
    # Deep sleep — write 0x80 to HALTCNT (0x04000301). Only wakes on
    # keypad/cartridge/serial interrupt.
    a.label("swi_stop")
    a.mov_imm(12, 0x04000000)
    a.mov_imm(11, 0x80)
    a.strb_imm(11, 12, 0x301)          # HALTCNT = 0x80 (Stop mode)
    a.b("swi_return")

    # ─── SWI 0x13: HuffUnComp(r0=src, r1=dst) ──────────────────────────────
    # Huffman decompression. Tree is encoded as 8-bit nodes; data is read
    # as 32-bit words with MSB first.
    #
    # Register plan:
    #   r0 = src (compressed data pointer, advances)
    #   r1 = dst (output pointer, advances)
    #   r2 = remaining output bytes
    #   r3 = bit_size (4 or 8, from header)
    #   r4 = tree_start (first node address)
    #   r5 = current 32-bit data word
    #   r6 = bit index within current word (31..0)
    #   r7 = current node address
    #   r8 = accumulator for output word
    #   r9 = bits accumulated in output word
    #   r10 = scratch
    #   r11 = scratch
    a.label("swi_huff_uncomp")
    a.stmfd(SP, R(0,1,2,3,4,5,6,7,8,9,10,11))
    a.ldr_post(2, 0, 4)                # r2 = header, src += 4
    a.and_imm(3, 2, 0x0F)              # r3 = bit_size
    a.lsr_imm(2, 2, 8)                 # r2 = decompressed length
    a.cmp_imm(2, 0)
    a.b("huf_done", cond=Asm.EQ)
    # Read tree size byte
    a.ldrb_post(10, 0, 1)              # r10 = tree_size_half
    a.lsl_imm(10, 10, 1)
    a.add_imm(10, 10, 1)               # tree_size = val * 2 + 1
    a.mov_reg(4, 0)                     # r4 = tree_start = src (current pos)
    a.add_reg(0, 0, 10)                 # src past tree → points at compressed data
    a.mov_imm(8, 0)                     # accumulator = 0
    a.mov_imm(9, 0)                     # bits in accumulator = 0

    # Outer: read a 32-bit data word
    a.label("huf_word")
    a.cmp_imm(2, 0)
    a.b("huf_done", cond=Asm.EQ)
    a.ldr_post(5, 0, 4)                # r5 = data word
    a.mov_imm(6, 31)                    # bit index = 31 (MSB first)

    # Per-bit: walk tree from root
    a.label("huf_bit")
    a.cmp_imm(2, 0)
    a.b("huf_done", cond=Asm.EQ)
    a.mov_reg(7, 4)                     # r7 = node = tree_start (root)

    a.label("huf_walk")
    a.ldrb_imm(10, 7, 0)               # r10 = node_val
    # Extract bit at position r6 from r5
    a.mov_imm(11, 1)
    a.lsl_reg(11, 11, 6)               # r11 = 1 << bit_index
    a.tst_reg(5, 11)                    # test data bit
    # child_offset = (node_val & 0x3F) * 2 + 2
    a.and_imm(11, 10, 0x3F)
    a.lsl_imm(11, 11, 1)
    a.add_imm(11, 11, 2)               # r11 = child_offset
    a.bic_imm(12, 7, 1)                # r12 = node & ~1 (align to pair)
    a.add_reg(12, 12, 11)              # r12 = base of child pair
    # If bit set (right child): +1, test bit 6 of node_val for leaf flag
    # If bit clear (left child): +0, test bit 7 of node_val for leaf flag
    a.b("huf_right", cond=Asm.NE)
    # Left child (bit=0)
    a.tst_imm(10, 0x80)                # bit 7 = left leaf flag
    a.b("huf_leaf", cond=Asm.NE)
    a.mov_reg(7, 12)                    # node = left child
    a.subs_imm(6, 6, 1)
    a.b("huf_word", cond=Asm.MI)       # used all 32 bits → next word
    a.b("huf_walk")

    a.label("huf_right")
    a.add_imm(12, 12, 1)               # right child = base + 1
    a.tst_imm(10, 0x40)                # bit 6 = right leaf flag
    a.b("huf_leaf", cond=Asm.NE)
    a.mov_reg(7, 12)
    a.subs_imm(6, 6, 1)
    a.b("huf_word", cond=Asm.MI)
    a.b("huf_walk")

    a.label("huf_leaf")
    a.ldrb_imm(10, 12, 0)              # r10 = leaf data byte
    a.orr_reg_lsl(8, 8, 10, 9)         # acc |= leaf << bits_in_acc
    a.add_reg(9, 9, 3)                 # bits_in_acc += bit_size
    a.cmp_imm(9, 32)
    a.b("huf_no_flush", cond=Asm.CC)
    a.str_post(8, 1, 4)                # *dst++ = accumulator
    a.mov_imm(8, 0)
    a.mov_imm(9, 0)
    a.sub_imm(2, 2, 4)                 # remaining -= 4
    a.label("huf_no_flush")
    a.subs_imm(6, 6, 1)
    a.b("huf_bit", cond=Asm.PL)        # >= 0 → more bits
    a.b("huf_word")                     # next 32-bit word

    a.label("huf_done")
    a.ldmfd(SP, R(0,1,2,3,4,5,6,7,8,9,10,11))
    a.b("swi_return")

    # ─── SWI 0x16/0x17: Diff8bitUnFilter (Wram/Vram) ────────────────────────
    # Reverse differential filter: output[0] = input[0], then
    # output[n] = output[n-1] + input[n] (cumulative sum of bytes).
    # WRAM variant writes bytes; VRAM variant buffers halfwords.
    #
    # Register plan:
    #   r0 = src, r1 = dst, r2 = remaining bytes, r3 = running sum,
    #   r4 = current byte, r5 = VRAM flag, r6 = pending byte, r7 = has pending
    a.label("swi_diff8_unfilt_wram")
    a.stmfd(SP, R(0,1,2,3,4,5,6,7))
    a.mov_imm(5, 0)                     # WRAM mode
    a.b("d8_common")

    a.label("swi_diff8_unfilt_vram")
    a.stmfd(SP, R(0,1,2,3,4,5,6,7))
    a.mov_imm(5, 1)                     # VRAM mode

    a.label("d8_common")
    a.ldr_post(2, 0, 4)                # header, src += 4
    a.lsr_imm(2, 2, 8)                 # decompressed length
    a.mov_imm(3, 0)                     # running sum = 0
    a.mov_imm(7, 0)                     # no pending byte
    a.cmp_imm(2, 0)
    a.b("d8_done", cond=Asm.EQ)

    a.label("d8_loop")
    a.ldrb_post(4, 0, 1)               # r4 = next delta byte
    a.add_reg(3, 3, 4)                 # sum += delta
    a.and_imm(3, 3, 0xFF)             # keep to 8 bits
    a.cmp_imm(5, 0)
    a.b("d8_vram", cond=Asm.NE)
    a.strb_post(3, 1, 1)               # WRAM: write byte
    a.subs_imm(2, 2, 1)
    a.b("d8_loop", cond=Asm.NE)
    a.b("d8_done")

    a.label("d8_vram")
    a.cmp_imm(7, 0)
    a.b("d8_v_flush", cond=Asm.NE)
    a.mov_reg(6, 3)                     # pending = sum
    a.mov_imm(7, 1)
    a.subs_imm(2, 2, 1)
    a.b("d8_loop", cond=Asm.NE)
    a.b("d8_v_trail")
    a.label("d8_v_flush")
    a.lsl_imm(4, 3, 8)
    a.orr_reg(4, 6, 4)                 # halfword = pending | (sum << 8)
    a.strh_imm(4, 1, 0)
    a.add_imm(1, 1, 2)
    a.mov_imm(7, 0)
    a.subs_imm(2, 2, 1)
    a.b("d8_loop", cond=Asm.NE)

    a.label("d8_v_trail")
    # Flush trailing pending byte in VRAM mode
    a.cmp_imm(7, 0)
    a.strh_imm(6, 1, 0, cond=Asm.NE)
    a.label("d8_done")
    a.ldmfd(SP, R(0,1,2,3,4,5,6,7))
    a.b("swi_return")

    # ─── SWI 0x18: Diff16bitUnFilter ────────────────────────────────────────
    # Same as Diff8 but operates on 16-bit units. Always writes halfwords.
    a.label("swi_diff16_unfilt")
    a.stmfd(SP, R(0,1,2,3,4))
    a.ldr_post(2, 0, 4)                # header, src += 4
    a.lsr_imm(2, 2, 8)                 # decompressed length (bytes)
    a.mov_imm(3, 0)                     # running sum = 0
    a.cmp_imm(2, 0)
    a.b("d16_done", cond=Asm.EQ)

    a.label("d16_loop")
    a.ldrh_imm(4, 0, 0)
    a.add_imm(0, 0, 2)                 # src += 2
    a.add_reg(3, 3, 4)                 # sum += delta
    # Keep to 16 bits
    a.lsl_imm(3, 3, 16)
    a.lsr_imm(3, 3, 16)
    a.strh_imm(3, 1, 0)
    a.add_imm(1, 1, 2)                 # dst += 2
    a.subs_imm(2, 2, 2)                # remaining -= 2
    a.b("d16_loop", cond=Asm.NE)

    a.label("d16_done")
    a.ldmfd(SP, R(0,1,2,3,4))
    a.b("swi_return")

    # ─── SWI 0x19: SoundBias(r0=level) ──────────────────────────────────────
    # Gradually ramp SOUNDBIAS (0x04000088) toward target level.
    # r0 = 0 → target 0x000, else target 0x200.
    # Real BIOS increments/decrements by 1 with a delay of 8 cycles per step.
    # We simplify: just set the target immediately. The gradual ramp is
    # cosmetic (avoids audio pop) and doesn't affect game logic.
    a.label("swi_sound_bias")
    a.stmfd(SP, R(1,2))
    a.mov_imm(1, 0x04000000)
    a.cmp_imm(0, 0)
    a.mov_imm(2, 0, cond=Asm.EQ)
    a.mov_imm(2, 0x02, cond=Asm.NE)
    a.lsl_imm(2, 2, 8, cond=Asm.NE)   # r2 = 0x200 if r0 != 0, else 0
    a.strh_imm(2, 1, 0x88)             # SOUNDBIAS = target
    a.ldmfd(SP, R(1,2))
    a.b("swi_return")

    # ─── SWI 0x1F: MidiKey2Freq(r0=wa, r1=mk, r2=fp) → r0=freq ─────────
    # freq = wave_freq * 2^((mk - 180 + fp/256) / 12)
    # where wave_freq = *(u32*)(wa + 4).
    #
    # The 2^(x/12) term is the hard part. We use a lookup table:
    # 768 entries (64 per semitone × 12 semitones) would be large. Instead,
    # use a 12-entry table for the semitone part and linear interpolation
    # for the fine part.
    #
    # 2^(n/12) for n=0..11 in 16.16 fixed-point:
    #   n=0: 65536, n=1: 69433, n=2: 73562, n=3: 77936,
    #   n=4: 82570, n=5: 87480, n=6: 92682, n=7: 98193,
    #   n=8: 104032, n=9: 110218, n=10: 116772, n=11: 123715
    #
    # For a full octave shift: multiply by 2 (or divide by 2) per 12 semitones.
    #
    # Algorithm:
    #   1. note = mk * 256 + fp  (combine into 8.8 fixed-point semitones)
    #   2. note -= 180 * 256     (center: MIDI key 180 = no shift)
    #   3. octave = note / (12 * 256)  (signed division, round toward -inf)
    #   4. remainder = note - octave * 12 * 256  (0..3071)
    #   5. semitone = remainder / 256  (0..11)
    #   6. fine = remainder & 0xFF     (0..255)
    #   7. base = table[semitone]
    #   8. next = table[semitone+1] (wraps: table[12] = table[0] * 2)
    #   9. factor = base + (next - base) * fine / 256  (16.16 interpolated)
    #  10. freq = wave_freq * factor >> 16
    #  11. Apply octave: shift left/right by |octave|
    #  12. Return freq (already in the right unit)
    #
    # This is complex but correct. Let's embed the table and implement.

    # 2^(n/12) table, 13 entries (entry 12 = 2 * entry 0 for interpolation)
    while a.pc % 4 != 0:
        a.emit(0)
    a.label("exp2_12_table")
    exp2_vals = [round(65536 * (2 ** (n / 12))) for n in range(13)]
    for i in range(0, 13, 1):
        a.emit(exp2_vals[i])

    a.label("swi_midi_key2freq")
    a.stmfd(SP, R(1,2,3,4,5,6,7,LR))
    # r0 = wa, r1 = mk, r2 = fp
    a.ldr_imm(3, 0, 4)                 # r3 = wave_freq = *(wa + 4)
    # note = mk * 256 + fp - 180 * 256
    a.lsl_imm(4, 1, 8)                 # r4 = mk * 256
    a.add_reg(4, 4, 2)                 # r4 = mk * 256 + fp
    # 180 * 256 = 46080 = 0xB400
    a.mov_imm(5, 0xB4)
    a.lsl_imm(5, 5, 8)                 # r5 = 0xB400
    a.sub_reg(4, 4, 5)                 # r4 = note (signed, in 8.8 semitones)

    # Divide by 12*256 = 3072 to get octave.
    # Use signed_div: r0 = note, r1 = 3072
    a.stmfd(SP, R(3))                  # save wave_freq
    a.mov_reg(0, 4)                     # numerator = note
    a.mov_imm(1, 0x0C)
    a.lsl_imm(1, 1, 8)                 # r1 = 3072
    a.bl("signed_div")                  # r0 = octave (signed)
    a.mov_reg(5, 0)                     # r5 = octave

    # remainder = note - octave * 3072
    a.mov_imm(1, 0x0C)
    a.lsl_imm(1, 1, 8)
    a.mul(6, 1, 5)                      # r6 = octave * 3072 (MUL: rd=6 != rm=1 ✓)
    a.sub_reg(6, 4, 6)                 # r6 = remainder
    # If remainder < 0, adjust: octave -= 1, remainder += 3072
    a.cmp_imm(6, 0)
    a.b("mk_rem_ok", cond=Asm.GE)
    a.sub_imm(5, 5, 1)
    a.mov_imm(1, 0x0C)
    a.lsl_imm(1, 1, 8)
    a.add_reg(6, 6, 1)
    a.label("mk_rem_ok")
    # r6 = remainder (0..3071)

    # semitone = remainder / 256, fine = remainder & 0xFF
    a.lsr_imm(7, 6, 8)                 # r7 = semitone (0..11)
    a.and_imm(6, 6, 0xFF)             # r6 = fine (0..255)

    # Load table base
    a.ldr_imm(1, PC, 0)
    a.b("mk_interp")
    a.emit(0)                           # placeholder for exp2_12_table address
    mk_table_lit_pc = a.pc - 4

    a.label("mk_interp")
    # base = table[semitone], next = table[semitone + 1]
    a.lsl_imm(0, 7, 2)                 # offset = semitone * 4
    a.add_reg(0, 1, 0)                 # addr = table + offset
    a.ldr_imm(0, 0, 0)                 # r0 = base (table[semitone])
    a.lsl_imm(2, 7, 2)
    a.add_imm(2, 2, 4)
    a.add_reg(2, 1, 2)
    a.ldr_imm(2, 2, 0)                 # r2 = next (table[semitone+1])

    # factor = base + (next - base) * fine / 256
    a.sub_reg(2, 2, 0)                 # r2 = next - base
    a.mul(2, 6, 2)                      # r2 = (next - base) * fine (MUL: rd=2 != rm=6 ✓)
    a.lsr_imm(2, 2, 8)                 # r2 = ... / 256
    a.add_reg(0, 0, 2)                 # r0 = factor (16.16)

    # freq = wave_freq * factor >> 16
    a.ldmfd(SP, R(3))                  # restore wave_freq
    # Use SMULL for 32×32→64, take high 32 bits (effectively >> 32, but we
    # want >> 16, so take the high word and shift)
    # Actually: wave_freq * factor. Both are 32-bit. Result can be up to
    # ~4M * 131072 = huge. Use UMULL.
    # SMULL RdLo, RdHi, Rm, Rs: result = RdHi:RdLo = Rm * Rs
    a.smull(1, 0, 3, 0)                # r0:r1 = wave_freq * factor
    # We want (wave_freq * factor) >> 16 = (r0 << 16) | (r1 >> 16)
    # But r0 = high 32, r1 = low 32 of 64-bit result.
    # Wait: SMULL convention: RdLo = low 32, RdHi = high 32.
    # So r1 = low, r0 = high. (wave_freq * factor) >> 16:
    a.lsr_imm(1, 1, 16)               # r1 = low >> 16
    a.lsl_imm(2, 0, 16)               # r2 = high << 16
    a.orr_reg(0, 1, 2)                # r0 = (high << 16) | (low >> 16)

    # Apply octave shift
    a.cmp_imm(5, 0)
    a.b("mk_oct_neg", cond=Asm.MI)
    # Positive octave: shift left (multiply by 2^octave)
    a.cmp_imm(5, 31)
    a.b("mk_oct_clamp", cond=Asm.GT)
    a.lsl_reg(0, 0, 5)
    a.b("mk_done")
    a.label("mk_oct_clamp")
    a.mov_imm(0, 0)                     # overflow → 0 (shouldn't happen in practice)
    a.b("mk_done")
    a.label("mk_oct_neg")
    a.rsb_imm(5, 5, 0)                 # r5 = |octave|
    a.cmp_imm(5, 31)
    a.b("mk_oct_clamp", cond=Asm.GT)
    a.lsr_reg(0, 0, 5)

    a.label("mk_done")
    a.ldmfd(SP, R(1,2,3,4,5,6,7,LR))
    a.b("swi_return")

    # ─── Fixup literal pools ────────────────────────────────────────────────
    sin_addr = a.labels["sin_table"]
    struct.pack_into("<I", a.code, oas_table_lit_pc, sin_addr)
    struct.pack_into("<I", a.code, bas_table_lit_pc, sin_addr)
    struct.pack_into("<I", a.code, at_table_lit_pc, sin_addr)
    struct.pack_into("<I", a.code, at2_table_lit_pc, sin_addr)
    exp2_addr = a.labels["exp2_12_table"]
    struct.pack_into("<I", a.code, mk_table_lit_pc, exp2_addr)

    # ─── Reset / hang ───────────────────────────────────────────────────────
    a.label("reset")
    # skip_bios should set PC=0x08000000 directly. If we ever execute this,
    # set up stacks like the real BIOS would and jump to ROM.
    a.mov_imm(0, 0x12); a.msr_cpsr_c(0)            # IRQ mode
    a.mov_imm(SP, 0x03000000); a.add_imm(SP, SP, 0x7F00); a.add_imm(SP, SP, 0xA0)
    a.mov_imm(0, 0x13); a.msr_cpsr_c(0)            # SVC mode
    a.mov_imm(SP, 0x03000000); a.add_imm(SP, SP, 0x7F00); a.add_imm(SP, SP, 0xE0)
    a.mov_imm(0, 0x1F); a.msr_cpsr_c(0)            # SYS mode
    a.mov_imm(SP, 0x03000000); a.add_imm(SP, SP, 0x7F00)
    a.mov_imm(LR, 0x08000000)
    a.mov_reg(PC, LR)

    a.label("hang")
    a.b("hang")

    a.fixup()
    return bytes(a.code)


def emit_c_header(data, out):
    # Trim trailing zeros — the consumer zero-fills to 0x4000. Saves ~14 KiB
    # of source noise; the actual code+tables fit in well under 2 KiB.
    end = len(data)
    while end > 0 and data[end-1] == 0:
        end -= 1
    end = (end + 15) & ~15  # round up to 16 for tidy hex rows

    with open(out, "w") as f:
        f.write("// Generated by spec/gen_bios_stub.py — do not edit.\n")
        f.write("// SWI dispatch, 27 handlers: 00-0F 10-13 14-15 16-18 19 1F.\n")
        f.write("// IRQ handler at 0x128 (updates BIOS_IF before calling game handler).\n")
        f.write("// Pad to 0x4000 with zeros at the use site.\n\n")
        f.write(f"static const unsigned char g_bios_stub[{end}] = {{\n")
        for i in range(0, end, 16):
            row = ", ".join(f"0x{b:02x}" for b in data[i:i+16])
            f.write(f"    {row},\n")
        f.write("};\n")


if __name__ == "__main__":
    data = build()
    out_bin = sys.argv[1] if len(sys.argv) > 1 else "gba_bios_stub.bin"
    with open(out_bin, "wb") as f:
        f.write(data)
    print(f"wrote {out_bin} ({len(data)} bytes)")

    # If a .h path is given, also emit a C array for embedding in shims.
    if len(sys.argv) > 2:
        emit_c_header(data, sys.argv[2])
        print(f"wrote {sys.argv[2]}")

    print(f"  IRQ:   0x128  (updates BIOS_IF, 11 instructions)")
    print(f"  SWI:   0x160  (dispatch, 27 handlers: 00-0F 10-13 14-15 16-18 19 1F)")
    print(f"  Reset: stack setup → 0x08000000")
