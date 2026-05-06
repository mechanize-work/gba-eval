//! APU — GBA sound system.
//!
//! 4 PSG channels (inherited from GBC):
//!   Ch1: Square wave with sweep
//!   Ch2: Square wave
//!   Ch3: Programmable wave
//!   Ch4: Noise
//!
//! 2 DMA sound channels (Direct Sound A & B):
//!   Fed by 32-byte FIFOs, clocked by Timer 0 or Timer 1.
//!
//! Output: 32768 Hz stereo (1 sample per ~512 CPU cycles).

const SAMPLE_RATE: u32 = 32768;
const CPU_FREQ: u32 = 16777216;
const CYCLES_PER_SAMPLE: u32 = CPU_FREQ / SAMPLE_RATE; // ~512
const FIFO_SIZE: usize = 32;

// Duty cycle patterns for square wave channels.
const DUTY_TABLE: [[u8; 8]; 4] = [
    [0, 0, 0, 0, 0, 0, 0, 1], // 12.5%
    [1, 0, 0, 0, 0, 0, 0, 1], // 25%
    [1, 0, 0, 0, 0, 1, 1, 1], // 50%
    [0, 1, 1, 1, 1, 1, 1, 0], // 75%
];

pub struct Apu {
    // Master control.
    pub soundcnt_l: u16,  // NR50/NR51 — PSG volume & panning.
    pub soundcnt_h: u16,  // DMA sound control.
    pub soundcnt_x: u16,  // Master enable.
    pub soundbias: u16,

    // PSG channels.
    pub ch1: SquareChannel,
    pub ch2: SquareChannel,
    pub ch3: WaveChannel,
    pub ch4: NoiseChannel,

    // DMA sound FIFOs. Zero-order hold: the latched byte sits in the DAC
    // until the next timer overflow swaps it. Hardware doesn't
    // interpolate, and neither does Mesen.
    pub fifo_a: Fifo,
    pub fifo_b: Fifo,
    pub fifo_a_sample: i8,
    pub fifo_b_sample: i8,

    // Sample generation.
    pub sample_clock: u32,
    pub buffer: Vec<f32>, // interleaved stereo samples for this frame

    // One-pole high-pass at ~21 Hz to strip DC bias from m4a-style
    // mixers; matches Mesen's GbaApu HPF.
    // y[n] = x[n] - x[n-1] + R*y[n-1], R = 0.996.
    dc_x_l: i32, dc_y_l: i32,
    dc_x_r: i32, dc_y_r: i32,
}

impl Apu {
    pub fn new() -> Self {
        Self {
            soundcnt_l: 0,
            soundcnt_h: 0,
            soundcnt_x: 0,
            soundbias: 0x0200,
            ch1: SquareChannel::new(),
            ch2: SquareChannel::new(),
            ch3: WaveChannel::new(),
            ch4: NoiseChannel::new(),
            fifo_a: Fifo::new(),
            fifo_b: Fifo::new(),
            fifo_a_sample: 0,
            fifo_b_sample: 0,
            sample_clock: 0,
            buffer: Vec::with_capacity(2048),
            dc_x_l: 0, dc_y_l: 0,
            dc_x_r: 0, dc_y_r: 0,
        }
    }

    pub fn enabled(&self) -> bool { self.soundcnt_x & 0x80 != 0 }

    /// Called every CPU cycle batch. Generates samples.
    pub fn tick(&mut self, cycles: u32) {
        self.tick_until(self.sample_clock.wrapping_add(cycles));
    }

    /// Advance sample generation up to `target` (sample_clock coordinate).
    /// Lets the bus interleave FIFO pops at their sub-batch cycle before
    /// samples in the same batch that see the old DAC latch are emitted.
    pub fn tick_until(&mut self, target: u32) {
        if !self.enabled() {
            self.sample_clock = target;
            return;
        }
        let mut end = (self.sample_clock / CYCLES_PER_SAMPLE + 1) * CYCLES_PER_SAMPLE;
        while end <= target {
            self.generate_sample();
            end += CYCLES_PER_SAMPLE;
        }
        self.sample_clock = target;
    }

