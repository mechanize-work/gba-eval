//! PPU — renders 240x160 @ 15-bit color, 4 BG layers + OBJ layer.
//!
//! Timing (in CPU cycles, 1 dot = 4 cycles):
//!   Visible:    240 dots = 960 cycles   (HDraw)
//!   HBlank:      68 dots = 272 cycles
//!   Total/line: 308 dots = 1232 cycles
//!   Visible lines: 160 (VDraw)
//!   VBlank lines:   68 (lines 160-227)
//!   Total lines:   228

use crate::io::IoRegs;
use crate::scheduler::{Event, Scheduler};

const SCREEN_W: usize = 240;
const SCREEN_H: usize = 160;
pub const FB_SIZE: usize = SCREEN_W * SCREEN_H;
const HDRAW_CYCLES: u64 = 960;
const HBLANK_CYCLES: u64 = 272;

/// Transparent pixel sentinel.
pub struct BgRegs {
    pub cnt: u16,
    pub xofs: u16,
    pub yofs: u16,
    // Affine parameters (BG2/BG3 only).
    pub pa: i16,
    pub pb: i16,
    pub pc: i16,
    pub pd: i16,
    pub ref_x: i32,  // 28-bit fixed-point reference X.
    pub ref_y: i32,
    pub internal_x: i32, // latched at VBlank, incremented per scanline.
    pub internal_y: i32,
}

impl BgRegs {
    fn new() -> Self {
        Self {
            cnt: 0, xofs: 0, yofs: 0,
            pa: 0x100, pb: 0, pc: 0, pd: 0x100,
            ref_x: 0, ref_y: 0,
            internal_x: 0, internal_y: 0,
        }
    }

    fn priority(&self) -> u16 { self.cnt & 3 }
    fn tile_base(&self) -> usize { ((self.cnt >> 2) & 3) as usize * 0x4000 }
    fn mosaic(&self) -> bool { self.cnt & 0x40 != 0 }
    fn is_8bpp(&self) -> bool { self.cnt & 0x80 != 0 }
    fn map_base(&self) -> usize { ((self.cnt >> 8) & 0x1F) as usize * 0x800 }
    fn wrap(&self) -> bool { self.cnt & (1 << 13) != 0 }
    fn screen_size(&self) -> u16 { (self.cnt >> 14) & 3 }
}

pub struct Ppu {
    pub dispcnt: u16,
    pub dispstat: u16,
    pub vcount: u16,
    pub bg: [BgRegs; 4],

    // Blending / effects.
    pub bldcnt: u16,
    pub bldalpha: u16,
    pub bldy: u16,

    // Windows.
    pub winh: [u16; 2],  // WIN0H, WIN1H
    pub winv: [u16; 2],  // WIN0V, WIN1V
    pub winin: u16,
    pub winout: u16,

    // Mosaic.
    pub mosaic: u16,

    pub palette: Box<[u8; 1024]>,
    pub vram: Box<[u8; 96 * 1024]>,
    pub oam: Box<[u8; 1024]>,

    /// 32-bit RGBA framebuffer for the frontend to consume.
    pub framebuffer: Box<[u32; FB_SIZE]>,

    pub frame_ready: bool,

    // Scanline compositing buffers.
    line_bg: [[u16; SCREEN_W]; 4],   // raw palette indices per BG
    line_obj: [u16; SCREEN_W],       // OBJ palette indices
    line_obj_prio: [u8; SCREEN_W],   // OBJ priority per pixel
    line_obj_mode: [u8; SCREEN_W],   // OBJ mode (0=normal, 1=semi-transparent, 2=window)
}

impl Ppu {
    pub fn new() -> Self {
        Self {
            dispcnt: 0x0080, // forced blank at boot (matches real GBA & Mesen)
            dispstat: 0,
            vcount: 0,
            bg: [BgRegs::new(), BgRegs::new(), BgRegs::new(), BgRegs::new()],
            bldcnt: 0,
            bldalpha: 0,
            bldy: 0,
            winh: [0; 2],
            winv: [0; 2],
            winin: 0,
            winout: 0,
            mosaic: 0,
            palette: Box::new([0; 1024]),
            vram: Box::new([0; 96 * 1024]),
            oam: Box::new([0; 1024]),
            framebuffer: Box::new([0; FB_SIZE]),
            frame_ready: false,
            line_bg: [[0; SCREEN_W]; 4],
            line_obj: [0; SCREEN_W],
            line_obj_prio: [3; SCREEN_W],
            line_obj_mode: [0; SCREEN_W],
        }
    }

    // --- Memory access helpers ---

    pub fn read_palette32(&self, addr: u32) -> u32 {
        let i = (addr as usize) & 0x3FC;
        u32::from_le_bytes(self.palette[i..i + 4].try_into().unwrap())
    }

