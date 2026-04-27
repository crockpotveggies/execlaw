//! Hardware profile types + Tier 1 detection (§5.3).
//!
//! Phase 14 bare-metal pivot — production code calls [`detect`], which
//! delegates to [`hardware_query::HardwareInfo::query`] for native
//! Windows (WMI), Linux (sysfs), and macOS (IOKit) probing. The
//! [`detect_sysfs`] helper is preserved for unit tests that build a
//! mock `/sys` tree in a temp directory and assert on the parser
//! shape — exercising a real WMI / IOKit fixture isn't worth the
//! cross-platform CI complexity.
//!
//! Both paths return the same [`HardwareProfile`] shape so consumers
//! (`presets_handler`, `admin_hardware` route) don't have to branch
//! on platform.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Stable per-GPU identifier. We use PCI BDF when available, fallback to
/// the kernel card index.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GpuId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GpuVendor {
    Nvidia,
    Intel,
    Amd,
    Unknown,
}

impl GpuVendor {
    pub fn from_pci_vendor(hex: &str) -> Self {
        // sysfs returns `0x10de`, `0x8086`, etc.
        match hex.trim().to_ascii_lowercase().trim_start_matches("0x") {
            "10de" => GpuVendor::Nvidia,
            "8086" => GpuVendor::Intel,
            "1002" => GpuVendor::Amd,
            _ => GpuVendor::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GpuDevice {
    pub id: GpuId,
    pub vendor: GpuVendor,
    pub pci_vendor_id: String,
    pub pci_device_id: String,
    pub device_files: Vec<PathBuf>,
    pub kernel_card_index: u32,
}

/// Where the hardware data came from. Pre-Phase-14 this was always
/// sysfs; the bare-metal pivot adds WMI (Windows) and IOKit (macOS)
/// behind `hardware-query`. Tests still reach `detect_sysfs` directly,
/// which always reports `Sysfs` regardless of platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SysfsSource {
    /// Linux sysfs walk (`/sys/class/drm/card*`).
    Sysfs,
    /// Cross-platform `hardware-query` crate (WMI / sysfs / IOKit).
    HardwareQuery,
    /// Test fixture pointed at a mock sysfs tree.
    Mock,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HardwareProfile {
    pub gpus: Vec<GpuDevice>,
    pub source: SysfsSource,
}

// ---------------------------------------------------------------------------
// Production detection — Phase 14 bare-metal pivot.
// ---------------------------------------------------------------------------

/// Cross-platform GPU detection. Production callers (the
/// `presets_handler`, the `admin_hardware` route, the
/// `BackendSupervisor` startup) use this; tests use [`detect_sysfs`]
/// with a mock root.
///
/// Linux: `hardware-query` walks `/sys/class/drm` + `/proc/driver/nvidia`
/// internally. Windows: WMI's `Win32_VideoController` query.
/// macOS: IOKit + `system_profiler` shell-out.
///
/// Failures are downgraded to "no GPU detected" rather than propagated
/// — the control plane runs fine on GPU-less hosts and on hosts where
/// a query times out (e.g. WMI service blocked); operators just see
/// "CPU only" in the Backend wizard until they fix the underlying
/// issue. The error is logged via `tracing::warn!`.
pub fn detect() -> HardwareProfile {
    match hardware_query::HardwareInfo::query() {
        Ok(hw) => HardwareProfile {
            gpus: hw.gpus().iter().map(GpuDevice::from_query).collect(),
            source: SysfsSource::HardwareQuery,
        },
        Err(e) => {
            tracing::warn!(
                "hardware-query failed: {e}; reporting no GPUs (CPU-only fallback)"
            );
            HardwareProfile {
                gpus: Vec::new(),
                source: SysfsSource::HardwareQuery,
            }
        }
    }
}

impl GpuDevice {
    /// Adapt a `hardware_query::GPUInfo` into our internal shape.
    /// `hardware-query` doesn't expose a separate PCI vendor id (only
    /// the device id, as a string). We synthesize the vendor hex from
    /// its `GPUVendor` enum — sufficient for our wizard's vendor-match
    /// logic. The kernel card index isn't meaningful on Windows or
    /// macOS, so we use the slot index in the returned vector.
    fn from_query(g: &hardware_query::GPUInfo) -> Self {
        let (vendor, vendor_hex) = match &g.vendor {
            hardware_query::GPUVendor::NVIDIA => (GpuVendor::Nvidia, "0x10de"),
            hardware_query::GPUVendor::Intel => (GpuVendor::Intel, "0x8086"),
            hardware_query::GPUVendor::AMD => (GpuVendor::Amd, "0x1002"),
            // Apple Silicon, ARM, Qualcomm, and unrecognized vendors
            // all report as Unknown — the preset library has no path
            // for them today, so the wizard recommends CPU which is
            // the truth until those backends ship.
            _ => (GpuVendor::Unknown, "0x0000"),
        };
        let device_hex = g
            .pci_device_id
            .clone()
            .unwrap_or_else(|| "0x0000".to_owned());
        let id = GpuId(format!(
            "{}:{}",
            vendor_hex,
            // Use model_name when device id is unavailable so two
            // GPUs from the same vendor still get distinct ids.
            g.pci_device_id
                .clone()
                .unwrap_or_else(|| g.model_name.clone())
        ));
        Self {
            id,
            vendor,
            pci_vendor_id: vendor_hex.to_owned(),
            pci_device_id: device_hex,
            // Device files are Linux-only. On Windows/macOS the
            // BackendSupervisor doesn't bind /dev/dri into containers
            // anyway (it uses --gpus on Docker Desktop), so leaving
            // this empty is correct.
            device_files: Vec::new(),
            kernel_card_index: 0,
        }
    }
}

/// Read the sysfs tree at `root` (typically `/sys`) and return a
/// HardwareProfile with one `GpuDevice` per `/sys/class/drm/card*` entry.
///
/// Returns an empty profile if the path doesn't exist — the control plane
/// happily runs on GPU-less machines.
pub fn detect_sysfs(root: &Path) -> HardwareProfile {
    let mut gpus = Vec::new();
    let drm = root.join("class").join("drm");
    if !drm.exists() {
        return HardwareProfile {
            gpus,
            source: SysfsSource::Sysfs,
        };
    }

    // Enumerate card* dirs.
    let Ok(rd) = std::fs::read_dir(&drm) else {
        return HardwareProfile {
            gpus,
            source: SysfsSource::Sysfs,
        };
    };

    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let name_os = entry.file_name();
        let Some(name) = name_os.to_str() else {
            continue;
        };

        // Match "card0", "card1", etc. — but not "card0-eDP-1".
        if !name.starts_with("card") {
            continue;
        }
        let rest = &name[4..];
        let Ok(kernel_idx) = rest.parse::<u32>() else {
            continue;
        };

        let device_dir = entry.path().join("device");
        let vendor_hex = std::fs::read_to_string(device_dir.join("vendor")).unwrap_or_default();
        let device_hex = std::fs::read_to_string(device_dir.join("device")).unwrap_or_default();

        let vendor_trimmed = vendor_hex.trim().to_owned();
        let device_trimmed = device_hex.trim().to_owned();

        if vendor_trimmed.is_empty() {
            continue;
        }

        let vendor = GpuVendor::from_pci_vendor(&vendor_trimmed);

        // Render device files — renderD128, renderD129, etc. — are looked
        // up in `/sys/class/drm/renderD*` under the same root. We keep
        // this simple: one guess per card index.
        let mut device_files = Vec::new();
        let render_name = format!("renderD{}", 128 + kernel_idx);
        let render_path = PathBuf::from("/dev/dri").join(&render_name);
        device_files.push(render_path);

        let id = GpuId(format!("{}:card{}", vendor_trimmed, kernel_idx));
        gpus.push(GpuDevice {
            id,
            vendor,
            pci_vendor_id: vendor_trimmed,
            pci_device_id: device_trimmed,
            device_files,
            kernel_card_index: kernel_idx,
        });
    }

    HardwareProfile {
        gpus,
        source: SysfsSource::Sysfs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_mock_sysfs() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        // Simulate one nvidia + one intel GPU.
        let nvidia_path = root.join("class/drm/card0/device");
        std::fs::create_dir_all(&nvidia_path).unwrap();
        std::fs::write(nvidia_path.join("vendor"), "0x10de\n").unwrap();
        std::fs::write(nvidia_path.join("device"), "0x2684\n").unwrap();

        let intel_path = root.join("class/drm/card1/device");
        std::fs::create_dir_all(&intel_path).unwrap();
        std::fs::write(intel_path.join("vendor"), "0x8086\n").unwrap();
        std::fs::write(intel_path.join("device"), "0xe20b\n").unwrap();

        // A non-card directory that must be ignored.
        std::fs::create_dir_all(root.join("class/drm/renderD128")).unwrap();
        // A "card0-DP-1" connector sub-dir that must be ignored.
        std::fs::create_dir_all(root.join("class/drm/card0-DP-1")).unwrap();

        (dir, root)
    }

    #[test]
    fn detects_nvidia_and_intel_from_mock_sysfs() {
        let (_dir, root) = mk_mock_sysfs();
        let prof = detect_sysfs(&root);
        assert_eq!(
            prof.gpus.len(),
            2,
            "should find exactly 2 gpus, got {:?}",
            prof.gpus
        );
        let vendors: Vec<GpuVendor> = prof.gpus.iter().map(|g| g.vendor).collect();
        assert!(vendors.contains(&GpuVendor::Nvidia));
        assert!(vendors.contains(&GpuVendor::Intel));
        // Kernel card indexes are correct.
        assert_eq!(prof.gpus[0].kernel_card_index, 0);
        assert_eq!(prof.gpus[1].kernel_card_index, 1);
    }

    #[test]
    fn returns_empty_profile_when_no_drm_dir() {
        let dir = tempfile::tempdir().unwrap();
        let prof = detect_sysfs(dir.path());
        assert!(prof.gpus.is_empty());
    }

    #[test]
    fn from_pci_vendor_maps_correctly() {
        assert_eq!(GpuVendor::from_pci_vendor("0x10de"), GpuVendor::Nvidia);
        assert_eq!(GpuVendor::from_pci_vendor("0x8086"), GpuVendor::Intel);
        assert_eq!(GpuVendor::from_pci_vendor("0x1002"), GpuVendor::Amd);
        assert_eq!(GpuVendor::from_pci_vendor("0xdead"), GpuVendor::Unknown);
    }

    // ---- Phase 14 — hardware-query adapter tests ------------------------

    /// Build a `hardware_query::GPUInfo` via JSON deserialization
    /// because `ComputeCapabilities` is publicly used from `GPUInfo`
    /// but the parent module is `mod gpu;` (private), so a struct
    /// literal won't compile downstream. The struct is `Deserialize`
    /// which is the supported construction path.
    fn mk_query_gpu(
        vendor_str: &str,
        model: &str,
        device_id: Option<&str>,
    ) -> hardware_query::GPUInfo {
        let device_id_json = match device_id {
            Some(s) => format!("\"{s}\""),
            None => "null".to_owned(),
        };
        let json = format!(
            r#"{{
                "vendor": {vendor_str},
                "model_name": "{model}",
                "gpu_type": "Discrete",
                "memory_mb": 0,
                "memory_type": null,
                "memory_bandwidth": null,
                "base_clock": null,
                "boost_clock": null,
                "memory_clock": null,
                "shader_units": null,
                "rt_cores": null,
                "tensor_cores": null,
                "compute_capabilities": {{
                    "cuda": null,
                    "rocm": false,
                    "directml": false,
                    "opencl": false,
                    "vulkan": false,
                    "metal": false,
                    "compute_units": null,
                    "max_workgroup_size": null
                }},
                "usage_percent": null,
                "temperature": null,
                "power_consumption": null,
                "power_limit": null,
                "driver_version": null,
                "vbios_version": null,
                "pci_device_id": {device_id_json},
                "pci_subsystem_id": null
            }}"#,
        );
        serde_json::from_str(&json).expect("hand-rolled JSON must round-trip")
    }