    /// Clock the DMA sound FIFO. Each timer overflow pops one byte into
    /// the DAC latch. The half-empty gate in bus.rs handles refill DMA
    /// between calls.
    pub fn on_timer_overflow(&mut self, timer_id: usize) {
        let timer_a = if self.soundcnt_h & (1 << 10) != 0 { 1 } else { 0 };
        if timer_id == timer_a {
            self.fifo_a_sample = self.fifo_a.read();
        }
        let timer_b = if self.soundcnt_h & (1 << 14) != 0 { 1 } else { 0 };
        if timer_id == timer_b {
            self.fifo_b_sample = self.fifo_b.read();
        }
    }

    /// Returns true if FIFO A needs refilling (<=16 bytes left).
    pub fn fifo_a_needs_data(&self) -> bool { self.fifo_a.len <= 16 }
    /// Returns true if FIFO B needs refilling.
    pub fn fifo_b_needs_data(&self) -> bool { self.fifo_b.len <= 16 }

    /// `sample_end` is the sample_clock value at which this audio sample's
    /// 512-cycle window closes. FIFO transitions stamped inside
    fn generate_sample(&mut self) {
        // Advance PSG channel timers by one audio sample worth of CPU cycles.
        self.ch1.step(CYCLES_PER_SAMPLE);
        self.ch2.step(CYCLES_PER_SAMPLE);
        self.ch3.step(CYCLES_PER_SAMPLE);
        self.ch4.step(CYCLES_PER_SAMPLE);

        // PSG: unipolar 0..15 per channel. The SOUNDBIAS clamp downstream
        // is what bounds the positive DC this introduces.
        let ch1 = self.ch1.output() as i32;
        let ch2 = self.ch2.output() as i32;
        let ch3 = self.ch3.output() as i32;
        let ch4 = self.ch4.output() as i32;

        let nr51 = self.soundcnt_l >> 8;
        let mut psg_l: i32 = 0;
        let mut psg_r: i32 = 0;
        if nr51 & 0x01 != 0 { psg_r += ch1; }
        if nr51 & 0x02 != 0 { psg_r += ch2; }
        if nr51 & 0x04 != 0 { psg_r += ch3; }
        if nr51 & 0x10 != 0 { psg_l += ch1; }
        if nr51 & 0x20 != 0 { psg_l += ch2; }
        if nr51 & 0x40 != 0 { psg_l += ch3; }
        psg_l <<= 3;
        psg_r <<= 3;
        if nr51 & 0x08 != 0 { psg_r += ch4 << 3; }
        if nr51 & 0x80 != 0 { psg_l += ch4 << 3; }

        let vol_r = ((self.soundcnt_l >> 0) & 7) as i32 + 1;
        let vol_l = ((self.soundcnt_l >> 4) & 7) as i32 + 1;
        psg_r *= vol_r;
        psg_l *= vol_l;

        // SOUNDCNT_H bits 0-1: PSG master volume → shift 4/3/2.
        let psg_shift = 4 - (self.soundcnt_h & 3).min(2) as i32;
        psg_r >>= psg_shift;
        psg_l >>= psg_shift;

        // DMA: zero-order hold — the FIFO byte sits in the DAC latch until
        // the next timer overflow swaps it. i8 → 24.8 fixed-point.
        let fa = (self.fifo_a_sample as i32) << 8;
        let fb = (self.fifo_b_sample as i32) << 8;
        // i8.8 << 2 → ±512 in 24.8, then halve if SOUNDCNT_H bit clear.
        let dma_a = (fa << 2) >> (if self.soundcnt_h & (1 << 2) != 0 { 0 } else { 1 });
        let dma_b = (fb << 2) >> (if self.soundcnt_h & (1 << 3) != 0 { 0 } else { 1 });

        // Mix in 24.8 so the bias clamp sees the interpolated value.
        let mut left  = psg_l << 8;
        let mut right = psg_r << 8;
        if self.soundcnt_h & (1 << 8)  != 0 { right += dma_a; }
        if self.soundcnt_h & (1 << 9)  != 0 { left  += dma_a; }
        if self.soundcnt_h & (1 << 12) != 0 { right += dma_b; }
        if self.soundcnt_h & (1 << 13) != 0 { left  += dma_b; }

        // SOUNDBIAS bits 14-15: DAC resolution. Trades bit depth for sample
        // rate (9bit/32768Hz → 6bit/262144Hz). Pokemon writes resolution=1.
        //
        // The mixer above works at 2× the nominal DAC scale: DMA at full
        // volume is i8<<2 = ±512, but the real 9-bit DAC is 0-511 (signed
        // ±256). The bias register (bits 1-9, default 0x200) is ALSO at 2×
        // (bit 1 is the LSB position, so 0x200 represents bias-value 256).
        // Keeping both at 2× makes the arithmetic cancel — until you change
        // resolution and only one of them moves.
        //
        // Lower resolution drops the low bits of BOTH the mix and the bias.
        // At res=1: 8-bit DAC, range 0-255, mix>>1, bias>>1. The clamp
        // range is also halved. After clamp, scale BACK up so the output
        // amplitude is constant — what changes is the quantization
        // granularity (audible as more noise floor at high res, which is
        // why GBATEK says res=0 is "best for DMA").
        let resolution = ((self.soundbias >> 14) & 3) as i32;
        // All in 24.8 fixed point. At res=0: bias≈512<<8, max=1023<<8.
        let bias_2x = (self.soundbias & 0x3FE) as i32;
        let bias = (bias_2x >> resolution) << 8;
        let max  = (0x3FF >> resolution) << 8;
        let lq = (left  >> resolution) + bias;
        let rq = (right >> resolution) + bias;
        // Re-center and gain. ×32 matches Mesen's mapping of the 9-bit
        // DAC range to i16. <<resolution undoes the >> above so output
        // amplitude is resolution-independent.
        let gain = 32 << resolution;
        let l = ((lq.clamp(0, max) - bias) * gain) >> 8;
        let r = ((rq.clamp(0, max) - bias) * gain) >> 8;

        // No anti-alias filter — Mesen emits the raw post-DAC signal,
        // and ZOH FIFO + no filter is what real hardware does.

        // DC blocker: y[n] = x[n] - x[n-1] + R*y[n-1], R ≈ 0.996.
        // m4a's software mixer produces DC-biased buffers; Mesen has the
        // same thing (GbaApu sets a 20 Hz HPF on _filterL/_filterR).
        // Fixed-point: R = 1 - 1/256 → multiply by 255/256 via (y - (y>>8)).
        // State scaled <<8 to keep precision through the (y>>8) feedback.
        let yl = ((l - self.dc_x_l) << 8) + self.dc_y_l - (self.dc_y_l >> 8);
        let yr = ((r - self.dc_x_r) << 8) + self.dc_y_r - (self.dc_y_r >> 8);
        self.dc_x_l = l; self.dc_y_l = yl;
        self.dc_x_r = r; self.dc_y_r = yr;

        self.buffer.push((yl >> 8) as f32 / 32768.0);
        self.buffer.push((yr >> 8) as f32 / 32768.0);
    }

