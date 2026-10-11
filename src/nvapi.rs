//! GPU hotspot and memory junction temperatures through NVAPI's undocumented
//! NvAPI_GPU_ThermalGetSensors, the same call LibreHardwareMonitor uses. NVML does not
//! expose either. Optional: if nvapi64.dll or the call is missing, nothing is reported.

use std::collections::HashMap;
use std::ffi::c_void;
use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};

type QueryInterface = unsafe extern "C" fn(u32) -> *mut c_void;
type Initialize = unsafe extern "C" fn() -> i32;
type Unload = unsafe extern "C" fn() -> i32;
type EnumGpus = unsafe extern "C" fn(*mut *mut c_void, *mut u32) -> i32;
type GetBusId = unsafe extern "C" fn(*mut c_void, *mut u32) -> i32;
type ThermalGetSensors = unsafe extern "C" fn(*mut c_void, *mut Sensors) -> i32;

#[repr(C)]
struct Sensors {
    version: u32,
    mask: u32,
    reserved: [i32; 8],
    temps: [i32; 32],
}

impl Sensors {
    fn new(mask: u32) -> Self {
        Sensors { version: std::mem::size_of::<Sensors>() as u32 | (2 << 16), mask, reserved: [0; 8], temps: [0; 32] }
    }
}

#[derive(Clone, Copy, Default)]
pub struct Extra {
    pub hotspot: Option<f64>,
    pub mem: Option<f64>,
}

pub struct NvApi {
    dll: HMODULE,
    unload: Option<Unload>,
    therm: ThermalGetSensors,
    /// PCI bus number -> (GPU handle, widest sensor mask the GPU accepts)
    gpus: HashMap<u32, (*mut c_void, u32)>,
}

impl NvApi {
    pub fn load() -> Option<Self> {
        unsafe {
            let dll = LoadLibraryA(b"nvapi64.dll\0".as_ptr());
            if dll.is_null() {
                return None;
            }
            let api = Self::init(dll);
            if api.is_none() {
                FreeLibrary(dll);
            }
            api
        }
    }

    unsafe fn init(dll: HMODULE) -> Option<Self> {
        let q: QueryInterface = std::mem::transmute(GetProcAddress(dll, b"nvapi_QueryInterface\0".as_ptr())?);
        let f = |id: u32| {
            let p = q(id);
            (!p.is_null()).then_some(p)
        };
        let initialize: Initialize = std::mem::transmute(f(0x0150_E828)?);
        if initialize() != 0 {
            return None;
        }
        let unload = f(0xD22B_DD7E).map(|p| std::mem::transmute::<_, Unload>(p));
        let enum_gpus: EnumGpus = std::mem::transmute(f(0xE5AC_921F)?);
        let bus_id: GetBusId = std::mem::transmute(f(0x1BE0_B8E5)?);
        let therm: ThermalGetSensors = std::mem::transmute(f(0x65FE_3AAD)?);

        let mut handles = [std::ptr::null_mut(); 64];
        let mut n = 0u32;
        if enum_gpus(handles.as_mut_ptr(), &mut n) != 0 {
            return None;
        }
        let mut gpus = HashMap::new();
        for &h in &handles[..n as usize] {
            let mut bus = 0u32;
            if bus_id(h, &mut bus) != 0 {
                continue;
            }
            // Older GPUs reject masks that name sensors they lack, so find the widest one accepted.
            let mask = (1..=32u32).rev().map(|b| if b == 32 { u32::MAX } else { (1 << b) - 1 }).find(|&m| {
                let mut s = Sensors::new(m);
                therm(h, &mut s) == 0
            });
            if let Some(m) = mask {
                gpus.insert(bus, (h, m));
            }
        }
        if gpus.is_empty() {
            if let Some(u) = unload {
                u();
            }
            return None;
        }
        Some(NvApi { dll, unload, therm, gpus })
    }

    /// Hotspot and memory junction temperatures for the GPU on this PCI bus.
    pub fn read(&self, bus: u32) -> Extra {
        let Some(&(h, mask)) = self.gpus.get(&bus) else { return Extra::default() };
        let mut s = Sensors::new(mask);
        if unsafe { (self.therm)(h, &mut s) } != 0 {
            return Extra::default();
        }
        // Values are 1/256 degrees C. Unsupported slots read 0 or 255 C (e.g. GDDR7 memory).
        let t = |i: usize| {
            let v = s.temps[i] as f64 / 256.0;
            (v > 0.0 && v < 200.0).then_some((v * 10.0).round() / 10.0)
        };
        Extra { hotspot: t(1), mem: t(9) }
    }
}

// NVAPI handles are opaque driver tokens; the sampler owns this and uses it from one thread at a time.
unsafe impl Send for NvApi {}

impl Drop for NvApi {
    fn drop(&mut self) {
        unsafe {
            if let Some(u) = self.unload {
                u();
            }
            FreeLibrary(self.dll);
        }
    }
}
