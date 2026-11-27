//! AMD Radeon GPUs through ADL (atiadlxx.dll, ships with the Radeon driver), using the
//! same PMLog sensor table LibreHardwareMonitor reads on Overdrive 8 cards (RX 5000 and
//! newer). Optional: if the DLL or a call is missing, no AMD GPU is reported.

use serde_json::{json, Value};
use std::ffi::c_void;
use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
use windows_sys::Win32::System::Memory::{GetProcessHeap, HeapAlloc};

type Ctx = *mut c_void;
type Malloc = unsafe extern "system" fn(i32) -> *mut c_void;
type Create = unsafe extern "C" fn(Malloc, i32, *mut Ctx) -> i32;
type Destroy = unsafe extern "C" fn(Ctx) -> i32;
type NumAdapters = unsafe extern "C" fn(Ctx, *mut i32) -> i32;
type AdapterInfoGet = unsafe extern "C" fn(Ctx, *mut AdapterInfo, i32) -> i32;
type AsicFamily = unsafe extern "C" fn(Ctx, i32, *mut i32, *mut i32) -> i32;
type PmLogGet = unsafe extern "C" fn(Ctx, i32, *mut PmLog) -> i32;
type MemInfoGet = unsafe extern "C" fn(Ctx, i32, *mut MemoryInfo) -> i32;
type VramUsage = unsafe extern "C" fn(Ctx, i32, *mut i32) -> i32;

// ADL reports AMD's PCI vendor 0x1002 as the decimal number 1002 (0x3EA).
const AMD_VENDOR: [i32; 2] = [1002, 0x1002];
const ASIC_DISCRETE: i32 = 1 << 0;

// ADLSensorType indices into the PMLog table (adl_defines.h).
const GFXCLK: usize = 1;
const TEMP_EDGE: usize = 8;
const TEMP_MEM: usize = 9;
const FAN_RPM: usize = 14;
const FAN_PCT: usize = 15;
const ACTIVITY_GFX: usize = 19;
const ACTIVITY_MEM: usize = 20;
const ASIC_POWER: usize = 23;
const TEMP_HOTSPOT: usize = 27;

#[repr(C)]
struct AdapterInfo {
    size: i32,
    index: i32,
    udid: [u8; 256],
    bus: i32,
    device: i32,
    function: i32,
    vendor: i32,
    name: [u8; 256],
    display_name: [u8; 256],
    present: i32,
    exist: i32,
    driver_path: [u8; 256],
    driver_path_ext: [u8; 256],
    pnp: [u8; 256],
    os_display_index: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Sensor {
    supported: i32,
    value: i32,
}

#[repr(C)]
struct PmLog {
    size: i32,
    sensors: [Sensor; 256],
}

#[repr(C)]
struct MemoryInfo {
    size: i64,
    kind: [u8; 256],
    bandwidth: i64,
}

unsafe extern "system" fn adl_malloc(size: i32) -> *mut c_void {
    // ADL hands this memory to the caller; none of the calls used here allocate,
    // so anything it does ask for lives on the process heap.
    HeapAlloc(GetProcessHeap(), 0, size.max(0) as usize)
}

struct Card {
    adapter: i32,
    name: String,
    vram_total_gb: Option<f64>,
}

pub struct Adl {
    dll: HMODULE,
    ctx: Ctx,
    destroy: Destroy,
    pmlog: PmLogGet,
    vram: Option<VramUsage>,
    cards: Vec<Card>,
}

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).trim().to_string()
}

impl Adl {
    /// `integrated` also lists APU graphics (off by default: on a desktop it mostly
    /// repeats the CPU temperature and clutters the dashboard).
    pub fn load(integrated: bool) -> Option<Self> {
        unsafe {
            let dll = LoadLibraryA(b"atiadlxx.dll\0".as_ptr());
            if dll.is_null() {
                return None;
            }
            let adl = Self::init(dll, integrated);
            if adl.is_none() {
                FreeLibrary(dll);
            }
            adl
        }
    }