    pub fn read_vram32(&self, addr: u32) -> u32 {
        let i = Self::mirror_vram(addr) & !3;
        u32::from_le_bytes(self.vram[i..i + 4].try_into().unwrap())
    }

    pub fn read_oam32(&self, addr: u32) -> u32 {
        let i = (addr as usize) & 0x3FC;
        u32::from_le_bytes(self.oam[i..i + 4].try_into().unwrap())
    }

    pub fn write_palette16(&mut self, addr: u32, val: u16) {
        let i = (addr as usize) & 0x3FE;
        self.palette[i..i + 2].copy_from_slice(&val.to_le_bytes());
    }

    pub fn write_vram16(&mut self, addr: u32, val: u16) {
        let i = Self::mirror_vram(addr) & !1;
        self.vram[i..i + 2].copy_from_slice(&val.to_le_bytes());
    }

    pub fn write_vram8(&mut self, addr: u32, val: u8) {
        let i = Self::mirror_vram(addr);
        let bg_limit = if self.dispcnt & 7 >= 3 { 0x14000 } else { 0x10000 };
        if i < bg_limit {
            let i = i & !1;
            self.vram[i] = val;
            self.vram[i + 1] = val;
        }
    }

    pub fn write_oam16(&mut self, addr: u32, val: u16) {
        let i = (addr as usize) & 0x3FE;
        self.oam[i..i + 2].copy_from_slice(&val.to_le_bytes());
    }

    fn mirror_vram(addr: u32) -> usize {
        let off = (addr as usize) & 0x1FFFF;
        if off >= 0x18000 { off - 0x8000 } else { off }
    }

    /// Write affine BG parameters (0x020-0x03F).
    pub fn write_affine_param(&mut self, reg: u16, val: u16) {
        match reg {
            0x020 => self.bg[2].pa = val as i16,
            0x022 => self.bg[2].pb = val as i16,
            0x024 => self.bg[2].pc = val as i16,
            0x026 => self.bg[2].pd = val as i16,
            0x028 => {
                self.bg[2].ref_x = (self.bg[2].ref_x & 0xFFFF0000u32 as i32) | val as i32;
                self.bg[2].ref_x = (self.bg[2].ref_x << 4) >> 4; // sign-extend 28-bit
                self.bg[2].internal_x = self.bg[2].ref_x;
            }
            0x02A => {
                self.bg[2].ref_x = (self.bg[2].ref_x & 0xFFFF) | ((val as i32) << 16);
                self.bg[2].ref_x = (self.bg[2].ref_x << 4) >> 4;
                self.bg[2].internal_x = self.bg[2].ref_x;
            }
            0x02C => {
                self.bg[2].ref_y = (self.bg[2].ref_y & 0xFFFF0000u32 as i32) | val as i32;
                self.bg[2].ref_y = (self.bg[2].ref_y << 4) >> 4;
                self.bg[2].internal_y = self.bg[2].ref_y;
            }
            0x02E => {
                self.bg[2].ref_y = (self.bg[2].ref_y & 0xFFFF) | ((val as i32) << 16);
                self.bg[2].ref_y = (self.bg[2].ref_y << 4) >> 4;
                self.bg[2].internal_y = self.bg[2].ref_y;
            }
            0x030 => self.bg[3].pa = val as i16,
            0x032 => self.bg[3].pb = val as i16,
            0x034 => self.bg[3].pc = val as i16,
            0x036 => self.bg[3].pd = val as i16,
            0x038 => {
                self.bg[3].ref_x = (self.bg[3].ref_x & 0xFFFF0000u32 as i32) | val as i32;
                self.bg[3].ref_x = (self.bg[3].ref_x << 4) >> 4;
                self.bg[3].internal_x = self.bg[3].ref_x;
            }
            0x03A => {
                self.bg[3].ref_x = (self.bg[3].ref_x & 0xFFFF) | ((val as i32) << 16);
                self.bg[3].ref_x = (self.bg[3].ref_x << 4) >> 4;
                self.bg[3].internal_x = self.bg[3].ref_x;
            }
            0x03C => {
                self.bg[3].ref_y = (self.bg[3].ref_y & 0xFFFF0000u32 as i32) | val as i32;
                self.bg[3].ref_y = (self.bg[3].ref_y << 4) >> 4;
                self.bg[3].internal_y = self.bg[3].ref_y;
            }
            0x03E => {
                self.bg[3].ref_y = (self.bg[3].ref_y & 0xFFFF) | ((val as i32) << 16);
                self.bg[3].ref_y = (self.bg[3].ref_y << 4) >> 4;
                self.bg[3].internal_y = self.bg[3].ref_y;
            }
            _ => {}
        }
    }