    /// Drain audio buffer (called once per frame by WASM).
    pub fn drain_samples(&mut self) -> Vec<f32> {
        std::mem::take(&mut self.buffer)
    }

    // ---- Frame sequencer (512 Hz from DIV counter) ----
    /// Called at 512 Hz (every 32768 cycles). Steps envelope, length, sweep.
    pub fn frame_sequencer_step(&mut self, step: u8) {
        // Length counter: steps 0, 2, 4, 6.
        if step % 2 == 0 {
            self.ch1.length_tick();
            self.ch2.length_tick();
            self.ch3.length_tick();
            self.ch4.length_tick();
        }
        // Sweep: steps 2, 6.
        if step == 2 || step == 6 {
            self.ch1.sweep_tick();
        }
        // Envelope: step 7.
        if step == 7 {
            self.ch1.envelope_tick();
            self.ch2.envelope_tick();
            self.ch4.envelope_tick();
        }
    }

    // ---- I/O register access ----

    pub fn read_reg(&self, addr: u32) -> u16 {
        match addr & 0xFFF {
            0x060 => self.ch1.read_nr10(),
            0x062 => self.ch1.read_nr11_12(),
            0x064 => self.ch1.read_nr13_14(),
            0x068 => self.ch2.read_nr11_12(),
            0x06C => self.ch2.read_nr13_14(),
            0x070 => self.ch3.read_nr30(),
            0x072 => self.ch3.read_nr31_32(),
            0x074 => self.ch3.read_nr33_34(),
            0x078 => self.ch4.read_nr41_42(),
            0x07C => self.ch4.read_nr43_44(),
            0x080 => self.soundcnt_l,
            0x082 => self.soundcnt_h,
            0x084 => {
                let mut v = self.soundcnt_x & 0x80;
                if self.ch1.active { v |= 1; }
                if self.ch2.active { v |= 2; }
                if self.ch3.active { v |= 4; }
                if self.ch4.active { v |= 8; }
                v
            }
            0x088 => self.soundbias,
            0x090..=0x09E => self.ch3.read_wave(addr),
            _ => 0,
        }
    }

