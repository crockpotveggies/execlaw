//! Phase 14 follow-up — accurate VRAM detection.
//!
//! `hardware-query` 0.2.1 has two known issues that affect the
//! BackendWizard's "filter models by VRAM" UX:
//!
//!   1. The NVIDIA merge step in `GPUInfo::query_all` copies CUDA
//!      capabilities, utilization, temperature, etc. from NVML
//!      back onto the WMI-sourced row — but it forgets to copy
//!      `memory_mb`. The result: NVIDIA cards report whatever
//!      `Win32_VideoController.AdapterRAM` returned (UINT32 →
//!      capped at 4 GB).
//!   2. Intel's path reads the same `AdapterRAM` field, which is
//!      both UINT32-capped AND under-reports for Arc cards (some
//!      drivers expose only a partial value, often 2 GB even on
//!      a 12 GB B580).
//!
//! This module overrides the buggy values with vendor-specific
//! probes:
//!
//!   * NVIDIA → NVML directly (`device.memory_info().total`,
//!     correct in bytes regardless of the >4 GB ceiling).
//!   * Intel  → Windows registry `HardwareInformation.qwMemorySize`
//!     (64-bit, accurate, what the modern Display Properties UI
//!     reads). Linux/macOS: trust hardware-query's sysfs/IOKit
//!     readout, which is correct in the upstream paths.
//!
//! All probes are best-effort — failures fall back to the
//! hardware-query value so the wizard still works on hosts where
//! NVML can't init or the registry lookup is blocked.

use tracing::debug;

/// Override `hardware-query`'s NVIDIA `memory_mb` with NVML's
/// authoritative read. Returns an empty Vec if NVML can't init
/// (Linux + no nvidia driver, Windows missing nvidia-ml.dll,
/// etc.) so callers fall back to the original value.
pub fn nvidia_memory_mb_via_nvml() -> Vec<u64> {
    let Ok(nvml) = nvml_wrapper::Nvml::init() else {
        debug!("NVML init failed; will fall back to hardware-query memory_mb");
        return Vec::new();
    };
    let device_count = nvml.device_count().unwrap_or(0);
    let mut out = Vec::with_capacity(device_count as usize);
    for i in 0..device_count {
        let Ok(device) = nvml.device_by_index(i) else {
            continue;
        };
        let Ok(mem) = device.memory_info() else {
            continue;
        };
        out.push(mem.total / 1024 / 1024);
    }
    out
}

/// Override Intel Arc's `memory_mb` with the Windows registry's
/// 64-bit `qwMemorySize`. Linux + macOS callers should not invoke
/// this — sysfs/IOKit return correct values in the upstream paths.
///
/// The registry layout we read:
///
/// ```
/// HKLM\SYSTEM\CurrentControlSet\Control\Class\
///     {4d36e968-e325-11ce-bfc1-08002be10318}\<index>\
///     HardwareInformation.qwMemorySize        REG_QWORD (bytes)
///     DriverDesc                              REG_SZ   (e.g. "Intel(R) Arc(TM) B580 Graphics")
/// ```
///
/// Returns one entry per Intel adapter found, paired with the
/// `DriverDesc` string so callers can match it back to a
/// `model_name` reported by hardware-query.
#[cfg(windows)]
pub fn intel_memory_mb_via_registry() -> Vec<(String, u64)> {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;

    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let display_class_path = r"SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}";
    let Ok(class_key) = hklm.open_subkey(display_class_path) else {
        debug!("display class registry key not found; skipping intel VRAM override");
        return Vec::new();
    };

    let mut out = Vec::new();
    // Subkeys are 4-digit indices: 0000, 0001, 0002, …
    for sub in class_key.enum_keys().flatten() {
        let Ok(adapter_key) = class_key.open_subkey(&sub) else {
            continue;
        };
        let driver_desc: String = adapter_key
            .get_value("DriverDesc")
            .unwrap_or_default();
        if driver_desc.is_empty() {
            continue;
        }
        // Some adapters (Microsoft Basic Display, virtual ones) expose
        // no qwMemorySize. Try the QWORD first, fall back to the older
        // DWORD `MemorySize` if present.
        let bytes: u64 = adapter_key
            .get_value("HardwareInformation.qwMemorySize")
            .ok()
            .or_else(|| {
                adapter_key
                    .get_value::<u32, _>("HardwareInformation.MemorySize")
                    .ok()
                    .map(|v| v as u64)
            })
            .unwrap_or(0);
        if bytes == 0 {
            continue;
        }
        out.push((driver_desc, bytes / 1024 / 1024));
    }
    out
}

#[cfg(not(windows))]
pub fn intel_memory_mb_via_registry() -> Vec<(String, u64)> {
    // Linux + macOS read VRAM correctly through hardware-query's
    // sysfs / IOKit paths. No override needed.
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvml_probe_returns_empty_or_some_vec() {
        // We can't assume an NVIDIA GPU is present on every CI
        // host — just smoke that the function doesn't panic and
        // returns a Vec (possibly empty).
        let v = nvidia_memory_mb_via_nvml();
        let _ = v.len();
    }

    #[test]
    fn intel_registry_probe_returns_well_formed_pairs() {
        let pairs = intel_memory_mb_via_registry();
        for (desc, mb) in pairs {
            assert!(!desc.is_empty());
            // 4 GB+ is plausible for any modern adapter; stale
            // virtual displays might report 0 (filtered above).
            assert!(mb >= 64, "implausible memory_mb={mb} for {desc}");
        }
    }
}