    /// Write blending/window effect registers (0x040-0x054).
    pub fn write_effect_reg(&mut self, reg: u16, val: u16) {
        match reg {
            0x040 => self.winh[0] = val,
            0x042 => self.winh[1] = val,
            0x044 => self.winv[0] = val,
            0x046 => self.winv[1] = val,
            0x048 => self.winin = val,
            0x04A => self.winout = val,
            0x04C => self.mosaic = val,
            0x050 => self.bldcnt = val,
            0x052 => self.bldalpha = val,
            0x054 => self.bldy = val,
            _ => {}
        }
    }

    // --- Timing events ---

    pub fn on_hblank(&mut self, io: &mut IoRegs, sched: &mut Scheduler) {
        let line = self.vcount;

        if line < SCREEN_H as u16 {
            self.render_scanline(line);

            // Advance affine reference points.
            for i in 2..4 {
                self.bg[i].internal_x += self.bg[i].pb as i32;
                self.bg[i].internal_y += self.bg[i].pd as i32;
            }
        }

        self.dispstat |= 0x2;
        if self.dispstat & 0x10 != 0 {
            io.request_hblank_irq();
        }
        sched.push(Event::HDraw, HBLANK_CYCLES);
    }

    pub fn on_hdraw(&mut self, io: &mut IoRegs, sched: &mut Scheduler) {
        self.dispstat &= !0x2;
        self.vcount = (self.vcount + 1) % 228;

        // VCount match.
        let lyc = (self.dispstat >> 8) as u16;
        if self.vcount == lyc {
            self.dispstat |= 0x4;
            if self.dispstat & 0x20 != 0 {
                io.request_vcounter_irq();
            }
        } else {
            self.dispstat &= !0x4;
        }

        if self.vcount == 160 {
            self.dispstat |= 0x1;
            self.frame_ready = true;
            if self.dispstat & 0x8 != 0 {
                io.request_vblank_irq();
            }
            // Latch affine reference points at VBlank.
            for i in 2..4 {
                self.bg[i].internal_x = self.bg[i].ref_x;
                self.bg[i].internal_y = self.bg[i].ref_y;
            }
        } else if self.vcount == 0 {
            self.dispstat &= !0x1;
        }

        sched.push(Event::HBlank, HDRAW_CYCLES);
    }

    // --- Rendering ---

    fn render_scanline(&mut self, line: u16) {
        let base = line as usize * SCREEN_W;

        // Forced blank: white screen.
        if self.dispcnt & 0x80 != 0 {
            for x in 0..SCREEN_W {
                self.framebuffer[base + x] = 0xFFFFFFFF; // white
            }
            return;
        }

        let mode = self.dispcnt & 7;

        // Clear line buffers.
        self.line_obj.fill(0);
        self.line_obj_prio.fill(3);
        self.line_obj_mode.fill(0);
        for bg in &mut self.line_bg { bg.fill(0); }

        // Render sprites first (they write into line_obj).
        if self.dispcnt & (1 << 12) != 0 {
            self.render_sprites(line);
        }

        match mode {
            0 => self.compose_mode0(line, base),
            1 => self.compose_mode1(line, base),
            2 => self.compose_mode2(line, base),
            3 => self.compose_mode3(line, base),
            4 => self.compose_mode4(line, base),
            5 => self.compose_mode5(line, base),
            _ => {
                let bg = self.backdrop_color();
                for x in 0..SCREEN_W { self.framebuffer[base + x] = bg; }
            }
        }

        // Apply mosaic to BG line buffers (before composition for tiled modes is too late,
        // but this handles the framebuffer for bitmap modes).
    }

    /// Apply mosaic to a BG line buffer.
    fn apply_bg_mosaic(&mut self, bgi: usize, _line: u16) {
        let bg_h = (self.mosaic & 0xF) as u16 + 1;
        let bg_v = ((self.mosaic >> 4) & 0xF) as u16 + 1;
        if bg_h <= 1 && bg_v <= 1 { return; }
        if !self.bg[bgi].mosaic() { return; }

        // Vertical mosaic: use the mosaic-row's buffer.
        // This is simplified — the correct approach needs to re-render the mosaic source line.
        // For now, just apply horizontal mosaic.
        if bg_h > 1 {
            let mut x = 0usize;
            while x < SCREEN_W {
                let val = self.line_bg[bgi][x];
                let end = (x + bg_h as usize).min(SCREEN_W);
                for px in x..end {
                    self.line_bg[bgi][px] = val;
                }
                x = end;
            }
        }
    }

    // ================================================================
    // Mode compositors
    // ================================================================

    fn compose_mode0(&mut self, line: u16, base: usize) {
        for i in 0..4 {
            if self.dispcnt & (1 << (8 + i)) != 0 {
                self.render_text_bg(i, line);
                self.apply_bg_mosaic(i, line);
            }
        }
        self.compose_layers(base);
    }

