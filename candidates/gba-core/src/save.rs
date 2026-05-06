//! Save/backup media emulation: SRAM, Flash (64K/128K), EEPROM.
//!
//! Save type is auto-detected from ROM strings:
//!   "SRAM_V"     -> SRAM 32K
//!   "FLASH_V"    -> Flash 64K
//!   "FLASH512_V" -> Flash 64K
//!   "FLASH1M_V"  -> Flash 128K
//!   "EEPROM_V"   -> EEPROM (512B or 8K, determined by DMA transfer size)

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SaveType {
    None,
    Sram,
    Flash64,
    Flash128,
    Eeprom,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FlashState {
    Ready,
    Cmd1,          // received 0x5555=0xAA
    Cmd2,          // received 0x2AAA=0x55
    Erase,         // waiting for sector erase or chip erase
    Write,         // single byte program mode
    BankSelect,    // waiting for bank number (Flash 128K only)
    #[allow(dead_code)]
    IdMode,
}

pub struct Save {
    pub save_type: SaveType,
    pub data: Vec<u8>,

    // Flash state machine.
    flash_state: FlashState,
    flash_bank: usize,
    flash_id_mode: bool,
    flash_erase_pending: bool, // set after 0x80 command, cleared after erase executes
    flash_manufacturer: u8,
    flash_device: u8,

    pub flash_read_count: u32,
    pub flash_write_count: u32,

    // EEPROM state machine.
    eeprom_state: EepromState,
    eeprom_bits_written: u32,
    eeprom_command: u64,    // accumulated command bits
    eeprom_address: u16,    // current read/write address
    eeprom_read_buffer: u64, // 64 bits to read out
    eeprom_read_pos: u8,
    eeprom_size: EepromSize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EepromState {
    Idle,
    ReadingCommand,
    ReadingData,
    WritingData,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EepromSize {
    Small, // 512 bytes (6-bit address, 4Kbit)
    Large, // 8KB (14-bit address, 64Kbit)
}

impl Save {
    pub fn new() -> Self {
        Self {
            save_type: SaveType::None,
            data: vec![0xFF; 128 * 1024],
            flash_state: FlashState::Ready,
            flash_bank: 0,
            flash_id_mode: false,
            flash_erase_pending: false,
            flash_read_count: 0,
            flash_write_count: 0,
            flash_manufacturer: 0x62,
            flash_device: 0x13,
            eeprom_state: EepromState::Idle,
            eeprom_bits_written: 0,
            eeprom_command: 0,
            eeprom_address: 0,
            eeprom_read_buffer: 0,
            eeprom_read_pos: 0,
            eeprom_size: EepromSize::Large,
        }
    }

    /// Detect save type from ROM contents.
    pub fn detect_from_rom(&mut self, rom: &[u8]) {
        let s = String::from_utf8_lossy(rom);
        if s.contains("FLASH1M_V") {
            self.save_type = SaveType::Flash128;
            self.flash_manufacturer = 0x62; // Sanyo
            self.flash_device = 0x13;       // 128K
        } else if s.contains("FLASH512_V") || s.contains("FLASH_V") {
            self.save_type = SaveType::Flash64;
            self.flash_manufacturer = 0x32; // Panasonic
            self.flash_device = 0x1B;       // 64K
        } else if s.contains("SRAM_V") {
            self.save_type = SaveType::Sram;
        } else if s.contains("EEPROM_V") {
            self.save_type = SaveType::Eeprom;
        } else {
            self.save_type = SaveType::None;
        }
    }

    pub fn read8(&mut self, addr: u32) -> u8 {
        if matches!(self.save_type, SaveType::Flash64 | SaveType::Flash128) {
            self.flash_read_count += 1;
        }
        match self.save_type {
            SaveType::Sram => {
                self.data[(addr as usize) & 0x7FFF]
            }
            SaveType::Flash64 | SaveType::Flash128 => {
                if self.flash_id_mode {
                    let local = addr & 1;
                    if local == 0 { self.flash_manufacturer } else { self.flash_device }
                } else {
                    let offset = self.flash_bank * 0x10000 + (addr as usize & 0xFFFF);
                    self.data[offset]
                }
            }
            SaveType::Eeprom => {
                // EEPROM read: return current bit from read buffer.
                if self.eeprom_state == EepromState::ReadingData {
                    let bit = if self.eeprom_read_pos < 4 {
                        0 // 4 dummy bits
                    } else {
                        let idx = self.eeprom_read_pos - 4;
                        if idx < 64 {
                            ((self.eeprom_read_buffer >> (63 - idx)) & 1) as u8
                        } else {
                            0
                        }
                    };
                    self.eeprom_read_pos += 1;
                    if self.eeprom_read_pos >= 68 {
                        self.eeprom_state = EepromState::Idle;
                    }
                    bit
                } else {
                    1 // ready bit
                }
            }
            SaveType::None => 0xFF,
        }
    }

    pub fn write8(&mut self, addr: u32, val: u8) {
        match self.save_type {
            SaveType::Sram => {
                self.data[(addr as usize) & 0x7FFF] = val;
            }
            SaveType::Flash64 | SaveType::Flash128 => {
                self.flash_write_count += 1;
                self.flash_write(addr, val);
            }
            SaveType::Eeprom => {
                self.eeprom_write_bit(val & 1);
            }
            SaveType::None => {}
        }
    }

    fn eeprom_write_bit(&mut self, bit: u8) {
        match self.eeprom_state {
            EepromState::Idle | EepromState::ReadingCommand => {
                self.eeprom_command = (self.eeprom_command << 1) | bit as u64;
                self.eeprom_bits_written += 1;
                self.eeprom_state = EepromState::ReadingCommand;

                let addr_bits = match self.eeprom_size {
                    EepromSize::Small => 6,
                    EepromSize::Large => 14,
                };

                // Check if we have a complete command.
                if self.eeprom_bits_written == 2 + addr_bits {
                    let cmd = self.eeprom_command >> addr_bits;
                    let addr_mask = (1u64 << addr_bits) - 1;
                    self.eeprom_address = (self.eeprom_command & addr_mask) as u16;

                    match cmd & 3 {
                        3 => {
                            // Read: load 64 bits from address.
                            let byte_addr = self.eeprom_address as usize * 8;
                            let mut val = 0u64;
                            for i in 0..8 {
                                let idx = byte_addr + i;
                                if idx < self.data.len() {
                                    val = (val << 8) | self.data[idx] as u64;
                                } else {
                                    val <<= 8;
                                }
                            }
                            self.eeprom_read_buffer = val;
                            self.eeprom_read_pos = 0;
                            self.eeprom_state = EepromState::ReadingData;
                            self.eeprom_bits_written = 0;
                            self.eeprom_command = 0;
                        }
                        2 => {
                            // Write: prepare to receive 64 data bits.
                            self.eeprom_state = EepromState::WritingData;
                            self.eeprom_bits_written = 0;
                            self.eeprom_command = 0;
                        }
                        _ => {
                            // Unknown command, reset.
                            self.eeprom_state = EepromState::Idle;
                            self.eeprom_bits_written = 0;
                            self.eeprom_command = 0;
                        }
                    }
                }
            }
            EepromState::WritingData => {
                self.eeprom_command = (self.eeprom_command << 1) | bit as u64;
                self.eeprom_bits_written += 1;
                if self.eeprom_bits_written == 64 {
                    // Write 8 bytes to the address.
                    let byte_addr = self.eeprom_address as usize * 8;
                    for i in 0..8 {
                        let idx = byte_addr + i;
                        if idx < self.data.len() {
                            self.data[idx] = ((self.eeprom_command >> (56 - i * 8)) & 0xFF) as u8;
                        }
                    }
                    self.eeprom_state = EepromState::Idle;
                    self.eeprom_bits_written = 0;
                    self.eeprom_command = 0;
                }
            }
            EepromState::ReadingData => {
                // Ignore writes during read.
            }
        }
    }

    fn flash_write(&mut self, addr: u32, val: u8) {
        let local = addr & 0xFFFF;

        match self.flash_state {
            FlashState::Ready => {
                if local == 0x5555 && val == 0xAA {
                    self.flash_state = FlashState::Cmd1;
                }
            }
            FlashState::Cmd1 => {
                if local == 0x2AAA && val == 0x55 {
                    self.flash_state = FlashState::Cmd2;
                } else {
                    self.flash_state = FlashState::Ready;
                }
            }
            FlashState::Cmd2 => {
                // After erase prefix (0x80), the second AA/55/CMD comes through here
                // with the sector erase (0x30) or chip erase (0x10).
                if self.flash_erase_pending {
                    self.flash_erase_pending = false;
                    if val == 0x10 && local == 0x5555 {
                        // Chip erase.
                        let size = if self.save_type == SaveType::Flash128 { 128 * 1024 } else { 64 * 1024 };
                        self.data[..size].fill(0xFF);
                    } else if val == 0x30 {
                        // Sector erase (4K).
                        let sector = (local as usize >> 12) & 0xF;
                        let offset = self.flash_bank * 0x10000 + sector * 0x1000;
                        let end = (offset + 0x1000).min(self.data.len());
                        self.data[offset..end].fill(0xFF);
                    }
                    self.flash_state = FlashState::Ready;
                } else if local == 0x5555 {
                    match val {
                        0x90 => {
                            self.flash_id_mode = true;
                            self.flash_state = FlashState::Ready;
                        }
                        0xF0 => {
                            self.flash_id_mode = false;
                            self.flash_state = FlashState::Ready;
                        }
                        0x80 => {
                            // Erase command — set pending flag and wait for second prefix.
                            self.flash_erase_pending = true;
                            self.flash_state = FlashState::Erase;
                        }
                        0xA0 => {
                            // Single byte program.
                            self.flash_state = FlashState::Write;
                        }
                        0xB0 => {
                            // Bank select (Flash 128K).
                            if self.save_type == SaveType::Flash128 {
                                self.flash_state = FlashState::BankSelect;
                            } else {
                                self.flash_state = FlashState::Ready;
                            }
                        }
                        _ => {
                            self.flash_state = FlashState::Ready;
                        }
                    }
                } else {
                    self.flash_state = FlashState::Ready;
                }
            }
            FlashState::Erase => {
                // After 0x80, expect the second AA/55 prefix sequence.
                if local == 0x5555 && val == 0xAA {
                    self.flash_state = FlashState::Cmd1;
                } else {
                    self.flash_state = FlashState::Ready;
                    self.flash_erase_pending = false;
                }
            }
            FlashState::Write => {
                // Program a single byte (can only clear bits, not set them).
                let offset = self.flash_bank * 0x10000 + local as usize;
                if offset < self.data.len() {
                    self.data[offset] &= val;
                }
                self.flash_state = FlashState::Ready;
            }
            FlashState::BankSelect => {
                if local == 0 {
                    self.flash_bank = (val & 1) as usize;
                }
                self.flash_state = FlashState::Ready;
            }
            FlashState::IdMode => {
                if val == 0xF0 {
                    self.flash_id_mode = false;
                    self.flash_state = FlashState::Ready;
                }
            }
        }
    }
}
