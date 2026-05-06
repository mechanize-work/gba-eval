//! DMA controller — 4 channels (0-3), priority 0 highest.
//!
//! Each channel has: source address, destination address, word count, control.
//! Transfers can be triggered immediately, at VBlank, at HBlank, or by special
//! (channel-dependent: sound FIFO for 1/2, video capture for 3).


#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DmaTiming {
    Immediate = 0,
    VBlank = 1,
    HBlank = 2,
    Special = 3,
}

#[derive(Clone, Copy)]
pub struct DmaChannel {
    pub src: u32,
    pub dst: u32,
    pub count: u32,
    pub control: u16,

    // Internal latched values (set on enable).
    pub internal_src: u32,
    pub internal_dst: u32,
    pub internal_count: u32,
}

impl Default for DmaChannel {
    fn default() -> Self {
        Self {
            src: 0, dst: 0, count: 0, control: 0,
            internal_src: 0, internal_dst: 0, internal_count: 0,
        }
    }
}

impl DmaChannel {
    pub fn enabled(&self) -> bool { self.control & 0x8000 != 0 }
    pub fn irq(&self) -> bool { self.control & 0x4000 != 0 }
    pub fn is_32bit(&self) -> bool { self.control & 0x0400 != 0 }
    pub fn repeat(&self) -> bool { self.control & 0x0200 != 0 }
    pub fn game_pak_drq(&self) -> bool { self.control & 0x0800 != 0 }

    pub fn timing(&self) -> DmaTiming {
        match (self.control >> 12) & 3 {
            0 => DmaTiming::Immediate,
            1 => DmaTiming::VBlank,
            2 => DmaTiming::HBlank,
            3 => DmaTiming::Special,
            _ => unreachable!(),
        }
    }

    pub fn dst_adj(&self) -> u32 {
        match (self.control >> 5) & 3 {
            0 => 0, // increment
            1 => 1, // decrement
            2 => 2, // fixed
            3 => 3, // increment/reload
            _ => unreachable!(),
        }
    }

    pub fn src_adj(&self) -> u32 {
        match (self.control >> 7) & 3 {
            0 => 0, // increment
            1 => 1, // decrement
            2 => 2, // fixed
            _ => 0, // prohibited, treat as increment
        }
    }
}

pub struct Dma {
    pub channels: [DmaChannel; 4],
}

impl Dma {
    pub fn new() -> Self {
        Self { channels: [DmaChannel::default(); 4] }
    }

    pub fn write_src_lo(&mut self, ch: usize, val: u16) {
        let cur = self.channels[ch].src;
        self.channels[ch].src = (cur & 0xFFFF0000) | val as u32;
    }

    pub fn write_src_hi(&mut self, ch: usize, val: u16) {
        let mask = if ch == 0 { 0x07FF } else { 0x0FFF };
        self.channels[ch].src = (self.channels[ch].src & 0xFFFF) | ((val & mask) as u32) << 16;
    }

    pub fn write_dst_lo(&mut self, ch: usize, val: u16) {
        let cur = self.channels[ch].dst;
        self.channels[ch].dst = (cur & 0xFFFF0000) | val as u32;
    }

    pub fn write_dst_hi(&mut self, ch: usize, val: u16) {
        let mask = if ch == 3 { 0x0FFF } else { 0x07FF };
        self.channels[ch].dst = (self.channels[ch].dst & 0xFFFF) | ((val & mask) as u32) << 16;
    }

    pub fn write_count(&mut self, ch: usize, val: u16) {
        self.channels[ch].count = val as u32;
    }

    pub fn write_control(&mut self, ch: usize, val: u16) {
        let was_enabled = self.channels[ch].enabled();
        self.channels[ch].control = val;

        // On 0→1 enable transition, latch source/dest/count.
        if !was_enabled && self.channels[ch].enabled() {
            self.channels[ch].internal_src = self.channels[ch].src;
            self.channels[ch].internal_dst = self.channels[ch].dst;
            let count = self.channels[ch].count;
            self.channels[ch].internal_count = if count == 0 {
                if ch == 3 { 0x10000 } else { 0x4000 }
            } else {
                count
            };

            // If immediate timing, mark for execution.
            if self.channels[ch].timing() == DmaTiming::Immediate {
                // Will be picked up by check_dma.
            }
        }
    }

}