    fn compose_mode1(&mut self, line: u16, base: usize) {
        if self.dispcnt & (1 << 8) != 0 { self.render_text_bg(0, line); self.apply_bg_mosaic(0, line); }
        if self.dispcnt & (1 << 9) != 0 { self.render_text_bg(1, line); self.apply_bg_mosaic(1, line); }
        if self.dispcnt & (1 << 10) != 0 { self.render_affine_bg(2, line); self.apply_bg_mosaic(2, line); }
        self.compose_layers(base);
    }

    fn compose_mode2(&mut self, line: u16, base: usize) {
        if self.dispcnt & (1 << 10) != 0 { self.render_affine_bg(2, line); self.apply_bg_mosaic(2, line); }
        if self.dispcnt & (1 << 11) != 0 { self.render_affine_bg(3, line); self.apply_bg_mosaic(3, line); }
        self.compose_layers(base);
    }

    fn compose_mode3(&mut self, _line: u16, base: usize) {
        // Mode 3 is a 240×160 RGB555 bitmap sampled through BG2's affine
        // transform — not a 1:1 blit. Without the transform pass, games
        // that scale/scroll the bitmap (spout, many demos) render to only
        // the unscaled top-left region while the rest shows backdrop.
        let pa = self.bg[2].pa as i32;
        let pc = self.bg[2].pc as i32;
        let mut ref_x = self.bg[2].internal_x;
        let mut ref_y = self.bg[2].internal_y;
        for x in 0..SCREEN_W {
            let tex_x = ref_x >> 8;
            let tex_y = ref_y >> 8;
            ref_x += pa;
            ref_y += pc;
            if tex_x < 0 || tex_x >= 240 || tex_y < 0 || tex_y >= 160 {
                self.framebuffer[base + x] = self.backdrop_color();
                continue;
            }
            let addr = ((tex_y as usize) * 240 + (tex_x as usize)) * 2;
            let rgb555 = u16::from_le_bytes([self.vram[addr], self.vram[addr + 1]]);
            self.framebuffer[base + x] = Self::rgb555_to_rgba(rgb555);
        }
    }

    fn compose_mode4(&mut self, _line: u16, base: usize) {
        // Mode 4: 240×160 8bpp paletted, sampled through BG2 affine.
        let page = if self.dispcnt & 0x10 != 0 { 0xA000 } else { 0 };
        let pa = self.bg[2].pa as i32;
        let pc = self.bg[2].pc as i32;
        let mut ref_x = self.bg[2].internal_x;
        let mut ref_y = self.bg[2].internal_y;
        for x in 0..SCREEN_W {
            let tex_x = ref_x >> 8;
            let tex_y = ref_y >> 8;
            ref_x += pa;
            ref_y += pc;
            if tex_x < 0 || tex_x >= 240 || tex_y < 0 || tex_y >= 160 {
                self.framebuffer[base + x] = self.backdrop_color();
                continue;
            }
            let idx = self.vram[page + (tex_y as usize) * 240 + (tex_x as usize)] as usize;
            if idx == 0 {
                self.framebuffer[base + x] = self.backdrop_color();
            } else {
                self.framebuffer[base + x] = self.palette_color(idx);
            }
        }
        // Overlay sprites.
        if self.dispcnt & (1 << 12) != 0 {
            for x in 0..SCREEN_W {
                if self.line_obj[x] != 0 {
                    self.framebuffer[base + x] = self.palette_color(256 + self.line_obj[x] as usize);
                }
            }
        }
    }

    fn compose_mode5(&mut self, _line: u16, base: usize) {
        // Mode 5: 160×128 RGB555 bitmap, sampled through BG2 affine.
        // Spout uses this to draw a low-res bitmap that BG2 scales to the
        // full 240×160 viewport. Without the transform, only the top-left
        // 160×128 region renders (the "1/4 screen" bug).
        let page = if self.dispcnt & 0x10 != 0 { 0xA000 } else { 0 };
        let pa = self.bg[2].pa as i32;
        let pc = self.bg[2].pc as i32;
        let mut ref_x = self.bg[2].internal_x;
        let mut ref_y = self.bg[2].internal_y;
        for x in 0..SCREEN_W {
            let tex_x = ref_x >> 8;
            let tex_y = ref_y >> 8;
            ref_x += pa;
            ref_y += pc;
            if tex_x < 0 || tex_x >= 160 || tex_y < 0 || tex_y >= 128 {
                self.framebuffer[base + x] = self.backdrop_color();
                continue;
            }
            let addr = page + ((tex_y as usize) * 160 + (tex_x as usize)) * 2;
            let rgb555 = u16::from_le_bytes([self.vram[addr], self.vram[addr + 1]]);
            self.framebuffer[base + x] = Self::rgb555_to_rgba(rgb555);
        }
    }

    // ================================================================
    // Layer composition with priority, blending, and windowing
    // ================================================================

    const LAYER_OBJ: u8 = 4;
    const LAYER_BD: u8 = 5;