    pub fn write_reg(&mut self, addr: u32, val: u16) {
        let reg = addr & 0xFFF;
        // When the master enable bit is clear, hardware blocks writes to the
        // PSG register block 0x060-0x081 only. SOUNDCNT_H (0x082), SOUNDCNT_X
        // (0x084), SOUNDBIAS (0x088), wave RAM, and FIFOs remain writable.
        // GBATEK: "registers 4000082h and 4000088h are kept read/write-able."
        if !self.enabled() && reg <= 0x081 { return; }

        match reg {
            0x060 => self.ch1.write_nr10(val),
            0x062 => self.ch1.write_nr11_12(val),
            0x064 => self.ch1.write_nr13_14(val),
            0x068 => self.ch2.write_nr11_12(val),
            0x06C => self.ch2.write_nr13_14(val),
            0x070 => self.ch3.write_nr30(val),
            0x072 => self.ch3.write_nr31_32(val),
            0x074 => self.ch3.write_nr33_34(val),
            0x078 => self.ch4.write_nr41_42(val),
            0x07C => self.ch4.write_nr43_44(val),
            0x080 => self.soundcnt_l = val,
            0x082 => {
                // Reset FIFOs on bits 11/15.
                if val & (1 << 11) != 0 { self.fifo_a.reset(); }
                if val & (1 << 15) != 0 { self.fifo_b.reset(); }
                self.soundcnt_h = val & !(0x0800 | 0x8000); // clear reset bits
            }
            0x084 => {
                let was_enabled = self.enabled();
                self.soundcnt_x = val & 0x80;
                if was_enabled && !self.enabled() {
                    // Power off: zero all PSG registers.
                    self.ch1 = SquareChannel::new();
                    self.ch2 = SquareChannel::new();
                    self.ch3 = WaveChannel::new();
                    self.ch4 = NoiseChannel::new();
                }
            }
            0x088 => self.soundbias = val & 0xC3FE,
            0x090..=0x09E => self.ch3.write_wave(addr, val),
            0x0A0 => self.fifo_a.write(val as u8, (val >> 8) as u8),
            0x0A2 => self.fifo_a.write(val as u8, (val >> 8) as u8),
            0x0A4 => self.fifo_b.write(val as u8, (val >> 8) as u8),
            0x0A6 => self.fifo_b.write(val as u8, (val >> 8) as u8),
            _ => {}
        }
    }

