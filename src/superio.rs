//! ITE Super I/O environment controllers (common on Gigabyte boards).
//! Register layout and per-chip channel counts follow LibreHardwareMonitor's
//! IT87XX driver (MPL-2.0), https://github.com/LibreHardwareMonitor/LibreHardwareMonitor

use crate::pawnio::{Module, NamedMutex, LPCIO};
use std::thread::sleep;
use std::time::Duration;

const CHIP_ID_REG: u8 = 0x20;
const LDN_SELECT_REG: u8 = 0x07;
const BASE_ADDR_REG: u8 = 0x60;
const ENV_CONTROLLER_LDN: u8 = 0x04;
const TEMP_BASE_REG: u8 = 0x29;
const FAN_REG: [u8; 6] = [0x0d, 0x0e, 0x0f, 0x80, 0x82, 0x4c];
const FAN_EXT_REG: [u8; 6] = [0x18, 0x19, 0x1a, 0x81, 0x83, 0x4d];

pub struct Chip {
    pub key: &'static str,
    m: Module,
    addr: u16,
    temps: usize,
    fans: usize,
    fan_disabled: [bool; 6],
}

pub struct Reading {
    pub temps: Vec<Option<f32>>,
    pub fans: Vec<Option<f32>>,
}

impl Chip {
    fn outb(&self, port: u16, v: u8) -> Result<(), String> {
        self.m.exec("ioctl_pio_outb", &[port as u64, v as u64], 0).map(|_| ())
    }
    fn inb(&self, port: u16) -> Result<u8, String> {
        Ok(self.m.exec("ioctl_pio_inb", &[port as u64], 1)?[0] as u8)
    }
    fn reg(&self, r: u8) -> Option<u8> {
        self.outb(self.addr + 5, r).ok()?;
        let v = self.inb(self.addr + 6).ok()?;
        (self.inb(self.addr + 5).ok()? == r).then_some(v)
    }

    /// Fans 4-6 can be switched out of 16-bit counter mode; those read garbage.
    fn init_fan_mask(&mut self) {
        if let Some(modes) = self.reg(0x0C) {
            if self.fans >= 5 {
                self.fan_disabled[3] = modes & (1 << 4) == 0;
                self.fan_disabled[4] = modes & (1 << 5) == 0;
            }
            if self.fans >= 6 {
                self.fan_disabled[5] = modes & (1 << 2) == 0;
            }
        }
    }

    pub fn read(&self) -> Reading {
        let temps = (0..self.temps)
            .map(|i| {
                self.reg(TEMP_BASE_REG + i as u8).and_then(|v| {
                    let t = v as i8;
                    (t > 0 && t < 127).then_some(t as f32)
                })
            })
            .collect();
        let fans = (0..self.fans)
            .map(|i| {
                if self.fan_disabled[i] {
                    return None;
                }
                let lo = self.reg(FAN_REG[i])? as u32;
                let hi = self.reg(FAN_EXT_REG[i])? as u32;
                let v = lo | (hi << 8);
                if v <= 0x3f {
                    None
                } else if v >= 0xffff {
                    Some(0.0)
                } else {
                    Some(1.35e6 / (v as f32 * 2.0))
                }
            })
            .collect();
        Reading { temps, fans }
    }
}

fn sio_inw(m: &Module, r: u8) -> Result<u16, String> {
    Ok(m.exec("ioctl_superio_inw", &[r as u64], 1)?[0] as u16)
}

fn sio_outb(m: &Module, r: u8, v: u8) -> Result<(), String> {
    m.exec("ioctl_superio_outb", &[r as u64, v as u64], 0).map(|_| ())
}

fn pio_outb(m: &Module, port: u16, v: u8) -> Result<(), String> {
    m.exec("ioctl_pio_outb", &[port as u64, v as u64], 0).map(|_| ())
}

pub fn detect(isa: &NamedMutex) -> (Vec<Chip>, Vec<String>) {
    let mut chips = Vec::new();
    let mut log = Vec::new();
    for (slot, port) in [(0u64, 0x2Eu16), (1, 0x4E)] {
        let m = match Module::load(LPCIO) {
            Ok(m) => m,
            Err(e) => {
                log.push(e);
                break;
            }
        };
        let Some(_g) = isa.lock(250) else {
            log.push(format!("ISA bus busy, skipped 0x{port:X}"));
            continue;
        };
        let r = (|| -> Result<Option<Chip>, String> {
            m.exec("ioctl_select_slot", &[slot], 0)?;
            // A second ITE chip on 0x4E is often left in config mode; re-entering it corrupts its ID.
            let mut id = if port == 0x4E { sio_inw(&m, CHIP_ID_REG)? } else { 0xFFFF };
            if port != 0x4E || id == 0xFFFF {
                for b in [0x87, 0x01, 0x55, if port == 0x4E { 0xAA } else { 0x55 }] {
                    pio_outb(&m, port, b)?;
                }
                id = sio_inw(&m, CHIP_ID_REG)?;
            }
            // ITE chips without bank switching: (id, key, temperature channels, fan channels)
            let spec = match id {
                0x8613 => Some(("it8613e", 4, 5)),
                0x8628 => Some(("it8628e", 6, 6)),
                0x8686 => Some(("it8686e", 7, 6)),
                0x8688 => Some(("it8688e", 6, 6)),
                0x8689 => Some(("it8689e", 6, 6)),
                0x8696 => Some(("it8696e", 6, 6)),
                0x8695 => Some(("it87952e", 3, 3)),
                0x8733 => Some(("it8792e", 3, 3)),
                _ => None,
            };
            let Some((key, temps, fans)) = spec else {
                if port != 0x4E {
                    pio_outb(&m, port, 0x02)?;
                    pio_outb(&m, port + 1, 0x02)?;
                }
                if id != 0 && id != 0xFFFF {
                    log.push(format!("unsupported Super I/O 0x{id:04X} at 0x{port:X}"));
                }
                return Ok(None);
            };
            m.exec("ioctl_find_bars", &[], 0)?;
            sio_outb(&m, LDN_SELECT_REG, ENV_CONTROLLER_LDN)?;
            let addr = sio_inw(&m, BASE_ADDR_REG)?;
            sleep(Duration::from_millis(1));
            let verify = sio_inw(&m, BASE_ADDR_REG)?;
            if port != 0x4E {
                pio_outb(&m, port, 0x02)?;
                pio_outb(&m, port + 1, 0x02)?;
            }
            if addr != verify || addr < 0x100 || addr & 0xF007 != 0 {
                return Err(format!("{key}: bad base address 0x{addr:X}/0x{verify:X}"));
            }
            log.push(format!("{key} at 0x{port:X}, base 0x{addr:X}"));
            let mut chip = Chip { key, m, addr, temps, fans, fan_disabled: [false; 6] };
            chip.init_fan_mask();
            Ok(Some(chip))
        })();
        match r {
            Ok(Some(c)) => chips.push(c),
            Ok(None) => {}
            Err(e) => log.push(e),
        }
    }
    (chips, log)
}
