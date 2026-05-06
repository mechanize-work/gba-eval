//! GPIO / RTC (Real Time Clock) support.
//!
//! Pokemon games use GPIO pins at ROM addresses 0x080000C4-0x080000C8 to
//! communicate with the RTC chip (S-3511A). The protocol is a serial
//! bit-bang interface.
//!
//! GPIO registers:
//!   0x080000C4: Data (read/write)
//!   0x080000C6: Direction (0=input, 1=output per bit)
//!   0x080000C8: Control (bit 0: 1=readable)
//!
//! RTC pins (bits in data register):
//!   Bit 0: SCK (clock)
//!   Bit 1: SIO (data)
//!   Bit 2: CS (chip select)

pub struct Gpio {
    pub data: u16,
    pub direction: u16,
    pub control: u16,
    pub readable: bool,

    rtc: Rtc,
}

struct Rtc {
    state: RtcState,
    cmd: u8,
    cmd_bits: u8,
    data_bits: u8,
    data_byte: u8,
    data_out: Vec<u8>,
    data_idx: usize,
    last_sck: bool,
    last_cs: bool,
}

#[derive(PartialEq)]
enum RtcState {
    Idle,
    Command,
    ReadData,
    WriteData,
}

impl Gpio {
    pub fn new() -> Self {
        Self {
            data: 0,
            direction: 0,
            control: 0,
            readable: false,
            rtc: Rtc::new(),
        }
    }

    pub fn read16(&self, addr: u32) -> u16 {
        if !self.readable { return 0; }
        match addr & 0xFF {
            0xC4 => {
                // Return input pins (direction=0) from RTC, output pins from data register.
                let input = self.rtc.read_sio() as u16;
                (self.data & self.direction) | (input & !self.direction)
            }
            0xC6 => self.direction,
            0xC8 => self.control,
            _ => 0,
        }
    }

    pub fn write16(&mut self, addr: u32, val: u16) {
        match addr & 0xFF {
            0xC4 => {
                self.data = val & 0xF;
                // Drive RTC pins with output bits.
                let sck = self.data & 1 != 0;
                let sio = (self.data >> 1) & 1 != 0;
                let cs = (self.data >> 2) & 1 != 0;
                // Only drive if direction is output.
                let sck_out = if self.direction & 1 != 0 { sck } else { false };
                let sio_out = if self.direction & 2 != 0 { Some(sio) } else { None };
                let cs_out = if self.direction & 4 != 0 { cs } else { true };
                self.rtc.clock(sck_out, sio_out, cs_out);
            }
            0xC6 => self.direction = val & 0xF,
            0xC8 => {
                self.control = val & 1;
                self.readable = val & 1 != 0;
            }
            _ => {}
        }
    }
}

impl Rtc {
    fn new() -> Self {
        Self {
            state: RtcState::Idle,
            cmd: 0,
            cmd_bits: 0,
            data_bits: 0,
            data_byte: 0,
            data_out: Vec::new(),
            data_idx: 0,
            last_sck: false,
            last_cs: false,
        }
    }

    fn read_sio(&self) -> u8 {
        if self.state == RtcState::ReadData && self.data_idx < self.data_out.len() {
            let byte = self.data_out[self.data_idx];
            let bit = (byte >> self.data_bits) & 1;
            bit << 1 // SIO is bit 1
        } else {
            0
        }
    }

    fn clock(&mut self, sck: bool, sio: Option<bool>, cs: bool) {
        // CS rising edge: start command.
        if cs && !self.last_cs {
            self.state = RtcState::Command;
            self.cmd = 0;
            self.cmd_bits = 0;
        }

        // CS falling edge: end transaction.
        if !cs && self.last_cs {
            self.state = RtcState::Idle;
        }

        self.last_cs = cs;

        if !cs { return; }

        // Rising edge of SCK: latch data.
        if sck && !self.last_sck {
            match self.state {
                RtcState::Command => {
                    if let Some(sio_val) = sio {
                        self.cmd |= (sio_val as u8) << self.cmd_bits;
                        self.cmd_bits += 1;
                        if self.cmd_bits == 8 {
                            self.process_command();
                        }
                    }
                }
                RtcState::ReadData => {
                    // Advance bit counter (data is read by the GPIO on SIO pin).
                    self.data_bits += 1;
                    if self.data_bits >= 8 {
                        self.data_bits = 0;
                        self.data_idx += 1;
                    }
                }
                RtcState::WriteData => {
                    if let Some(sio_val) = sio {
                        self.data_byte |= (sio_val as u8) << self.data_bits;
                        self.data_bits += 1;
                        if self.data_bits >= 8 {
                            self.data_bits = 0;
                            self.data_byte = 0;
                            // Could store the written data, but we don't need it.
                        }
                    }
                }
                _ => {}
            }
        }

        self.last_sck = sck;
    }

    fn process_command(&mut self) {
        // RTC command byte format: MSB first, reversed.
        // Bit 0-3: parameter, bit 4-6: command, bit 7: read(1)/write(0)
        let reversed = self.cmd.reverse_bits();
        let is_read = reversed & 0x80 != 0;
        let cmd_id = (reversed >> 4) & 0x7;

        if is_read {
            self.data_out.clear();
            match cmd_id {
                0 => { /* Reset */ }
                1 => { /* Status register 1 */ self.data_out.push(0); }
                2 => {
                    // Date/time: 7 bytes (year, month, day, weekday, hour, minute, second)
                    // Use system time or return dummy values.
                    let now = get_rtc_time();
                    self.data_out.extend_from_slice(&now);
                }
                3 => { /* Status register 2 */ self.data_out.push(0x40); } // 24h mode
                4 => {
                    // Time only: 3 bytes (hour, minute, second)
                    let now = get_rtc_time();
                    self.data_out.extend_from_slice(&now[4..7]);
                }
                _ => {}
            }
            self.data_idx = 0;
            self.data_bits = 0;
            self.state = RtcState::ReadData;
        } else {
            self.state = RtcState::WriteData;
            self.data_bits = 0;
            self.data_byte = 0;
        }
    }
}

/// Get current time as BCD-encoded bytes for RTC.
fn get_rtc_time() -> [u8; 7] {
    // Return a fixed time since we can't easily get system time in no_std/wasm.
    // Year=24, Month=6, Day=15, Weekday=6(Sat), Hour=12, Min=30, Sec=0
    [
        to_bcd(24), // year
        to_bcd(6),  // month
        to_bcd(15), // day
        to_bcd(6),  // weekday
        to_bcd(12), // hour
        to_bcd(30), // minute
        to_bcd(0),  // second
    ]
}

fn to_bcd(val: u8) -> u8 {
    ((val / 10) << 4) | (val % 10)
}