    pub fn write_fifo_32(&mut self, addr: u32, val: u32) {
        let bytes = val.to_le_bytes();
        match addr & 0xFFF {
            0x0A0 => {
                self.fifo_a.write(bytes[0], bytes[1]);
                self.fifo_a.write(bytes[2], bytes[3]);
            }
            0x0A4 => {
                self.fifo_b.write(bytes[0], bytes[1]);
                self.fifo_b.write(bytes[2], bytes[3]);
            }
            _ => {}
        }
    }
}

// ================================================================
// FIFO
// ================================================================

pub struct Fifo {
    data: [i8; FIFO_SIZE],
    read_pos: usize,
    write_pos: usize,
    pub len: usize,
}

impl Fifo {
    fn new() -> Self {
        Self { data: [0; FIFO_SIZE], read_pos: 0, write_pos: 0, len: 0 }
    }

    fn reset(&mut self) {
        self.read_pos = 0;
        self.write_pos = 0;
        self.len = 0;
    }

    fn write(&mut self, a: u8, b: u8) {
        if self.len < FIFO_SIZE {
            self.data[self.write_pos] = a as i8;
            self.write_pos = (self.write_pos + 1) % FIFO_SIZE;
            self.len += 1;
        }
        if self.len < FIFO_SIZE {
            self.data[self.write_pos] = b as i8;
            self.write_pos = (self.write_pos + 1) % FIFO_SIZE;
            self.len += 1;
        }
    }

    fn read(&mut self) -> i8 {
        if self.len == 0 { return 0; }
        let val = self.data[self.read_pos];
        self.read_pos = (self.read_pos + 1) % FIFO_SIZE;
        self.len -= 1;
        val
    }
}

// ================================================================
// Square Wave Channel (Ch1 / Ch2)
// ================================================================

pub struct SquareChannel {
    pub active: bool,
    // NR10 (Ch1 only): sweep.
    sweep_period: u8,
    sweep_negate: bool,
    sweep_shift: u8,
    sweep_timer: u8,
    sweep_enabled: bool,
    sweep_shadow: u16,
    // NR11: duty + length.
    duty: u8,
    length_counter: u16,
    length_enabled: bool,
    // NR12: envelope.
    env_initial: u8,
    env_direction: bool, // true = increase
    env_period: u8,
    env_timer: u8,
    env_volume: u8,
    // NR13/NR14: frequency.
    frequency: u16,
    // Internal.
    timer: u32,
    duty_pos: u8,
}

impl SquareChannel {
    fn new() -> Self {
        Self {
            active: false,
            sweep_period: 0, sweep_negate: false, sweep_shift: 0,
            sweep_timer: 0, sweep_enabled: false, sweep_shadow: 0,
            duty: 0, length_counter: 0, length_enabled: false,
            env_initial: 0, env_direction: false, env_period: 0, env_timer: 0, env_volume: 0,
            frequency: 0, timer: 0, duty_pos: 0,
        }
    }

    /// Advance the channel by `cycles` CPU cycles. The duty position
    /// advances every (2048-freq)*16 cycles (1048576 Hz timer base).
    fn step(&mut self, mut cycles: u32) {
        if !self.active { return; }
        while cycles >= self.timer {
            cycles -= self.timer;
            self.timer = (2048 - self.frequency as u32) * 16;
            self.duty_pos = (self.duty_pos + 1) & 7;
        }
        self.timer -= cycles;
    }

    /// Unipolar 0..15: duty_bit × envelope volume. The mixer's
    /// SOUNDBIAS clamp handles centering.
    fn output(&self) -> i16 {
        if !self.active { return 0; }
        DUTY_TABLE[self.duty as usize][self.duty_pos as usize] as i16 * self.env_volume as i16
    }

    fn length_tick(&mut self) {
        if self.length_enabled && self.length_counter > 0 {
            self.length_counter -= 1;
            if self.length_counter == 0 { self.active = false; }
        }
    }

