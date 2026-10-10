//! Memory, per-process usage (one NtQuerySystemInformation call), and our own footprint.

use crate::cpu::NtQuerySystemInformation;
use std::collections::HashMap;
use std::time::Instant;
use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::ProcessStatus::{K32EmptyWorkingSet, K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

const GB: f64 = 1024.0 * 1024.0 * 1024.0;

pub struct Mem {
    pub used_gb: f64,
    pub total_gb: f64,
    pub commit_used_gb: f64,
    pub commit_total_gb: f64,
    pub pagefile_used_gb: f64,
    pub pagefile_size_gb: f64,
    pub pagefile_peak_gb: f64,
}

pub fn memory() -> Mem {
    let (pagefile_used_gb, pagefile_size_gb, pagefile_peak_gb) = pagefiles();
    unsafe {
        let mut m: MEMORYSTATUSEX = std::mem::zeroed();
        m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        GlobalMemoryStatusEx(&mut m);
        Mem {
            used_gb: (m.ullTotalPhys - m.ullAvailPhys) as f64 / GB,
            total_gb: m.ullTotalPhys as f64 / GB,
            commit_used_gb: (m.ullTotalPageFile - m.ullAvailPageFile) as f64 / GB,
            commit_total_gb: m.ullTotalPageFile as f64 / GB,
            pagefile_used_gb,
            pagefile_size_gb,
            pagefile_peak_gb,
        }
    }
}

/// Page file space actually written (in use / size / peak), summed over all page files.
/// SystemPageFileInformation, counts are in 4 KB pages.
fn pagefiles() -> (f64, f64, f64) {
    let mut buf = [0u8; 4096];
    let mut ret = 0u32;
    let st = unsafe { NtQuerySystemInformation(18, buf.as_mut_ptr() as _, buf.len() as u32, &mut ret) };
    if st < 0 || ret == 0 {
        return (0.0, 0.0, 0.0);
    }
    let rd = |o: usize| u32::from_le_bytes(buf[o..o + 4].try_into().unwrap()) as f64;
    let page = 4096.0 / GB;
    let (mut used, mut size, mut peak) = (0.0, 0.0, 0.0);
    let mut off = 0usize;
    while off + 32 <= ret as usize {
        size += rd(off + 4) * page;
        used += rd(off + 8) * page;
        peak += rd(off + 12) * page;
        let next = rd(off) as usize;
        if next == 0 {
            break;
        }
        off += next;
    }
    (used, size, peak)
}

pub struct ProcStat {
    pub name: String,
    pub count: u32,
    pub cpu_pct: f32,
    pub mem_gb: f64,
}

pub struct Procs {
    buf: Vec<u8>,
    prev: HashMap<usize, i64>,
    at: Option<Instant>,
    ncpu: f64,
}

impl Procs {
    pub fn new() -> Self {
        let ncpu = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f64;
        Procs { buf: Vec::new(), prev: HashMap::new(), at: None, ncpu }
    }

    pub fn release(&mut self) {
        self.buf = Vec::new();
        self.prev.clear();
        self.at = None;
    }

    /// Per-image totals (all chrome.exe summed, like Task Manager's grouping).
    pub fn sample(&mut self) -> Vec<ProcStat> {
        loop {
            let mut ret = 0u32;
            let st = unsafe {
                NtQuerySystemInformation(5, self.buf.as_mut_ptr() as _, self.buf.len() as u32, &mut ret)
            };
            if st == 0xC000_0004u32 as i32 {
                self.buf.resize((ret as usize).max(self.buf.len()).max(1 << 16) * 2, 0);
                continue;
            }
            if st < 0 {
                return Vec::new();
            }
            break;
        }
        let now = Instant::now();
        let elapsed_100ns = self.at.map(|a| now.duration_since(a).as_secs_f64() * 1e7).unwrap_or(0.0);
        let mut cur = HashMap::new();
        let mut groups: HashMap<String, ProcStat> = HashMap::new();
        let mut off = 0usize;
        let b = &self.buf;
        let rd_u32 = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let rd_i64 = |o: usize| i64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        let rd_usize = |o: usize| usize::from_le_bytes(b[o..o + 8].try_into().unwrap());
        loop {
            let next = rd_u32(off) as usize;
            let private_ws = rd_i64(off + 8);
            let cpu_time = rd_i64(off + 40) + rd_i64(off + 48);
            let name_len = u16::from_le_bytes(b[off + 56..off + 58].try_into().unwrap()) as usize;
            let name_ptr = rd_usize(off + 64) as *const u16;
            let pid = rd_usize(off + 80);
            let name = if pid == 0 {
                "System Idle".to_string()
            } else if name_ptr.is_null() {
                "?".to_string()
            } else {
                let s = unsafe { std::slice::from_raw_parts(name_ptr, name_len / 2) };
                String::from_utf16_lossy(s)
            };
            cur.insert(pid, cpu_time);
            if pid != 0 {
                let d = self.prev.get(&pid).map(|p| (cpu_time - p).max(0)).unwrap_or(0);
                let pct = if elapsed_100ns > 0.0 { d as f64 / elapsed_100ns / self.ncpu * 100.0 } else { 0.0 };
                let key = name.trim_end_matches(".exe").to_string();
                let g = groups.entry(key.clone()).or_insert(ProcStat { name: key, count: 0, cpu_pct: 0.0, mem_gb: 0.0 });
                g.count += 1;
                g.cpu_pct += pct as f32;
                g.mem_gb += private_ws as f64 / GB;
            }
            if next == 0 {
                break;
            }
            off += next;
        }
        self.prev = cur;
        self.at = Some(now);
        groups.into_values().collect()
    }
}

/// Our own working set (MB) and CPU time (seconds), to show what the monitor costs.
pub fn self_usage() -> (f64, f64) {
    unsafe {
        let h = GetCurrentProcess();
        let mut pmc: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        pmc.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        K32GetProcessMemoryInfo(h, &mut pmc, pmc.cb);
        let z = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let (mut c, mut e, mut k, mut u) = (z, z, z, z);
        GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u);
        let ft = |f: FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) as f64 / 1e7;
        (pmc.WorkingSetSize as f64 / (1024.0 * 1024.0), ft(k) + ft(u))
    }
}

/// Hand our pages back to Windows while idle.
pub fn trim() {
    unsafe { K32EmptyWorkingSet(GetCurrentProcess()) };
}
