//! NVIDIA GPUs through NVML (ships with the driver). Handles more than one card.
//! Hotspot and memory junction temperatures come from NVAPI when available (see nvapi.rs).

use crate::nvapi::NvApi;
use nvml_wrapper::enum_wrappers::device::{Clock, TemperatureSensor};
use nvml_wrapper::Nvml;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

pub struct Gpus {
    nvml: Option<Nvml>,
    nvapi: Option<NvApi>,
    retry_at: Instant,
}

impl Gpus {
    pub fn new() -> Self {
        Gpus { nvml: None, nvapi: None, retry_at: Instant::now() }
    }

    /// Unload NVML and NVAPI (and the driver DLLs they pull in) while nobody is watching.
    pub fn release(&mut self) {
        self.nvml = None;
        self.nvapi = None;
        self.retry_at = Instant::now();
    }

    pub fn read(&mut self) -> Vec<Value> {
        if self.nvml.is_none() && Instant::now() >= self.retry_at {
            self.nvml = Nvml::init().ok();
            self.nvapi = self.nvml.as_ref().and_then(|_| NvApi::load());
            self.retry_at = Instant::now() + Duration::from_secs(60);
        }
        let Some(nvml) = &self.nvml else { return Vec::new() };
        let n = nvml.device_count().unwrap_or(0);
        (0..n)
            .filter_map(|i| nvml.device_by_index(i).ok().map(|d| (i, d)))
            .map(|(i, d)| {
                let util = d.utilization_rates().ok();
                let mem = d.memory_info().ok();
                // Some cards report more fan channels than have a fan on them (the unused
                // ones read 0), so show the fastest channel rather than always channel 0.
                let fans = d.num_fans().unwrap_or(1).max(1);
                let fan_pct = (0..fans).filter_map(|f| d.fan_speed(f).ok()).max();
                let fan_rpm = (0..fans).filter_map(|f| d.fan_speed_rpm(f).ok()).max();
                let extra = match (&self.nvapi, d.pci_info()) {
                    (Some(api), Ok(pci)) => api.read(pci.bus),
                    _ => Default::default(),
                };
                json!({
                    "index": i,
                    "name": d.name().unwrap_or_default().replace("NVIDIA GeForce ", ""),
                    "temp": d.temperature(TemperatureSensor::Gpu).ok(),
                    "hotspot": extra.hotspot,
                    "mem_temp": extra.mem,
                    "util": util.as_ref().map(|u| u.gpu),
                    "mem_util": util.as_ref().map(|u| u.memory),
                    "vram_used_gb": mem.as_ref().map(|m| m.used as f64 / 1073741824.0),
                    "vram_total_gb": mem.as_ref().map(|m| m.total as f64 / 1073741824.0),
                    "power_w": d.power_usage().ok().map(|p| p as f64 / 1000.0),
                    "power_limit_w": d.enforced_power_limit().ok().map(|p| p as f64 / 1000.0),
                    "fan_pct": fan_pct,
                    "fan_rpm": fan_rpm,
                    "clock_mhz": d.clock_info(Clock::Graphics).ok(),
                })
            })
            .collect()
    }
}
