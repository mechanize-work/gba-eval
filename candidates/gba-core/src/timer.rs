//! GBA timer implementation. 4 timers (TM0-TM3), each 16-bit with:
//!   - Prescaler: F/1, F/64, F/256, F/1024 (F = 16.78 MHz)
//!   - Cascade mode: timer N increments when timer N-1 overflows
//!   - Overflow IRQ
//!   - Used for sound FIFO timing (TM0/TM1 → DMA sound channels A/B)

pub struct Timers {
    pub t: [TimerState; 4],
}

/// What `Timers::tick` reports back per timer.
#[derive(Clone, Copy)]
pub struct TimerOverflow {
    /// How many times this timer overflowed during the batch.
    pub count: u32,
    /// CPU-cycle offset within the batch where the FIRST overflow landed.
    /// Subsequent overflows are spaced by `Timers::period_cycles(i)`.
    /// This lets the APU place FIFO transitions sub-sample-accurately
    /// instead of snapping them to batch boundaries — without this,
    /// sample-and-hold aliasing leaks ~25% extra energy at 4-13 kHz.
    pub first_cycle: u32,
}

impl TimerOverflow {
    pub const NONE: Self = Self { count: 0, first_cycle: 0 };
}

#[derive(Clone)]
pub struct TimerState {
    pub reload: u16,
    pub counter: u16,
    pub control: u16,
    /// Internal fractional counter (counts CPU cycles within current prescaler period).
    pub prescaler_counter: u32,
    /// CPU cycles remaining before the timer starts ticking after a 0→1 enable.
    /// GBA hardware has a 2-cycle delay before a newly-enabled timer ticks.
    pub startup_delay: u32,
}

impl Default for TimerState {
    fn default() -> Self {
        Self { reload: 0, counter: 0, control: 0, prescaler_counter: 0, startup_delay: 0 }
    }
}

impl TimerState {
    pub fn enabled(&self) -> bool { self.control & 0x80 != 0 }
    pub fn cascade(&self) -> bool { self.control & 0x04 != 0 }
    pub fn irq_enabled(&self) -> bool { self.control & 0x40 != 0 }
    pub fn prescaler_shift(&self) -> u32 {
        match self.control & 3 {
            0 => 0,   // F/1
            1 => 6,   // F/64
            2 => 8,   // F/256
            3 => 10,  // F/1024
            _ => unreachable!(),
        }
    }
}

impl Timers {
    pub fn new() -> Self {
        Self { t: [TimerState::default(), TimerState::default(),
                   TimerState::default(), TimerState::default()] }
    }

    /// Advance all timers by `cycles` CPU cycles. Returns overflow count
    /// and first-overflow cycle offset per timer.
    pub fn tick(&mut self, cycles: u32) -> [TimerOverflow; 4] {
        let mut overflows = [TimerOverflow::NONE; 4];

        for i in 0..4 {
            if !self.t[i].enabled() { continue; }
            if self.t[i].cascade() { continue; } // driven by overflow, not cycles

            // Startup delay eats the first N cycles of this batch only —
            // remaining cycles still count.
            let mut effective_cycles = cycles;
            let mut delay_offset = 0u32;
            if self.t[i].startup_delay > 0 {
                if self.t[i].startup_delay >= cycles {
                    self.t[i].startup_delay -= cycles;
                    continue;
                }
                delay_offset = self.t[i].startup_delay;
                effective_cycles = cycles - self.t[i].startup_delay;
                self.t[i].startup_delay = 0;
            }

            let shift = self.t[i].prescaler_shift();
            // How many cycles until the prescaler emits its next tick:
            // it had `presc_phase` cycles accumulated, needs (1<<shift).
            let presc_phase = self.t[i].prescaler_counter;
            self.t[i].prescaler_counter += effective_cycles;
            let ticks = self.t[i].prescaler_counter >> shift;
            self.t[i].prescaler_counter &= (1 << shift) - 1;

            if ticks > 0 {
                let counter_before = self.t[i].counter;
                let (new_counter, n) = self.add_to_timer(i, ticks);
                self.t[i].counter = new_counter;
                if n > 0 {
                    // First overflow needs (0x10000 - counter) timer ticks.
                    // First tick costs ((1<<shift) - presc_phase) cycles,
                    // each subsequent tick costs (1<<shift).
                    let ticks_to_first = 0x10000u32 - counter_before as u32;
                    let cycles_to_first_tick = (1u32 << shift) - presc_phase;
                    let first_cycle = delay_offset + cycles_to_first_tick
                        + (ticks_to_first - 1).wrapping_shl(shift);
                    overflows[i] = TimerOverflow {
                        count: n,
                        first_cycle: first_cycle.min(cycles),
                    };
                }
            }
        }

        // Cascading: each overflow of timer i-1 ticks timer i once.
        for i in 1..4 {
            if !self.t[i].enabled() || !self.t[i].cascade() { continue; }
            if overflows[i - 1].count > 0 {
                let (new_counter, n) = self.add_to_timer(i, overflows[i - 1].count);
                self.t[i].counter = new_counter;
                if n > 0 {
                    overflows[i] = TimerOverflow {
                        count: n,
                        first_cycle: overflows[i - 1].first_cycle,
                    };
                }
            }
        }

        overflows
    }

