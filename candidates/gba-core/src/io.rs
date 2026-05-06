//! I/O register block (0x0400_0000 – 0x0400_03FE).
//!
//! Only the registers needed for basic operation are implemented so far.
//! Unimplemented reads return 0; unimplemented writes are silently dropped.

use crate::ppu::Ppu;
use crate::scheduler::Scheduler;

pub struct IoRegs {
    // Interrupt control
    pub ime: bool,
    pub ie: u16,
    pub if_: u16,

    // Key input
    pub keyinput: u16, // 0=pressed, active-low

    // Halt
    pub haltcnt: bool,

    // WAITCNT
    pub waitcnt: u16,
}

impl IoRegs {
    pub fn new() -> Self {
        Self {
            ime: false,
            ie: 0,
            if_: 0,
            keyinput: 0x03FF,
            haltcnt: false,
            waitcnt: 0,
        }
    }

    pub fn irq_pending(&self) -> bool {
        self.ime && (self.ie & self.if_) != 0
    }

    pub fn read32(&self, addr: u32, ppu: &Ppu, _sched: &Scheduler) -> u32 {
        let lo = self.read16(addr, ppu) as u32;
        let hi = self.read16(addr + 2, ppu) as u32;
        lo | (hi << 16)
    }

    pub fn read16(&self, addr: u32, ppu: &Ppu) -> u16 {
        match addr & 0x3FE {
            // PPU
            0x000 => ppu.dispcnt,
            0x004 => ppu.dispstat,
            0x006 => ppu.vcount,
            0x008 => ppu.bg[0].cnt,
            0x00A => ppu.bg[1].cnt,
            0x00C => ppu.bg[2].cnt,
            0x00E => ppu.bg[3].cnt,

            // BG scroll (write-only on real HW, but needed for 8-bit write RMW)
            0x010 => ppu.bg[0].xofs,
            0x012 => ppu.bg[0].yofs,
            0x014 => ppu.bg[1].xofs,
            0x016 => ppu.bg[1].yofs,
            0x018 => ppu.bg[2].xofs,
            0x01A => ppu.bg[2].yofs,
            0x01C => ppu.bg[3].xofs,
            0x01E => ppu.bg[3].yofs,

            // Window registers (write-only on real HW, needed for 8-bit RMW)
            0x040 => ppu.winh[0],
            0x042 => ppu.winh[1],
            0x044 => ppu.winv[0],
            0x046 => ppu.winv[1],
            0x048 => ppu.winin,
            0x04A => ppu.winout,
            0x04C => ppu.mosaic,

            // Blend/window (readable).
            0x050 => ppu.bldcnt,
            0x052 => ppu.bldalpha,

            // Serial (stubs).
            0x120 => 0, // SIODATA32 / SIOMULTI
            0x122 => 0,
            0x124 => 0,
            0x126 => 0,
            0x128 => 0, // SIOCNT — bit 7 clear = transfer not active
            0x12A => 0, // SIODATA8
            0x134 => 0, // RCNT
            0x140 => 0, // JOYCNT
            0x150 => 0, // JOY_RECV
            0x152 => 0,
            0x154 => 0, // JOY_TRANS
            0x156 => 0,
            0x158 => 0, // JOYSTAT

            // Key input
            0x130 => self.keyinput,
            0x132 => 0, // KEYCNT

            // Interrupt control
            0x200 => self.ie,
            0x202 => self.if_,
            0x204 => self.waitcnt,
            0x208 => self.ime as u16,
            0x300 => 1, // POSTFLG = 1 (post-boot)

            _ => 0,
        }
    }

    pub fn write16(&mut self, addr: u32, val: u16, ppu: &mut Ppu, _sched: &mut Scheduler) {
        match addr & 0x3FE {
            // PPU
            0x000 => ppu.dispcnt = val,
            0x004 => ppu.dispstat = (ppu.dispstat & 0x7) | (val & 0xFFF8),
            0x008 => ppu.bg[0].cnt = val,
            0x00A => ppu.bg[1].cnt = val,
            0x00C => ppu.bg[2].cnt = val,
            0x00E => ppu.bg[3].cnt = val,
            0x010 => ppu.bg[0].xofs = val & 0x1FF,
            0x012 => ppu.bg[0].yofs = val & 0x1FF,
            0x014 => ppu.bg[1].xofs = val & 0x1FF,
            0x016 => ppu.bg[1].yofs = val & 0x1FF,
            0x018 => ppu.bg[2].xofs = val & 0x1FF,
            0x01A => ppu.bg[2].yofs = val & 0x1FF,
            0x01C => ppu.bg[3].xofs = val & 0x1FF,
            0x01E => ppu.bg[3].yofs = val & 0x1FF,

            // Timers handled by bus.write16 → timers module.
            // Interrupt control
            0x200 => self.ie = val,
            0x202 => self.if_ &= !val, // write-1-to-clear
            0x204 => self.waitcnt = val,
            0x208 => self.ime = val & 1 != 0,

            // HALTCNT is at 0x301 (8-bit), but some games write 16-bit to 0x300.
            0x300 => {
                if val & 0x80 == 0 {
                    self.haltcnt = true;
                }
            }

            _ => {}
        }
    }

    pub fn write8(&mut self, addr: u32, val: u8, ppu: &mut Ppu, sched: &mut Scheduler) {
        // HALTCNT is the main 8-bit I/O write games use.
        if addr == 0x0400_0301 {
            if val & 0x80 == 0 {
                self.haltcnt = true;
            }
            return;
        }
        // For other registers, promote to 16-bit write (read-modify-write).
        let aligned = addr & !1;
        let old = self.read16(aligned, ppu);
        let new = if addr & 1 == 0 {
            (old & 0xFF00) | val as u16
        } else {
            (old & 0x00FF) | ((val as u16) << 8)
        };
        self.write16(aligned, new, ppu, sched);
    }

    /// Request VBlank IRQ.
    pub fn request_vblank_irq(&mut self) {
        self.if_ |= 1; // bit 0 = vblank
    }

    /// Request HBlank IRQ.
    pub fn request_hblank_irq(&mut self) {
        self.if_ |= 2; // bit 1 = hblank
    }

    /// Request VCount match IRQ.
    pub fn request_vcounter_irq(&mut self) {
        self.if_ |= 4; // bit 2 = vcounter
    }
}
