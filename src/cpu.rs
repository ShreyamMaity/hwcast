//! CPU temperatures and package power through PawnIO, plus per-core load from
//! NtQuerySystemInformation.
//! Ryzen: SMN thermal registers + RAPL MSRs. Offsets and decoding follow
//! LibreHardwareMonitor's Amd17Cpu (MPL-2.0) and Linux k10temp.
//! Intel: IA32_PACKAGE_THERM_STATUS / IA32_TEMPERATURE_TARGET + RAPL MSRs, as in
//! LibreHardwareMonitor's IntelCpu and Linux coretemp / intel_rapl.

use crate::pawnio::{Module, NamedMutex, AMD17, INTEL_MSR};
use std::time::Instant;

const THM_TCON_CUR_TMP: u32 = 0x0005_9800;
const RANGE_SEL_MASK: u32 = 0x80000;
const TJ_SEL_MASK: u32 = 0x30000;
const ZEN4_CCD1_TEMP: u32 = 0x0005_9b08;
const ZEN2_CCD1_TEMP: u32 = 0x0005_9954;
const MSR_PWR_UNIT: u64 = 0xC001_0299;
const MSR_PKG_ENERGY: u64 = 0xC001_029B;

const MSR_IA32_TEMPERATURE_TARGET: u64 = 0x1A2;
const MSR_IA32_PACKAGE_THERM_STATUS: u64 = 0x1B1;
const MSR_RAPL_POWER_UNIT: u64 = 0x606;
const MSR_PKG_ENERGY_STATUS: u64 = 0x611;

/// CPUID vendor string, e.g. "GenuineIntel" or "AuthenticAMD".
pub fn cpu_vendor() -> String {
    use std::arch::x86_64::__cpuid;
    let r = __cpuid(0);
    let mut v = Vec::with_capacity(12);
    for x in [r.ebx, r.edx, r.ecx] {
        v.extend_from_slice(&x.to_le_bytes());
    }
    String::from_utf8_lossy(&v).into_owned()
}

pub fn cpu_id() -> (u32, u32, String) {
    use std::arch::x86_64::__cpuid;
    {
        let l1 = __cpuid(1);
        let base_fam = (l1.eax >> 8) & 0xF;
        let base_model = (l1.eax >> 4) & 0xF;
        let ext_fam = (l1.eax >> 20) & 0xFF;
        let ext_model = (l1.eax >> 16) & 0xF;
        let family = if base_fam == 0xF { base_fam + ext_fam } else { base_fam };
        let model = if base_fam >= 0xF { (ext_model << 4) | base_model } else { base_model };
        let mut name = Vec::new();
        for leaf in 0x8000_0002u32..=0x8000_0004 {
            let r = __cpuid(leaf);
            for v in [r.eax, r.ebx, r.ecx, r.edx] {
                name.extend_from_slice(&v.to_le_bytes());
            }
        }
        let name = String::from_utf8_lossy(&name).trim_matches(char::from(0)).trim().to_string();
        (family, model, name)
    }
}

/// One reading of the CPU package. `tctl` is AMD's Tctl, or the package
/// temperature on Intel; both are what the CPU uses for its own fan and
/// throttle decisions, so mods and the dashboard can treat them the same.
pub struct CpuReading {
    pub tctl: Option<f32>,
    pub ccds: Vec<f32>,
    pub package_w: Option<f32>,
}

/// Package power from a 32-bit RAPL energy counter (same layout on AMD and Intel).
struct Rapl {
    unit_j: f64,
    last: Option<(u32, Instant)>,
}

impl Rapl {
    fn new(power_unit_msr: Option<u64>) -> Self {
        let unit_j = power_unit_msr.map(|v| 1.0 / (1u64 << ((v >> 8) & 0x1F)) as f64).unwrap_or(0.0);
        Rapl { unit_j, last: None }
    }

    fn watts(&mut self, counter: Option<u64>) -> Option<f32> {
        if self.unit_j <= 0.0 {
            return None;
        }
        let e = counter? as u32;
        let now = Instant::now();
        let out = self.last.and_then(|(prev, at)| {
            let dt = now.duration_since(at).as_secs_f64();
            (dt > 0.05).then(|| (e.wrapping_sub(prev) as f64 * self.unit_j / dt) as f32)
        });
        self.last = Some((e, now));
        out
    }
}

/// Picks the right backend for the CPU in this PC.
pub enum CpuSensors {
    Amd(AmdSensors),
    Intel(IntelSensors),
}

