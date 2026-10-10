//! Minimal PawnIO client: talks to the signed PawnIO driver directly via
//! DeviceIoControl (same protocol LibreHardwareMonitor uses), so no DLL is needed.

use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    WAIT_ABANDONED, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::{
    InitializeSecurityDescriptor, SetSecurityDescriptorDacl, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Threading::{CreateMutexW, OpenMutexW, ReleaseMutex, WaitForSingleObject};
use windows_sys::Win32::System::IO::DeviceIoControl;

const DEVICE_TYPE: u32 = 41394 << 16;
const IOCTL_LOAD_BINARY: u32 = DEVICE_TYPE | (0x821 << 2);
const IOCTL_EXECUTE_FN: u32 = DEVICE_TYPE | (0x841 << 2);
const FN_NAME_LENGTH: usize = 32;

pub static LPCIO: &[u8] = include_bytes!("../vendor/mods/LpcIO.bin");
pub static AMD17: &[u8] = include_bytes!("../vendor/mods/AMDFamily17.bin");

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

pub struct Module {
    h: HANDLE,
}

unsafe impl Send for Module {}

impl Module {
    pub fn load(blob: &[u8]) -> Result<Self, String> {
        unsafe {
            let path = wide(r"\\?\GLOBALROOT\Device\PawnIO");
            let h = CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                null_mut(),
            );
            if h == INVALID_HANDLE_VALUE {
                return Err(format!("cannot open PawnIO (error {}; needs admin and the PawnIO driver)", GetLastError()));
            }
            let mut ret = 0u32;
            let ok = DeviceIoControl(
                h,
                IOCTL_LOAD_BINARY,
                blob.as_ptr() as _,
                blob.len() as u32,
                null_mut(),
                0,
                &mut ret,
                null_mut(),
            );
            if ok == 0 {
                let e = GetLastError();
                CloseHandle(h);
                return Err(format!("PawnIO module load failed (error {e})"));
            }
            Ok(Module { h })
        }
    }

    pub fn exec(&self, name: &str, input: &[u64], out_len: usize) -> Result<Vec<u64>, String> {
        let mut buf = vec![0u8; FN_NAME_LENGTH + input.len() * 8];
        let n = name.len().min(FN_NAME_LENGTH - 1);
        buf[..n].copy_from_slice(&name.as_bytes()[..n]);
        for (i, v) in input.iter().enumerate() {
            let o = FN_NAME_LENGTH + i * 8;
            buf[o..o + 8].copy_from_slice(&v.to_le_bytes());
        }
        let mut out = vec![0u64; out_len];
        let mut read = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                self.h,
                IOCTL_EXECUTE_FN,
                buf.as_ptr() as _,
                buf.len() as u32,
                out.as_mut_ptr() as _,
                (out_len * 8) as u32,
                &mut read,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(format!("{name} failed (error {})", unsafe { GetLastError() }));
        }
        Ok(out)
    }
}

impl Drop for Module {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.h) };
    }
}

/// Cross-tool bus lock. HWiNFO, LHM and Gigabyte's software all honour these
/// names, so holding one keeps our register reads from interleaving with theirs.
pub struct NamedMutex(HANDLE);

unsafe impl Send for NamedMutex {}
unsafe impl Sync for NamedMutex {}

pub struct MutexGuard<'a>(&'a NamedMutex);

impl NamedMutex {
    pub fn open(name: &str) -> Option<Self> {
        const SYNCHRONIZE: u32 = 0x0010_0000;
        const MUTEX_MODIFY_STATE: u32 = 0x0001;
        let w = wide(name);
        unsafe {
            let h = OpenMutexW(SYNCHRONIZE | MUTEX_MODIFY_STATE, 0, w.as_ptr());
            if !h.is_null() {
                return Some(NamedMutex(h));
            }
            // Create it with a null DACL so tools running as a normal user can still open it.
            let mut sd: SECURITY_DESCRIPTOR = std::mem::zeroed();
            InitializeSecurityDescriptor(&mut sd as *mut _ as _, 1);
            SetSecurityDescriptorDacl(&mut sd as *mut _ as _, 1, null(), 0);
            let sa = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: &mut sd as *mut _ as _,
                bInheritHandle: 0,
            };
            let h = CreateMutexW(&sa, 0, w.as_ptr());
            if h.is_null() {
                None
            } else {
                Some(NamedMutex(h))
            }
        }
    }

    pub fn lock(&self, timeout_ms: u32) -> Option<MutexGuard<'_>> {
        let r = unsafe { WaitForSingleObject(self.0, timeout_ms) };
        if r == WAIT_OBJECT_0 || r == WAIT_ABANDONED {
            Some(MutexGuard(self))
        } else {
            None
        }
    }
}

impl Drop for MutexGuard<'_> {
    fn drop(&mut self) {
        unsafe { ReleaseMutex(self.0 .0) };
    }
}