/// Run a single DMA transfer for the given channel (legacy, used for reference).
#[allow(dead_code)]
pub fn run_dma_transfer(ch: usize, dma: &mut DmaChannel,
                         bus: &mut DmaBusAccess) -> u32 {
    if !dma.enabled() { return 0; }

    let word_size: u32 = if dma.is_32bit() { 4 } else { 2 };
    let src_step = match dma.src_adj() {
        0 => word_size as i32,
        1 => -(word_size as i32),
        2 => 0,
        _ => word_size as i32,
    };
    let dst_step = match dma.dst_adj() {
        0 | 3 => word_size as i32,
        1 => -(word_size as i32),
        2 => 0,
        _ => word_size as i32,
    };

    let count = dma.internal_count;
    let mut src = dma.internal_src;
    let mut dst = dma.internal_dst;

    for _ in 0..count {
        if dma.is_32bit() {
            let val = bus.read32(src);
            bus.write32(dst, val);
        } else {
            let val = bus.read16(src);
            bus.write16(dst, val);
        }
        src = (src as i32).wrapping_add(src_step) as u32;
        dst = (dst as i32).wrapping_add(dst_step) as u32;
    }

    dma.internal_src = src;
    dma.internal_dst = dst;

    // Reload destination if dst_adj == 3 (increment/reload).
    if dma.dst_adj() == 3 {
        dma.internal_dst = dma.dst;
    }

    if !dma.repeat() {
        dma.control &= !0x8000; // disable
    } else {
        // Reload count.
        let count = dma.count;
        dma.internal_count = if count == 0 {
            if ch == 3 { 0x10000 } else { 0x4000 }
        } else {
            count
        };
    }

    count * if dma.is_32bit() { 4 } else { 2 }
}

/// Trait-like struct for DMA bus access without borrowing Bus itself.
pub struct DmaBusAccess<'a> {
    pub ewram: &'a mut [u8],
    pub iwram: &'a mut [u8],
    pub palette: &'a mut [u8],
    pub vram: &'a mut [u8],
    pub oam: &'a mut [u8],
    pub rom: &'a [u8],
    pub sram: &'a mut [u8],
}

impl<'a> DmaBusAccess<'a> {
    pub fn read16(&self, addr: u32) -> u16 {
        let region = (addr >> 24) & 0xF;
        match region {
            0x2 => {
                let i = (addr as usize) & 0x3FFFE;
                u16::from_le_bytes([self.ewram[i], self.ewram[i + 1]])
            }
            0x3 => {
                let i = (addr as usize) & 0x7FFE;
                u16::from_le_bytes([self.iwram[i], self.iwram[i + 1]])
            }
            0x5 => {
                let i = (addr as usize) & 0x3FE;
                u16::from_le_bytes([self.palette[i], self.palette[i + 1]])
            }
            0x6 => {
                let off = (addr as usize) & 0x1FFFF;
                let i = if off >= 0x18000 { off - 0x8000 } else { off };
                let i = i & !1;
                if i + 1 < self.vram.len() {
                    u16::from_le_bytes([self.vram[i], self.vram[i + 1]])
                } else { 0 }
            }
            0x7 => {
                let i = (addr as usize) & 0x3FE;
                u16::from_le_bytes([self.oam[i], self.oam[i + 1]])
            }
            0x8..=0xD => {
                let off = (addr as usize) & 0x01FF_FFFF;
                let i = off & !1;
                if i + 1 < self.rom.len() {
                    u16::from_le_bytes([self.rom[i], self.rom[i + 1]])
                } else { 0 }
            }
            _ => 0,
        }
    }

    pub fn read32(&self, addr: u32) -> u32 {
        let lo = self.read16(addr) as u32;
        let hi = self.read16(addr + 2) as u32;
        lo | (hi << 16)
    }

    pub fn write16(&mut self, addr: u32, val: u16) {
        let region = (addr >> 24) & 0xF;
        match region {
            0x2 => {
                let i = (addr as usize) & 0x3FFFE;
                self.ewram[i..i + 2].copy_from_slice(&val.to_le_bytes());
            }
            0x3 => {
                let i = (addr as usize) & 0x7FFE;
                self.iwram[i..i + 2].copy_from_slice(&val.to_le_bytes());
            }
            0x5 => {
                let i = (addr as usize) & 0x3FE;
                self.palette[i..i + 2].copy_from_slice(&val.to_le_bytes());
            }
            0x6 => {
                let off = (addr as usize) & 0x1FFFF;
                let i = if off >= 0x18000 { off - 0x8000 } else { off };
                let i = i & !1;
                if i + 1 < self.vram.len() {
                    self.vram[i..i + 2].copy_from_slice(&val.to_le_bytes());
                }
            }
            0x7 => {
                let i = (addr as usize) & 0x3FE;
                self.oam[i..i + 2].copy_from_slice(&val.to_le_bytes());
            }
            _ => {}
        }
    }

    pub fn write32(&mut self, addr: u32, val: u32) {
        self.write16(addr, val as u16);
        self.write16(addr + 2, (val >> 16) as u16);
    }
}
