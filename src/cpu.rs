//! Ryzen temperatures and package power through PawnIO (SMN + RAPL MSRs),
//! plus per-core load from NtQuerySystemInformation.
//! Register offsets and decoding follow LibreHardwareMonitor's Amd17Cpu (MPL-2.0)
//! and Linux k10temp.

use crate::pawnio::{Module, NamedMutex, AMD17};
use std::time::Instant;

const THM_TCON_CUR_TMP: u32 = 0x0005_9800;
const RANGE_SEL_MASK: u32 = 0x80000;
const TJ_SEL_MASK: u32 = 0x30000;
const ZEN4_CCD1_TEMP: u32 = 0x0005_9b08;
const ZEN2_CCD1_TEMP: u32 = 0x0005_9954;
const MSR_PWR_UNIT: u64 = 0xC001_0299;
const MSR_PKG_ENERGY: u64 = 0xC001_029B;

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

pub struct AmdSensors {
    m: Module,
    model: u32,
    energy_unit: f64,
    last_energy: Option<(u32, Instant)>,
}

pub struct AmdReading {
    pub tctl: Option<f32>,
    pub ccds: Vec<f32>,
    pub package_w: Option<f32>,
}

impl AmdSensors {
    pub fn new() -> Result<Self, String> {
        let (family, model, _) = cpu_id();
        if !(0x17..=0x1A).contains(&family) {
            return Err(format!("CPU family 0x{family:X} not supported"));
        }
        let m = Module::load(AMD17)?;
        let energy_unit = m
            .exec("ioctl_read_msr", &[MSR_PWR_UNIT], 1)
            .map(|v| 1.0 / (1u64 << ((v[0] >> 8) & 0x1F)) as f64)
            .unwrap_or(0.0);
        Ok(AmdSensors { m, model, energy_unit, last_energy: None })
    }

    fn smn(&self, off: u32) -> Option<u32> {
        self.m.exec("ioctl_read_smn", &[off as u64], 1).ok().map(|v| v[0] as u32)
    }

    pub fn read(&mut self, pci: &NamedMutex) -> AmdReading {
        let mut out = AmdReading { tctl: None, ccds: Vec::new(), package_w: None };
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
        if self.energy_unit > 0.0 {
            if let Ok(v) = self.m.exec("ioctl_read_msr", &[MSR_PKG_ENERGY], 1) {
                let e = v[0] as u32;
                let now = Instant::now();
                if let Some((prev, at)) = self.last_energy {
                    let dt = now.duration_since(at).as_secs_f64();
                    if dt > 0.05 {
                        let joules = e.wrapping_sub(prev) as f64 * self.energy_unit;
                        out.package_w = Some((joules / dt) as f32);
                    }
                }
                self.last_energy = Some((e, now));
            }
        }
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