    fn envelope_tick(&mut self) {
        if self.env_period == 0 { return; }
        self.env_timer = self.env_timer.saturating_sub(1);
        if self.env_timer == 0 {
            self.env_timer = self.env_period;
            if self.env_direction && self.env_volume < 15 {
                self.env_volume += 1;
            } else if !self.env_direction && self.env_volume > 0 {
                self.env_volume -= 1;
            }
        }
    }

    fn sweep_tick(&mut self) {
        if !self.sweep_enabled || self.sweep_period == 0 { return; }
        self.sweep_timer = self.sweep_timer.saturating_sub(1);
        if self.sweep_timer == 0 {
            self.sweep_timer = self.sweep_period;
            let new_freq = self.calc_sweep();
            if new_freq <= 2047 && self.sweep_shift > 0 {
                self.sweep_shadow = new_freq;
                self.frequency = new_freq;
                // Overflow check again.
                if self.calc_sweep() > 2047 { self.active = false; }
            } else if new_freq > 2047 {
                self.active = false;
            }
        }
    }

    fn calc_sweep(&self) -> u16 {
        let delta = self.sweep_shadow >> self.sweep_shift;
        if self.sweep_negate {
            self.sweep_shadow.wrapping_sub(delta)
        } else {
            self.sweep_shadow + delta
        }
    }

    fn trigger(&mut self) {
        self.active = true;
        if self.length_counter == 0 { self.length_counter = 64; }
        self.timer = (2048 - self.frequency as u32) * 16;
        self.env_volume = self.env_initial;
        self.env_timer = self.env_period;
        self.sweep_shadow = self.frequency;
        self.sweep_timer = if self.sweep_period > 0 { self.sweep_period } else { 8 };
        self.sweep_enabled = self.sweep_period > 0 || self.sweep_shift > 0;
        if self.sweep_shift > 0 && self.calc_sweep() > 2047 { self.active = false; }
        if self.env_initial == 0 && !self.env_direction { self.active = false; }
    }

    fn read_nr10(&self) -> u16 {
        ((self.sweep_period as u16) << 4) | ((self.sweep_negate as u16) << 3) | self.sweep_shift as u16
    }
    fn read_nr11_12(&self) -> u16 {
        ((self.duty as u16) << 6)
        | ((self.env_initial as u16) << 12)
        | ((self.env_direction as u16) << 11)
        | ((self.env_period as u16) << 8)
    }
    fn read_nr13_14(&self) -> u16 {
        (self.length_enabled as u16) << 14
    }

    fn write_nr10(&mut self, val: u16) {
        self.sweep_period = ((val >> 4) & 7) as u8;
        self.sweep_negate = val & 8 != 0;
        self.sweep_shift = (val & 7) as u8;
    }
    fn write_nr11_12(&mut self, val: u16) {
        self.duty = ((val >> 6) & 3) as u8;
        self.length_counter = 64 - (val & 0x3F);
        self.env_period = ((val >> 8) & 7) as u8;
        self.env_direction = val & (1 << 11) != 0;
        self.env_initial = ((val >> 12) & 0xF) as u8;
        if self.env_initial == 0 && !self.env_direction { self.active = false; }
    }
    fn write_nr13_14(&mut self, val: u16) {
        self.frequency = (self.frequency & 0x700) | (val & 0x7FF);
        self.length_enabled = val & (1 << 14) != 0;
        if val & 0x8000 != 0 {
            self.frequency = val & 0x7FF;
            self.trigger();
        }
    }
}

// ================================================================
// Wave Channel (Ch3)
// ================================================================

pub struct WaveChannel {
    pub active: bool,
    enabled: bool,
    length_counter: u16,
    length_enabled: bool,
    volume_code: u8,
    force_75: bool,
    frequency: u16,
    timer: u32,
    pos: u8,
    wave_ram: [u8; 32], // 2 banks of 16 bytes, as nibbles = 32 samples
    bank: u8,
    dimension: bool, // true = 64 samples
}