impl CpuSensors {
    pub fn new() -> Result<Self, String> {
        match cpu_vendor().as_str() {
            "AuthenticAMD" | "HygonGenuine" => AmdSensors::new().map(CpuSensors::Amd),
            "GenuineIntel" => IntelSensors::new().map(CpuSensors::Intel),
            v => Err(format!("CPU vendor {v} not supported for temperatures")),
        }
    }

    /// `pci` is the cross-tool PCI bus lock; only the AMD SMN reads need it.
    pub fn read(&mut self, pci: Option<&NamedMutex>) -> CpuReading {
        match self {
            CpuSensors::Amd(a) => match pci {
                Some(p) => a.read(p),
                None => CpuReading { tctl: None, ccds: Vec::new(), package_w: None },
            },
            CpuSensors::Intel(i) => i.read(),
        }
    }
}

pub struct IntelSensors {
    m: Module,
    tj_max: f32,
    rapl: Rapl,
}

/// TjMax from IA32_TEMPERATURE_TARGET bits 23:16. Some CPUs and VMs report 0;
/// 100 C is what Intel uses on almost every desktop part since Sandy Bridge.
pub fn intel_tj_max(temp_target: Option<u64>) -> f32 {
    match temp_target.map(|v| ((v >> 16) & 0xFF) as f32) {
        Some(t) if t > 0.0 => t,
        _ => 100.0,
    }
}

/// IA32_PACKAGE_THERM_STATUS bits 22:16 hold how many degrees the package is
/// below TjMax.
pub fn intel_package_temp(therm_status: u64, tj_max: f32) -> f32 {
    tj_max - ((therm_status >> 16) & 0x7F) as f32
}

impl IntelSensors {
    pub fn new() -> Result<Self, String> {
        use std::arch::x86_64::__cpuid;
        // CPUID.06H:EAX bit 6 = package thermal management (IA32_PACKAGE_THERM_STATUS exists).
        if __cpuid(0).eax < 6 || __cpuid(6).eax & (1 << 6) == 0 {
            return Err("this Intel CPU has no package temperature sensor".into());
        }
        let m = Module::load(INTEL_MSR)?;
        let msr = |reg| m.exec("ioctl_read_msr", &[reg], 1).ok().map(|v| v[0]);
        let tj_max = intel_tj_max(msr(MSR_IA32_TEMPERATURE_TARGET));
        if msr(MSR_IA32_PACKAGE_THERM_STATUS).is_none() {
            return Err("cannot read the Intel package temperature MSR".into());
        }
        let rapl = Rapl::new(msr(MSR_RAPL_POWER_UNIT));
        Ok(IntelSensors { m, tj_max, rapl })
    }

    fn msr(&self, reg: u64) -> Option<u64> {
        self.m.exec("ioctl_read_msr", &[reg], 1).ok().map(|v| v[0])
    }

    pub fn read(&mut self) -> CpuReading {
        let tctl = self.msr(MSR_IA32_PACKAGE_THERM_STATUS).map(|v| intel_package_temp(v, self.tj_max));
        let energy = self.msr(MSR_PKG_ENERGY_STATUS);
        CpuReading { tctl, ccds: Vec::new(), package_w: self.rapl.watts(energy) }
    }
}

pub struct AmdSensors {
    m: Module,
    model: u32,
    rapl: Rapl,
}

impl AmdSensors {
    pub fn new() -> Result<Self, String> {
        let (family, model, _) = cpu_id();
        if !(0x17..=0x1A).contains(&family) {
            return Err(format!("CPU family 0x{family:X} not supported"));
        }
        let m = Module::load(AMD17)?;
        let rapl = Rapl::new(m.exec("ioctl_read_msr", &[MSR_PWR_UNIT], 1).ok().map(|v| v[0]));
        Ok(AmdSensors { m, model, rapl })
    }

    fn smn(&self, off: u32) -> Option<u32> {
        self.m.exec("ioctl_read_smn", &[off as u64], 1).ok().map(|v| v[0] as u32)
    }