    /// CPU cycles between consecutive overflows of timer `i` (no cascade).
    pub fn period_cycles(&self, i: usize) -> u32 {
        (0x10000u32 - self.t[i].reload as u32) << self.t[i].prescaler_shift()
    }

    /// Add `ticks` to timer `i`. Returns (new_counter, overflow_count).
    fn add_to_timer(&self, i: usize, ticks: u32) -> (u16, u32) {
        let counter = self.t[i].counter as u32;
        let reload = self.t[i].reload as u32;
        let sum = counter + ticks;

        if sum <= 0xFFFF {
            return (sum as u16, 0);
        }

        // First overflow consumes (0x10000 - counter) ticks.
        // Each subsequent overflow consumes (0x10000 - reload).
        let remaining = sum - 0x10000;
        let period = 0x10000 - reload;
        let extra_overflows = remaining / period;
        let new_val = reload + (remaining % period);
        (new_val as u16, 1 + extra_overflows)
    }

    pub fn write_reload(&mut self, i: usize, val: u16) {
        self.t[i].reload = val;
    }

    pub fn write_control(&mut self, i: usize, val: u16) {
        let was_enabled = self.t[i].enabled();
        let old_shift = self.t[i].prescaler_shift();
        self.t[i].control = val;

        if was_enabled && !self.t[i].enabled() && !self.t[i].cascade() {
            // 1→0 disable: the bus write happens partway through the
            // instruction (cycle 2 of STRH). The timer still ticks for
            // the cycle(s) before the write takes effect. Advance by 1
            // tick to account for this.
            let shift = old_shift;
            self.t[i].prescaler_counter += 1;
            let ticks = self.t[i].prescaler_counter >> shift;
            self.t[i].prescaler_counter &= (1 << shift) - 1;
            if ticks > 0 {
                let (new_counter, _) = self.add_to_timer(i, ticks);
                self.t[i].counter = new_counter;
            }
        } else if !was_enabled && self.t[i].enabled() {
            // 0→1 enable: reload counter, reset prescaler, 2-cycle enable delay.
            self.t[i].counter = self.t[i].reload;
            self.t[i].prescaler_counter = 0;
            self.t[i].startup_delay = 2;
        } else if was_enabled && self.t[i].enabled() {
            // 1→1 (prescaler or cascade change while running): reset prescaler
            // to avoid stale fractional cycles from the old rate.
            let new_shift = self.t[i].prescaler_shift();
            if new_shift != old_shift {
                self.t[i].prescaler_counter = 0;
            }
        }

        // 1→0 disable: nothing special (counter freezes at current value).
    }

    pub fn read_counter(&self, i: usize) -> u16 {
        self.t[i].counter
    }

    pub fn read_control(&self, i: usize) -> u16 {
        self.t[i].control
    }
}