    #[test]
    fn from_query_maps_nvidia_to_local_enum() {
        let g = mk_query_gpu("\"NVIDIA\"", "GeForce RTX 4090", Some("0x2684"));
        let dev = GpuDevice::from_query(&g);
        assert_eq!(dev.vendor, GpuVendor::Nvidia);
        assert_eq!(dev.pci_vendor_id, "0x10de");
        assert_eq!(dev.pci_device_id, "0x2684");
    }

    #[test]
    fn from_query_maps_intel_to_local_enum() {
        let g = mk_query_gpu("\"Intel\"", "Arc A770", Some("0xe20b"));
        let dev = GpuDevice::from_query(&g);
        assert_eq!(dev.vendor, GpuVendor::Intel);
        assert_eq!(dev.pci_vendor_id, "0x8086");
    }

    #[test]
    fn from_query_maps_amd_to_local_enum() {
        let g = mk_query_gpu("\"AMD\"", "RX 7900 XTX", None);
        let dev = GpuDevice::from_query(&g);
        assert_eq!(dev.vendor, GpuVendor::Amd);
        assert_eq!(dev.pci_vendor_id, "0x1002");
        // Missing pci_device_id falls back to "0x0000" rather than
        // panicking — apple silicon hosts hit this path.
        assert_eq!(dev.pci_device_id, "0x0000");
    }