    pub fn read(&mut self, pci: &NamedMutex) -> CpuReading {
        let mut out = CpuReading { tctl: None, ccds: Vec::new(), package_w: None };
        if let Some(_g) = pci.lock(10) {
            if let Some(raw) = self.smn(THM_TCON_CUR_TMP) {
                let mut t = ((raw >> 21) * 125) as f32 * 0.001;
                if raw & RANGE_SEL_MASK != 0 || raw & TJ_SEL_MASK == TJ_SEL_MASK {
                    t -= 49.0;
                }
                out.tctl = Some(t);
            }
            let ccd_base = if matches!(self.model, 0x61 | 0x44) { ZEN4_CCD1_TEMP } else { ZEN2_CCD1_TEMP };
            for i in 0..8 {
                if let Some(raw) = self.smn(ccd_base + i * 4) {
                    let raw = raw & 0xFFF;
                    let t = ((raw as i64 * 125) - 305_000) as f32 * 0.001;
                    if raw > 0 && t < 125.0 {
                        out.ccds.push(t);
                    }
                }
            }
        }
        let energy = self.m.exec("ioctl_read_msr", &[MSR_PKG_ENERGY], 1).ok().map(|v| v[0]);
        out.package_w = self.rapl.watts(energy);
        out
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ProcPerf {
    idle: i64,
    kernel: i64,
    user: i64,
    dpc: i64,
    interrupt: i64,
    interrupt_count: u32,
    _pad: u32,
}

#[link(name = "ntdll")]
extern "system" {
    pub fn NtQuerySystemInformation(class: u32, buf: *mut core::ffi::c_void, len: u32, ret: *mut u32) -> i32;
}

pub struct CoreLoad {
    prev: Vec<ProcPerf>,
}

impl CoreLoad {
    pub fn new() -> Self {
        CoreLoad { prev: Vec::new() }
    }

    fn query() -> Vec<ProcPerf> {
        let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(16);
        let mut v = vec![ProcPerf::default(); n.max(1) * 2];
        let mut ret = 0u32;
        let st = unsafe {
            NtQuerySystemInformation(
                8, // SystemProcessorPerformanceInformation
                v.as_mut_ptr() as _,
                (v.len() * std::mem::size_of::<ProcPerf>()) as u32,
                &mut ret,
            )
        };
        if st < 0 {
            return Vec::new();
        }
        v.truncate(ret as usize / std::mem::size_of::<ProcPerf>());
        v
    }

    /// Returns per-core busy % since the previous call.
    pub fn sample(&mut self) -> Vec<f32> {
        let cur = Self::query();
        let out = if self.prev.len() == cur.len() {
            cur.iter()
                .zip(&self.prev)
                .map(|(c, p)| {
                    // KernelTime includes idle time.
                    let total = (c.kernel - p.kernel) + (c.user - p.user);
                    let idle = c.idle - p.idle;
                    if total <= 0 {
                        0.0
                    } else {
                        (((total - idle) as f64 / total as f64) * 100.0).clamp(0.0, 100.0) as f32
                    }
                })
                .collect()
        } else {
            vec![0.0; cur.len()]
        };
        self.prev = cur;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intel_tj_max_decodes_bits_23_16() {
        // i7-13700F: TjMax 100 C, no TCC offset.
        assert_eq!(intel_tj_max(Some(0x0064_0000)), 100.0);
        // TjMax 105 with a TCC offset in bits 29:24 that must be ignored here.
        assert_eq!(intel_tj_max(Some(0x0569_0000)), 105.0);
        assert_eq!(intel_tj_max(Some(0)), 100.0);
        assert_eq!(intel_tj_max(None), 100.0);
    }

    #[test]
    fn intel_package_temp_is_tjmax_minus_readout() {
        // Readout 0x2A (42) below TjMax 100 -> 58 C. Bit 31 and status bits are ignored.
        assert_eq!(intel_package_temp(0x882A_0000 | 0x3, 100.0), 58.0);
        // Readout 0 means the package is at TjMax.
        assert_eq!(intel_package_temp(0, 100.0), 100.0);
    }

    #[test]
    fn rapl_unit_and_wraparound() {
        // ESU = 14 -> 1/16384 J per count (typical Intel desktop).
        let mut r = Rapl::new(Some(0x000A_0E03));
        assert!((r.unit_j - 1.0 / 16384.0).abs() < 1e-12);
        assert_eq!(r.watts(Some(u32::MAX as u64 - 100)), None);
        std::thread::sleep(std::time::Duration::from_millis(100));
        let w = r.watts(Some(16384 * 5 - 101)).unwrap();
        // ~5 J across the 32-bit wrap in ~0.1 s: roughly 50 W, never negative.
        assert!(w > 20.0 && w < 60.0, "{w}");
        assert_eq!(Rapl::new(None).watts(Some(1)), None);
    }
}
