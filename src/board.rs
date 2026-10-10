//! Motherboard identity (from the registry, no WMI) and sensor label profiles.
//!
//! Super I/O chips expose numbered channels; which header each channel belongs to
//! depends on the board. Known boards get friendly names here, everything else falls
//! back to "<chip> temp N", and config.json can override any label.

use std::collections::HashMap;
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};

pub struct Board {
    pub manufacturer: String,
    pub product: String,
}

fn reg_string(name: &str) -> String {
    let key: Vec<u16> = r"HARDWARE\DESCRIPTION\System\BIOS".encode_utf16().chain(Some(0)).collect();
    let val: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let mut buf = [0u16; 256];
    let mut len = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            val.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr() as _,
            &mut len,
        )
    };
    if r != 0 {
        return String::new();
    }
    let n = (len as usize / 2).saturating_sub(1);
    String::from_utf16_lossy(&buf[..n]).trim().to_string()
}

pub fn detect() -> Board {
    Board { manufacturer: reg_string("BaseBoardManufacturer"), product: reg_string("BaseBoardProduct") }
}

pub struct Profile {
    pub name: &'static str,
    pub labels: HashMap<String, String>,
    /// Channels wired to user-placed thermistor probes, shown as big tiles.
    pub probes: Vec<String>,
    /// Board channel that mirrors CPU temperature, shown on the CPU card.
    pub socket: Option<String>,
}

fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

/// Picks a label profile from the board vendor and the chips that were found.
/// Mappings follow LibreHardwareMonitor's board tables.
pub fn profile(board: &Board, chips: &[&str]) -> Profile {
    let gigabyte = board.manufacturer.to_ascii_lowercase().contains("gigabyte");
    if gigabyte && chips.contains(&"it8696e") {
        // Gigabyte AM5 (X870 / X870E / B850) with IT8696E, plus IT87952E on larger boards.
        // Matches LHM's X870E AORUS PRO / XTREME AI TOP tables (same chip pair and channel layout).
        return Profile {
            name: "Gigabyte AM5 (IT8696E + IT87952E)",
            labels: map(&[
                ("it8696e.t0", "System"),
                ("it8696e.t1", "Chipset"),
                ("it8696e.t2", "CPU socket"),
                ("it8696e.t3", "PCIe x16"),
                ("it8696e.t4", "VRM"),
                ("it8696e.t5", "External 1"),
                ("it87952e.t0", "PCIe x4"),
                ("it87952e.t1", "External 2"),
                ("it87952e.t2", "System 2"),
                ("it8696e.f0", "CPU fan"),
                ("it8696e.f1", "Sys fan 1"),
                ("it8696e.f2", "Sys fan 2"),
                ("it8696e.f3", "Sys fan 3"),
                ("it8696e.f4", "CPU opt"),
                ("it8696e.f5", "Fan 6"),
                ("it87952e.f0", "Sys fan 5 / pump"),
                ("it87952e.f1", "Sys fan 6 / pump"),
                ("it87952e.f2", "Sys fan 4"),
            ]),
            probes: vec!["it8696e.t5".into(), "it87952e.t1".into()],
            socket: Some("it8696e.t2".into()),
        };
    }
    Profile { name: "generic", labels: HashMap::new(), probes: Vec::new(), socket: None }
}

pub fn generic_label(id: &str) -> String {
    // "it8696e.t3" -> "IT8696E temp 4", "it8696e.f0" -> "IT8696E fan 1"
    let (chip, ch) = id.split_once('.').unwrap_or((id, ""));
    let (kind, n) = ch.split_at(1.min(ch.len()));
    let n: usize = n.parse().unwrap_or(0) + 1;
    let kind = if kind == "f" { "fan" } else { "temp" };
    format!("{} {kind} {n}", chip.to_ascii_uppercase())
}