    fn compose_layers(&mut self, base: usize) {
        let backdrop = self.backdrop_color();
        let win0_enabled = self.dispcnt & (1 << 13) != 0;
        let win1_enabled = self.dispcnt & (1 << 14) != 0;
        let objwin_enabled = self.dispcnt & (1 << 15) != 0;
        let any_window = win0_enabled || win1_enabled || objwin_enabled;

        let blend_mode = (self.bldcnt >> 6) & 3;
        let eva = (self.bldalpha & 0x1F).min(16) as u32;
        let evb = ((self.bldalpha >> 8) & 0x1F).min(16) as u32;
        let evy = (self.bldy & 0x1F).min(16) as u32;

        let line = self.vcount;
        let win0_y_in = Self::in_win_v(self.winv[0], line);
        let win1_y_in = Self::in_win_v(self.winv[1], line);

        for x in 0..SCREEN_W {
            let win_flags = if any_window {
                self.get_window_flags(x as u16, win0_enabled, win1_enabled,
                                       objwin_enabled, win0_y_in, win1_y_in)
            } else {
                0x3F
            };
            let blend_enabled = win_flags & 0x20 != 0;

            // Collect sorted layers: (color, layer_id). Sorted by priority (0=top).
            // On GBA: OBJ and BG can share a priority level; OBJ wins ties.
            // We need the top 2 opaque pixels for blending.
            let mut top_color = backdrop;
            let mut top_layer: u8 = Self::LAYER_BD;
            let mut bot_color = backdrop;
            let mut bot_layer: u8 = Self::LAYER_BD;
            let mut obj_semi = false;
            let mut found_top = false;

            // Priority 0 = highest. We scan 0→3 and stop after finding top 2.
            'prio_loop: for prio in 0..4u16 {
                // OBJ at this priority.
                if !found_top || bot_layer == Self::LAYER_BD {
                    if self.line_obj[x] != 0 && self.line_obj_prio[x] as u16 == prio
                        && (win_flags & (1 << 4) != 0)
                    {
                        let c = self.palette_color(256 + self.line_obj[x] as usize);
                        if !found_top {
                            top_color = c;
                            top_layer = Self::LAYER_OBJ;
                            obj_semi = self.line_obj_mode[x] == 1;
                            found_top = true;
                        } else {
                            bot_color = c;
                            bot_layer = Self::LAYER_OBJ;
                            break 'prio_loop;
                        }
                    }
                }

                // BG layers at this priority (BG0 highest within same priority).
                for bgi in 0..4usize {
                    if self.dispcnt & (1 << (8 + bgi)) == 0 { continue; }
                    if self.bg[bgi].priority() != prio { continue; }
                    if win_flags & (1 << bgi) == 0 { continue; }
                    let idx = self.line_bg[bgi][x] as usize;
                    if idx == 0 { continue; }
                    let c = self.palette_color(idx);
                    if !found_top {
                        top_color = c;
                        top_layer = bgi as u8;
                        found_top = true;
                    } else {
                        bot_color = c;
                        bot_layer = bgi as u8;
                        break 'prio_loop;
                    }
                }
            }

            // Apply blending.
            let final_color = if blend_enabled {
                let is_first = self.bldcnt & (1 << top_layer) != 0;
                let is_second = self.bldcnt & (1 << (8 + bot_layer)) != 0;

                if obj_semi && is_second {
                    Self::alpha_blend(top_color, bot_color, eva, evb)
                } else {
                    match blend_mode {
                        1 if is_first && is_second => {
                            Self::alpha_blend(top_color, bot_color, eva, evb)
                        }
                        2 if is_first => Self::brightness_increase(top_color, evy),
                        3 if is_first => Self::brightness_decrease(top_color, evy),
                        _ => top_color,
                    }
                }
            } else {
                top_color
            };

            self.framebuffer[base + x] = final_color;
        }
    }

    fn get_window_flags(&self, x: u16, win0: bool, win1: bool,
                         objwin: bool, win0_y: bool, win1_y: bool) -> u8 {
        if win0 && win0_y && Self::in_win_h(self.winh[0], x) {
            return (self.winin & 0x3F) as u8;
        }
        if win1 && win1_y && Self::in_win_h(self.winh[1], x) {
            return ((self.winin >> 8) & 0x3F) as u8;
        }
        if objwin && self.line_obj_mode[x as usize] == 2 {
            return ((self.winout >> 8) & 0x3F) as u8;
        }
        // Outside all windows.
        (self.winout & 0x3F) as u8
    }

    fn in_win_h(winh: u16, x: u16) -> bool {
        let left = (winh >> 8) as u16;
        let right = (winh & 0xFF) as u16;
        if left <= right {
            x >= left && x < right
        } else {
            x >= left || x < right
        }
    }

    fn in_win_v(winv: u16, y: u16) -> bool {
        let top = (winv >> 8) as u16;
        let bottom = (winv & 0xFF) as u16;
        if top <= bottom {
            y >= top && y < bottom
        } else {
            y >= top || y < bottom
        }
    }

    /// Extract 5-bit color channels from 0xFF_BB_GG_RR format.
    #[inline]
    fn to_rgb5(c: u32) -> (u32, u32, u32) {
        ((c >> 3) & 0x1F, (c >> 11) & 0x1F, (c >> 19) & 0x1F)
    }

    /// Convert 5-bit RGB back to 0xFF_BB_GG_RR.
    #[inline]
    fn from_rgb5(r: u32, g: u32, b: u32) -> u32 {
        let r8 = (r << 3) | (r >> 2);
        let g8 = (g << 3) | (g >> 2);
        let b8 = (b << 3) | (b >> 2);
        0xFF000000 | (b8 << 16) | (g8 << 8) | r8
    }

    fn alpha_blend(a: u32, b: u32, eva: u32, evb: u32) -> u32 {
        let (r1, g1, b1) = Self::to_rgb5(a);
        let (r2, g2, b2) = Self::to_rgb5(b);
        let r = ((r1 * eva + r2 * evb) / 16).min(31);
        let g = ((g1 * eva + g2 * evb) / 16).min(31);
        let b = ((b1 * eva + b2 * evb) / 16).min(31);
        Self::from_rgb5(r, g, b)
    }

    fn brightness_increase(c: u32, evy: u32) -> u32 {
        let (r, g, b) = Self::to_rgb5(c);
        let r = r + ((31 - r) * evy / 16);
        let g = g + ((31 - g) * evy / 16);
        let b = b + ((31 - b) * evy / 16);
        Self::from_rgb5(r.min(31), g.min(31), b.min(31))
    }

    fn brightness_decrease(c: u32, evy: u32) -> u32 {
        let (r, g, b) = Self::to_rgb5(c);
        let r = r.saturating_sub(r * evy / 16);
        let g = g.saturating_sub(g * evy / 16);
        let b = b.saturating_sub(b * evy / 16);
        Self::from_rgb5(r, g, b)
    }

    // ================================================================
    // Text BG rendering
    // ================================================================

    fn render_text_bg(&mut self, bgi: usize, line: u16) {
        let tile_base = self.bg[bgi].tile_base();
        let map_base = self.bg[bgi].map_base();
        let is_8bpp = self.bg[bgi].is_8bpp();
        let screen_size = self.bg[bgi].screen_size();
        let (map_w, map_h) = match screen_size {
            0 => (256u32, 256u32), 1 => (512, 256), 2 => (256, 512), 3 => (512, 512),
            _ => unreachable!(),
        };

        let scroll_x = self.bg[bgi].xofs as u32;
        let scroll_y = self.bg[bgi].yofs as u32;
        let y = (line as u32 + scroll_y) % map_h;
        let tile_row = y / 8;
        let fine_y = y % 8;

        for px in 0..SCREEN_W as u32 {
            let x = (px + scroll_x) % map_w;
            let tile_col = x / 8;
            let fine_x = x % 8;

            let screen_block = match screen_size {
                0 => 0,
                1 => tile_col / 32,
                2 => tile_row / 32,
                3 => (tile_col / 32) + (tile_row / 32) * 2,
                _ => 0,
            };
            let local_col = tile_col % 32;
            let local_row = tile_row % 32;

            let map_addr = map_base + screen_block as usize * 0x800
                + (local_row as usize * 32 + local_col as usize) * 2;
            if map_addr + 1 >= self.vram.len() { continue; }
            let entry = u16::from_le_bytes([self.vram[map_addr], self.vram[map_addr + 1]]);

            let tile_num = (entry & 0x3FF) as usize;
            let hflip = entry & 0x400 != 0;
            let vflip = entry & 0x800 != 0;
            let pal = ((entry >> 12) & 0xF) as usize;

            let fy = if vflip { 7 - fine_y } else { fine_y };
            let fx = if hflip { 7 - fine_x } else { fine_x };

            let color_idx = if is_8bpp {
                let addr = tile_base + tile_num * 64 + fy as usize * 8 + fx as usize;
                if addr >= self.vram.len() { continue; }
                self.vram[addr] as u16
            } else {
                let addr = tile_base + tile_num * 32 + fy as usize * 4 + (fx as usize / 2);
                if addr >= self.vram.len() { continue; }
                let byte = self.vram[addr];
                let nibble = if fx & 1 == 0 { byte & 0xF } else { byte >> 4 };
                if nibble == 0 { continue; }
                (pal * 16 + nibble as usize) as u16
            };

            if color_idx == 0 { continue; }
            self.line_bg[bgi][px as usize] = color_idx;
        }
    }

    // ================================================================
    // Affine BG rendering
    // ================================================================

    fn render_affine_bg(&mut self, bgi: usize, _line: u16) {
        let tile_base = self.bg[bgi].tile_base();
        let map_base = self.bg[bgi].map_base();
        let size_bits = self.bg[bgi].screen_size();
        let size = 128 << size_bits; // 128, 256, 512, 1024
        let wrap = self.bg[bgi].wrap();

        let pa = self.bg[bgi].pa as i32;
        let pc = self.bg[bgi].pc as i32;

        let mut ref_x = self.bg[bgi].internal_x;
        let mut ref_y = self.bg[bgi].internal_y;

        for px in 0..SCREEN_W {
            let tex_x = ref_x >> 8;
            let tex_y = ref_y >> 8;

            ref_x += pa;
            ref_y += pc;

            let (tx, ty) = if wrap {
                (((tex_x % size) + size) % size, ((tex_y % size) + size) % size)
            } else {
                if tex_x < 0 || tex_y < 0 || tex_x >= size || tex_y >= size { continue; }
                (tex_x, tex_y)
            };

            let tile_col = tx / 8;
            let tile_row = ty / 8;
            let fine_x = (tx % 8) as usize;
            let fine_y = (ty % 8) as usize;
            let map_w_tiles = size / 8;

            let map_addr = map_base + (tile_row * map_w_tiles + tile_col) as usize;
            if map_addr >= self.vram.len() { continue; }
            let tile_num = self.vram[map_addr] as usize;

            let pixel_addr = tile_base + tile_num * 64 + fine_y * 8 + fine_x;
            if pixel_addr >= self.vram.len() { continue; }
            let idx = self.vram[pixel_addr] as u16;
            if idx == 0 { continue; }
            self.line_bg[bgi][px] = idx;
        }
    }

    // ================================================================
    // Sprite (OBJ) rendering
    // ================================================================

    fn render_sprites(&mut self, line: u16) {
        let mapping_1d = self.dispcnt & (1 << 6) != 0;

        // Parse all 128 OAM entries, render in reverse order (lower index = higher priority).
        for i in (0..128).rev() {
            let attr0 = u16::from_le_bytes([self.oam[i * 8], self.oam[i * 8 + 1]]);
            let attr1 = u16::from_le_bytes([self.oam[i * 8 + 2], self.oam[i * 8 + 3]]);
            let attr2 = u16::from_le_bytes([self.oam[i * 8 + 4], self.oam[i * 8 + 5]]);

            let obj_mode = (attr0 >> 8) & 3;
            if obj_mode == 2 { continue; } // hidden

            let is_affine = attr0 & 0x100 != 0;
            let double_size = is_affine && (attr0 & 0x200 != 0);
            let gfx_mode = (attr0 >> 10) & 3;
            let _mosaic = attr0 & (1 << 12) != 0;
            let is_8bpp = attr0 & (1 << 13) != 0;
            let shape = (attr0 >> 14) & 3;

            let hflip = !is_affine && (attr1 & (1 << 12) != 0);
            let vflip = !is_affine && (attr1 & (1 << 13) != 0);
            let size_idx = (attr1 >> 14) & 3;
            let affine_idx = if is_affine { ((attr1 >> 9) & 0x1F) as usize } else { 0 };

            let priority = ((attr2 >> 10) & 3) as u8;
            let pal_bank = ((attr2 >> 12) & 0xF) as usize;
            let tile_num = (attr2 & 0x3FF) as usize;

            // Sprite dimensions.
            let (w, h) = match (shape, size_idx) {
                (0, 0) => (8, 8),    (0, 1) => (16, 16),  (0, 2) => (32, 32),  (0, 3) => (64, 64),
                (1, 0) => (16, 8),   (1, 1) => (32, 8),   (1, 2) => (32, 16),  (1, 3) => (64, 32),
                (2, 0) => (8, 16),   (2, 1) => (8, 32),   (2, 2) => (16, 32),  (2, 3) => (32, 64),
                _ => continue,
            };

            let (render_w, render_h) = if double_size { (w * 2, h * 2) } else { (w, h) };

            let mut obj_y = (attr0 & 0xFF) as i32;
            if obj_y >= 160 { obj_y -= 256; }
            let mut obj_x = (attr1 & 0x1FF) as i32;
            if obj_x >= 240 { obj_x -= 512; }

            // Check if this sprite is on the current scanline.
            let local_y = line as i32 - obj_y;
            if local_y < 0 || local_y >= render_h as i32 { continue; }

            if is_affine {
                // Read affine matrix from OAM.
                let pa = i16::from_le_bytes([self.oam[affine_idx * 32 + 6], self.oam[affine_idx * 32 + 7]]);
                let pb = i16::from_le_bytes([self.oam[affine_idx * 32 + 14], self.oam[affine_idx * 32 + 15]]);
                let pc = i16::from_le_bytes([self.oam[affine_idx * 32 + 22], self.oam[affine_idx * 32 + 23]]);
                let pd = i16::from_le_bytes([self.oam[affine_idx * 32 + 30], self.oam[affine_idx * 32 + 31]]);

                let half_w = render_w as i32 / 2;
                let half_h = render_h as i32 / 2;
                let iy = local_y - half_h;

                for ix_screen in 0..render_w as i32 {
                    let screen_x = obj_x + ix_screen;
                    if screen_x < 0 || screen_x >= SCREEN_W as i32 { continue; }

                    let ix = ix_screen - half_w;
                    let tex_x = ((pa as i32 * ix + pb as i32 * iy) >> 8) + (w as i32 / 2);
                    let tex_y = ((pc as i32 * ix + pd as i32 * iy) >> 8) + (h as i32 / 2);

                    if tex_x < 0 || tex_x >= w as i32 || tex_y < 0 || tex_y >= h as i32 { continue; }

                    let idx = self.get_sprite_pixel(tile_num, tex_x as u32, tex_y as u32,
                                                     w, is_8bpp, pal_bank, mapping_1d);
                    if idx != 0 {
                        let sx = screen_x as usize;
                        self.line_obj[sx] = idx;
                        self.line_obj_prio[sx] = priority;
                        self.line_obj_mode[sx] = gfx_mode as u8;
                    }
                }
            } else {
                // Non-affine sprite.
                let ty = if vflip { h as i32 - 1 - local_y } else { local_y };

                for lx in 0..w as i32 {
                    let screen_x = obj_x + lx;
                    if screen_x < 0 || screen_x >= SCREEN_W as i32 { continue; }

                    let tx = if hflip { w as i32 - 1 - lx } else { lx };
                    let idx = self.get_sprite_pixel(tile_num, tx as u32, ty as u32,
                                                     w, is_8bpp, pal_bank, mapping_1d);
                    if idx != 0 {
                        let sx = screen_x as usize;
                        self.line_obj[sx] = idx;
                        self.line_obj_prio[sx] = priority;
                        self.line_obj_mode[sx] = gfx_mode as u8;
                    }
                }
            }
        }
    }

    fn get_sprite_pixel(&self, base_tile: usize, x: u32, y: u32, sprite_w: u32,
                         is_8bpp: bool, pal_bank: usize, mapping_1d: bool) -> u16 {
        let tile_x = x / 8;
        let tile_y = y / 8;
        let fine_x = x % 8;
        let fine_y = y % 8;

        let tile_num = if mapping_1d {
            let tiles_per_row = sprite_w / 8;
            if is_8bpp {
                base_tile + (tile_y * tiles_per_row + tile_x) as usize * 2
            } else {
                base_tile + (tile_y * tiles_per_row + tile_x) as usize
            }
        } else {
            // 2D mapping: tile number wraps in a 32-tile-wide grid.
            if is_8bpp {
                (base_tile + tile_y as usize * 32 + tile_x as usize * 2) & 0x3FF
            } else {
                (base_tile + tile_y as usize * 32 + tile_x as usize) & 0x3FF
            }
        };

        let obj_tile_base = 0x10000; // OBJ tiles start at 0x06010000.
        // Each 4bpp tile = 32 bytes, each 8bpp tile = 64 bytes.
        // Tile number always indexes in 32-byte units on GBA.
        let tile_byte_base = obj_tile_base + tile_num * 32;

        if is_8bpp {
            let addr = tile_byte_base + fine_y as usize * 8 + fine_x as usize;
            if addr >= self.vram.len() { return 0; }
            self.vram[addr] as u16
        } else {
            let addr = tile_byte_base + fine_y as usize * 4 + (fine_x as usize / 2);
            if addr >= self.vram.len() { return 0; }
            let byte = self.vram[addr];
            let nibble = if fine_x & 1 == 0 { byte & 0xF } else { byte >> 4 };
            if nibble == 0 { return 0; }
            (pal_bank * 16 + nibble as usize) as u16
        }
    }

    // ================================================================
    // Color helpers
    // ================================================================

    fn palette_color(&self, idx: usize) -> u32 {
        let addr = idx * 2;
        if addr + 1 >= self.palette.len() { return self.backdrop_color(); }
        let rgb555 = u16::from_le_bytes([self.palette[addr], self.palette[addr + 1]]);
        Self::rgb555_to_rgba(rgb555)
    }

    fn backdrop_color(&self) -> u32 {
        let rgb555 = u16::from_le_bytes([self.palette[0], self.palette[1]]);
        Self::rgb555_to_rgba(rgb555)
    }

    fn rgb555_to_rgba(c: u16) -> u32 {
        let r = ((c & 0x1F) as u32) << 3;
        let g = (((c >> 5) & 0x1F) as u32) << 3;
        let b = (((c >> 10) & 0x1F) as u32) << 3;
        let r = r | (r >> 5);
        let g = g | (g >> 5);
        let b = b | (b >> 5);
        0xFF000000 | (b << 16) | (g << 8) | r
    }
}