impl WaveChannel {
    fn new() -> Self {
        Self {
            active: false, enabled: false,
            length_counter: 0, length_enabled: false,
            volume_code: 0, force_75: false,
            frequency: 0, timer: 0, pos: 0,
            wave_ram: [0; 32], bank: 0, dimension: false,
        }
    }

    /// Advance by `cycles` CPU cycles. Wave RAM position advances every
    /// (2048-freq)*8 cycles (2097152 Hz sample clock).
    fn step(&mut self, mut cycles: u32) {
        if !self.active || !self.enabled { return; }
        let max = if self.dimension { 64 } else { 32 };
        while cycles >= self.timer {
            cycles -= self.timer;
            self.timer = (2048 - self.frequency as u32) * 8;
            self.pos = (self.pos + 1) % max;
        }
        self.timer -= cycles;
    }

    /// Unipolar 4-bit nibble with volume applied. force_75 multiplies
    /// the sample by 3; volume_code provides the right-shift amount.
    fn output(&self) -> i16 {
        if !self.active || !self.enabled { return 0; }
        let idx = if self.dimension {
            self.pos as usize
        } else {
            (self.bank as usize * 32 + self.pos as usize) % 64
        };
        let byte = self.wave_ram[idx / 2];
        let mut sample = (if idx & 1 == 0 { byte >> 4 } else { byte & 0xF }) as i16;
        let shift = match self.volume_code {
            0 => 4, 1 => 0, 2 => 1, _ => 2,
        };
        if self.force_75 {
            sample += sample << 1;  // ×3
        }
        sample >> shift
    }

    fn length_tick(&mut self) {
        if self.length_enabled && self.length_counter > 0 {
            self.length_counter -= 1;
            if self.length_counter == 0 { self.active = false; }
        }
    }

    fn read_nr30(&self) -> u16 {
        ((self.dimension as u16) << 5)
        | ((self.bank as u16) << 6)
        | ((self.enabled as u16) << 7)
    }
    fn read_nr31_32(&self) -> u16 {
        ((self.volume_code as u16) << 13) | ((self.force_75 as u16) << 15)
    }
    fn read_nr33_34(&self) -> u16 { (self.length_enabled as u16) << 14 }
    fn read_wave(&self, addr: u32) -> u16 {
        let i = ((addr & 0xF) * 2) as usize;
        (self.wave_ram[i] as u16) | ((self.wave_ram[i + 1] as u16) << 8)
    }

    fn write_nr30(&mut self, val: u16) {
        self.dimension = val & (1 << 5) != 0;
        self.bank = ((val >> 6) & 1) as u8;
        self.enabled = val & (1 << 7) != 0;
        if !self.enabled { self.active = false; }
    }
    fn write_nr31_32(&mut self, val: u16) {
        self.length_counter = 256 - (val & 0xFF);
        self.volume_code = ((val >> 13) & 3) as u8;
        self.force_75 = val & (1 << 15) != 0;
    }
    fn write_nr33_34(&mut self, val: u16) {
        self.frequency = (self.frequency & 0x700) | (val & 0x7FF);
        self.length_enabled = val & (1 << 14) != 0;
        if val & 0x8000 != 0 {
            self.frequency = val & 0x7FF;
            self.active = true;
            if self.length_counter == 0 { self.length_counter = 256; }
            self.timer = (2048 - self.frequency as u32) * 8;
            self.pos = 0;
        }
    }
    fn write_wave(&mut self, addr: u32, val: u16) {
        let i = ((addr & 0xF) * 2) as usize;
        self.wave_ram[i] = val as u8;
        self.wave_ram[i + 1] = (val >> 8) as u8;
    }
}

// ================================================================
// Noise Channel (Ch4)
// ================================================================