    #[test]
    fn from_query_maps_apple_to_unknown() {
        // Apple Silicon GPUs are real GPUs, but the v1 preset library
        // has no MLX path. Reporting them as Unknown makes the
        // BackendWizard recommend the CPU preset, which is the truth
        // until MLX presets ship.
        let g = mk_query_gpu("\"Apple\"", "Apple M3 Pro", Some("0xa07"));
        let dev = GpuDevice::from_query(&g);
        assert_eq!(dev.vendor, GpuVendor::Unknown);
    }

    #[test]
    fn from_query_uses_model_name_when_device_id_missing() {
        // Two Apple GPUs without device ids should still get distinct
        // ids so the SPA can render them as separate badges.
        let g1 = mk_query_gpu("\"Apple\"", "M3 Pro", None);
        let g2 = mk_query_gpu("\"Apple\"", "M3 Max", None);
        let dev1 = GpuDevice::from_query(&g1);
        let dev2 = GpuDevice::from_query(&g2);
        assert_ne!(dev1.id, dev2.id);
    }

    #[test]
    fn from_query_unknown_vendor_string_maps_to_unknown_local() {
        // hardware-query's GPUVendor::Unknown(String) catches anything
        // we don't recognize — like a future Imagination Tech, ARM
        // Mali, etc. Our adapter folds those into our Unknown.
        let g = mk_query_gpu(
            "{\"Unknown\":\"Imagination PowerVR\"}",
            "PowerVR GE10",
            None,
        );
        let dev = GpuDevice::from_query(&g);
        assert_eq!(dev.vendor, GpuVendor::Unknown);
    }

    #[test]
    fn detect_doesnt_panic_on_any_host() {
        // Smoke test: the production `detect()` runs hardware-query
        // and falls through to "no GPUs" on any platform-specific
        // failure. Just call it; assert we always get a profile.
        let p = detect();
        assert_eq!(p.source, SysfsSource::HardwareQuery);
    }
}