    unsafe fn init(dll: HMODULE, integrated: bool) -> Option<Self> {
        macro_rules! sym {
            ($name:literal, $t:ty) => {
                GetProcAddress(dll, concat!($name, "\0").as_ptr()).map(|p| std::mem::transmute::<_, $t>(p))
            };
        }
        let create = sym!("ADL2_Main_Control_Create", Create)?;
        let destroy = sym!("ADL2_Main_Control_Destroy", Destroy)?;
        let num = sym!("ADL2_Adapter_NumberOfAdapters_Get", NumAdapters)?;
        let info = sym!("ADL2_Adapter_AdapterInfo_Get", AdapterInfoGet)?;
        let pmlog = sym!("ADL2_New_QueryPMLogData_Get", PmLogGet)?;
        let asic = sym!("ADL2_Adapter_ASICFamilyType_Get", AsicFamily);
        let meminfo = sym!("ADL2_Adapter_MemoryInfo_Get", MemInfoGet);
        let vram = sym!("ADL2_Adapter_DedicatedVRAMUsage_Get", VramUsage);

        let mut ctx: Ctx = std::ptr::null_mut();
        if create(adl_malloc, 1, &mut ctx) != 0 || ctx.is_null() {
            return None;
        }
        let mut adl = Adl { dll, ctx, destroy, pmlog, vram, cards: Vec::new() };
        let mut n = 0;
        if num(ctx, &mut n) != 0 || n <= 0 {
            return None; // Drop destroys the context and frees the DLL.
        }
        let mut list: Vec<AdapterInfo> = (0..n).map(|_| std::mem::zeroed()).collect();
        for a in list.iter_mut() {
            a.size = std::mem::size_of::<AdapterInfo>() as i32;
        }
        if info(ctx, list.as_mut_ptr(), (list.len() * std::mem::size_of::<AdapterInfo>()) as i32) != 0 {
            return None;
        }
        // ADL lists one adapter per display output; keep one per physical card (PCI bus).
        let mut seen = Vec::new();
        for a in &list {
            if !AMD_VENDOR.contains(&a.vendor) || a.present == 0 || seen.contains(&a.bus) {
                continue;
            }
            if let Some(asic) = asic {
                let (mut types, mut valid) = (0, 0);
                if asic(ctx, a.index, &mut types, &mut valid) == 0 && valid & ASIC_DISCRETE != 0 && types & ASIC_DISCRETE == 0 && !integrated {
                    continue;
                }
            }
            // The PMLog table exists on Overdrive 8 parts (RX 5000 / RDNA and newer). Some
            // report Overdrive as unsupported yet still serve it, so the read itself decides.
            let mut log: PmLog = std::mem::zeroed();
            log.size = std::mem::size_of::<PmLog>() as i32;
            if pmlog(ctx, a.index, &mut log) != 0 {
                continue;
            }
            seen.push(a.bus);
            let vram_total_gb = meminfo.and_then(|f| {
                let mut m: MemoryInfo = std::mem::zeroed();
                (f(ctx, a.index, &mut m) == 0 && m.size > 0).then(|| m.size as f64 / 1073741824.0)
            });
            let name = cstr(&a.name).replace("AMD Radeon(TM) ", "Radeon ").replace("AMD ", "");
            adl.cards.push(Card { adapter: a.index, name, vram_total_gb });
        }
        if adl.cards.is_empty() {
            return None;
        }
        Some(adl)
    }

    /// One JSON entry per card, in the same shape as the NVIDIA ones. `first_index`
    /// numbers AMD cards after any NVIDIA cards.
    pub fn read(&self, first_index: usize) -> Vec<Value> {
        self.cards
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let mut log: PmLog = unsafe { std::mem::zeroed() };
                log.size = std::mem::size_of::<PmLog>() as i32;
                let ok = unsafe { (self.pmlog)(self.ctx, c.adapter, &mut log) } == 0;
                let s = |k: usize| (ok && log.sensors[k].supported != 0).then_some(log.sensors[k].value);
                // Unsupported or idle-parked sensors can read 0; a 0 C temperature is never real.
                let temp = |k: usize| s(k).filter(|&v| v > 0 && v < 150);
                let vram_used_gb = self.vram.and_then(|f| {
                    let mut mb = 0;
                    (unsafe { f(self.ctx, c.adapter, &mut mb) } == 0 && mb >= 0).then(|| mb as f64 / 1024.0)
                });
                json!({
                    "index": first_index + i,
                    "vendor": "amd",
                    "name": c.name,
                    "temp": temp(TEMP_EDGE),
                    "hotspot": temp(TEMP_HOTSPOT),
                    "mem_temp": temp(TEMP_MEM),
                    "util": s(ACTIVITY_GFX),
                    "mem_util": s(ACTIVITY_MEM),
                    "vram_used_gb": vram_used_gb,
                    "vram_total_gb": c.vram_total_gb,
                    "power_w": s(ASIC_POWER),
                    "power_limit_w": Value::Null,
                    "fan_pct": s(FAN_PCT),
                    "fan_rpm": s(FAN_RPM).filter(|&v| v > 0),
                    "clock_mhz": s(GFXCLK),
                })
            })
            .collect()
    }
}

// The ADL context is only used from the sampler thread.
unsafe impl Send for Adl {}

impl Drop for Adl {
    fn drop(&mut self) {
        unsafe {
            (self.destroy)(self.ctx);
            FreeLibrary(self.dll);
        }
    }
}