pub struct NoiseChannel {
    pub active: bool,
    length_counter: u16,
    length_enabled: bool,
    env_initial: u8,
    env_direction: bool,
    env_period: u8,
    env_timer: u8,
    env_volume: u8,
    clock_shift: u8,
    width_mode: bool, // true = 7-bit, false = 15-bit
    divisor_code: u8,
    timer: u32,
    lfsr: u16,
}

impl NoiseChannel {
    fn new() -> Self {
        Self {
            active: false, length_counter: 0, length_enabled: false,
            env_initial: 0, env_direction: false, env_period: 0, env_timer: 0, env_volume: 0,
            clock_shift: 0, width_mode: false, divisor_code: 0,
            timer: 0, lfsr: 0x7FFF,
        }
    }

    /// Period in CPU cycles. Noise frequency = 524288 / r / 2^(s+1) Hz with
    /// r = max(divisor_code, 0.5), so period = 32 cycles when code=0,
    /// otherwise 64*code cycles, then shifted by clock_shift.
    fn period(&self) -> u32 {
        let base = if self.divisor_code == 0 { 32 } else { self.divisor_code as u32 * 64 };
        base << self.clock_shift
    }

    fn step(&mut self, mut cycles: u32) {
        if !self.active { return; }
        while cycles >= self.timer {
            cycles -= self.timer;
            self.timer = self.period();
            let xor = (self.lfsr & 1) ^ ((self.lfsr >> 1) & 1);
            self.lfsr = (self.lfsr >> 1) | (xor << 14);
            if self.width_mode {
                self.lfsr = (self.lfsr & !0x40) | (xor << 6);
            }
        }
        self.timer -= cycles;
    }

    /// Unipolar 0..15: inverted LFSR bit × envelope volume.
    fn output(&self) -> i16 {
        if !self.active { return 0; }
        ((!self.lfsr & 1) as i16) * self.env_volume as i16
    }

    fn length_tick(&mut self) {
        if self.length_enabled && self.length_counter > 0 {
            self.length_counter -= 1;
            if self.length_counter == 0 { self.active = false; }
        }
    }

    fn envelope_tick(&mut self) {
        if self.env_period == 0 { return; }
        self.env_timer = self.env_timer.saturating_sub(1);
        if self.env_timer == 0 {
            self.env_timer = self.env_period;
            if self.env_direction && self.env_volume < 15 { self.env_volume += 1; }
            else if !self.env_direction && self.env_volume > 0 { self.env_volume -= 1; }
        }
    }

    fn read_nr41_42(&self) -> u16 {
        ((self.env_initial as u16) << 12)
        | ((self.env_direction as u16) << 11)
        | ((self.env_period as u16) << 8)
    }
    fn read_nr43_44(&self) -> u16 {
        // SOUND4CNT_H: NR43 in bits 0-7, NR44 in bits 8-15.
        (self.divisor_code as u16)
        | ((self.width_mode as u16) << 3)
        | ((self.clock_shift as u16) << 4)
        | ((self.length_enabled as u16) << 14)
    }

    fn write_nr41_42(&mut self, val: u16) {
        self.length_counter = 64 - (val & 0x3F);
        self.env_period = ((val >> 8) & 7) as u8;
        self.env_direction = val & (1 << 11) != 0;
        self.env_initial = ((val >> 12) & 0xF) as u8;
    }
    fn write_nr43_44(&mut self, val: u16) {
        // SOUND4CNT_H: divisor (0-2), width (3), clock shift (4-7) | length (14), trigger (15)
        self.divisor_code = (val & 7) as u8;
        self.width_mode = val & (1 << 3) != 0;
        self.clock_shift = ((val >> 4) & 0xF) as u8;
        self.length_enabled = val & (1 << 14) != 0;
        if val & 0x8000 != 0 {
            self.active = true;
            if self.length_counter == 0 { self.length_counter = 64; }
            self.env_volume = self.env_initial;
            self.env_timer = self.env_period;
            self.lfsr = 0x7FFF;
            self.timer = self.period();
            if self.env_initial == 0 && !self.env_direction { self.active = false; }
        }
    }
}
