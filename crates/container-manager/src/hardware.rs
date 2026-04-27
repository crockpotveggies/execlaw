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
    /// Resolved SKU from `hardware-query`'s WMI / sysfs / IOKit
    /// readout (e.g. "GeForce RTX 4090", "Arc A770", "Apple M3 Pro").
    /// `None` for the legacy sysfs path which only carries PCI ids.
    /// The setup wizard renders this as the badge text instead of
    /// the raw PCI string.
    #[serde(default)]
    pub model_name: Option<String>,
    /// VRAM in MiB, also from `hardware-query`. Used by the setup
    /// wizard to filter the model catalog to entries that fit.
    #[serde(default)]
    pub memory_mb: Option<u64>,
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
        Ok(hw) => {
            let mut gpus: Vec<GpuDevice> =
                hw.gpus().iter().map(GpuDevice::from_query).collect();
            apply_vram_corrections(&mut gpus);
            HardwareProfile {
                gpus,
                source: SysfsSource::HardwareQuery,
            }
        }
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

/// Apply per-vendor VRAM overrides on top of the hardware-query
/// readout. See `gpu_memory.rs` for why each path exists.
///
/// The corrections are best-effort: if NVML / the registry lookup
/// fails we keep the original (possibly truncated) hardware-query
/// value so the wizard still functions, just with the known
/// under-reporting.
fn apply_vram_corrections(gpus: &mut [GpuDevice]) {
    // NVIDIA — order in NVML matches `nvml.device_by_index` enumeration
    // order, which on every Windows / Linux host I've checked matches
    // hardware-query's NVIDIA-merge order. We zip by NVIDIA-only
    // position to be defensive against multi-vendor hosts where
    // hardware-query interleaves Intel + NVIDIA in a different order.
    let nvml = crate::gpu_memory::nvidia_memory_mb_via_nvml();
    let mut nvml_iter = nvml.into_iter();
    for g in gpus.iter_mut().filter(|g| g.vendor == GpuVendor::Nvidia) {
        if let Some(mb) = nvml_iter.next() {
            tracing::debug!(
                model = ?g.model_name,
                old_memory_mb = ?g.memory_mb,
                new_memory_mb = mb,
                "apply NVML VRAM override"
            );
            g.memory_mb = Some(mb);
        }
    }

    // Intel — match by `model_name` substring against the registry's
    // `DriverDesc`. Both come from Windows, so the strings tend to
    // line up exactly ("Intel(R) Arc(TM) B580 Graphics"). We do a
    // case-insensitive substring test in case driver versions tweak
    // formatting.
    let registry_intel = crate::gpu_memory::intel_memory_mb_via_registry();
    for g in gpus.iter_mut().filter(|g| g.vendor == GpuVendor::Intel) {
        let model = match &g.model_name {
            Some(m) => m,
            None => continue,
        };
        let model_lower = model.to_ascii_lowercase();
        for (desc, mb) in &registry_intel {
            let desc_lower = desc.to_ascii_lowercase();
            if desc_lower == model_lower
                || desc_lower.contains(&model_lower)
                || model_lower.contains(&desc_lower)
            {
                tracing::debug!(
                    model = %model,
                    driver_desc = %desc,
                    old_memory_mb = ?g.memory_mb,
                    new_memory_mb = mb,
                    "apply Intel registry VRAM override"
                );
                g.memory_mb = Some(*mb);
                break;
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
        // hardware-query's `pci_device_id` includes Windows PNP
        // strings like `PCI\VEN_10DE&DEV_2230&SUBSYS_…&REV_A1\…` —
        // useful to forensic operators, but unreadable as a label.
        // `display_pci_device_id` extracts a clean `0xXXXX` from the
        // PNP string when possible, falling back to whatever we got.
        let device_hex = g
            .pci_device_id
            .as_deref()
            .map(extract_clean_device_hex)
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
        let model_name = if g.model_name.trim().is_empty() {
            None
        } else {
            Some(g.model_name.clone())
        };
        let memory_mb = if g.memory_mb == 0 {
            None
        } else {
            Some(g.memory_mb)
        };
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
            model_name,
            memory_mb,
        }
    }
}

/// Pull a clean `0xXXXX` hex out of whatever `hardware-query`'s
/// `pci_device_id` string carries:
///
/// * Linux sysfs path: already `0x2684` — return as-is.
/// * Windows PNP path: `PCI\VEN_10DE&DEV_2230&SUBSYS_…&REV_A1\…` —
///   parse `&DEV_XXXX` out and prefix with `0x`.
/// * macOS IOKit path: `0xa07` — return as-is.
/// * Anything else → fall through to the original string truncated
///   to 16 chars so the SPA never displays multi-line garbage.
fn extract_clean_device_hex(raw: &str) -> String {
    if raw.starts_with("0x") || raw.starts_with("0X") {
        return raw.to_owned();
    }
    // Windows PNP shape — search for `DEV_` followed by 4 hex chars.
    if let Some(idx) = raw.find("DEV_") {
        let after = &raw[idx + 4..];
        let hex: String = after.chars().take(4).collect();
        if hex.len() == 4 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return format!("0x{}", hex.to_lowercase());
        }
    }
    // Truncate anything else so the SPA doesn't render a ~100-char
    // PNP string as a badge label.
    if raw.len() > 16 {
        format!("{}…", &raw[..15])
    } else {
        raw.to_owned()
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
            // Sysfs alone doesn't expose human-readable model names
            // or VRAM size — that's why the wizard's primary path
            // goes through `detect()` (hardware-query). Sysfs stays
            // as a deterministic test fixture.
            model_name: None,
            memory_mb: None,
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
    fn from_query_pulls_model_name_and_memory_mb() {
        let g = mk_query_gpu("\"NVIDIA\"", "GeForce RTX 4090", Some("0x2684"));
        let mut g = g;
        g.memory_mb = 24_576;
        let dev = GpuDevice::from_query(&g);
        assert_eq!(dev.model_name.as_deref(), Some("GeForce RTX 4090"));
        assert_eq!(dev.memory_mb, Some(24_576));
    }

    #[test]
    fn from_query_zero_memory_falls_through_to_none() {
        // hardware-query reports memory_mb=0 when it can't resolve
        // (Apple Silicon unified-memory often hits this). Our adapter
        // must surface None, not Some(0), so the SPA filters models
        // correctly.
        let g = mk_query_gpu("\"Apple\"", "M3 Pro", None);
        let dev = GpuDevice::from_query(&g);
        assert_eq!(dev.memory_mb, None);
    }

    #[test]
    fn extract_clean_device_hex_handles_linux_sysfs() {
        // Linux sysfs already gives `0xNNNN`.
        assert_eq!(extract_clean_device_hex("0x2684"), "0x2684");
    }

    #[test]
    fn extract_clean_device_hex_parses_windows_pnp() {
        // Windows PNP IDs from `Win32_VideoController` look like:
        //   PCI\VEN_10DE&DEV_2230&SUBSYS_145910DE&REV_A1\4&8BD6E8D&0&0008
        //   PCI\VEN_8086&DEV_E20B&SUBSYS_11008086&REV_00\6&2421D8B7&0&000800E8
        // The user reported these rendering as multi-line badge text.
        let pnp =
            "PCI\\VEN_10DE&DEV_2230&SUBSYS_145910DE&REV_A1\\4&8BD6E8D&0&0008";
        assert_eq!(extract_clean_device_hex(pnp), "0x2230");
        let arc =
            "PCI\\VEN_8086&DEV_E20B&SUBSYS_11008086&REV_00\\6&2421D8B7&0&000800E8";
        assert_eq!(extract_clean_device_hex(arc), "0xe20b");
    }

    #[test]
    fn extract_clean_device_hex_truncates_garbage() {
        let weird = "totally-not-a-pci-id-but-very-long-string-here";
        let got = extract_clean_device_hex(weird);
        assert!(got.chars().count() <= 17, "got: {got}");
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
