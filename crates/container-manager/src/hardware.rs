//! Hardware profile types + Tier 1 sysfs detection (§5.3).
//!
//! Tier 1 runs on every control-plane startup. It reads
//! `/sys/class/drm/card*/device/vendor` and `/device` to produce a
//! `HardwareProfile` listing the installed GPUs with their vendor and
//! PCI device IDs.
//!
//! The function takes a root path so tests can point it at a mock sysfs
//! tree in a temp directory. Production callers pass `Path::new("/sys")`.

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SysfsSource {
    Sysfs,
    Mock,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HardwareProfile {
    pub gpus: Vec<GpuDevice>,
    pub source: SysfsSource,
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
        let Some(name) = name_os.to_str() else { continue };

        // Match "card0", "card1", etc. — but not "card0-eDP-1".
        if !name.starts_with("card") {
            continue;
        }
        let rest = &name[4..];
        let Ok(kernel_idx) = rest.parse::<u32>() else { continue };

        let device_dir = entry.path().join("device");
        let vendor_hex =
            std::fs::read_to_string(device_dir.join("vendor")).unwrap_or_default();
        let device_hex =
            std::fs::read_to_string(device_dir.join("device")).unwrap_or_default();

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
        assert_eq!(prof.gpus.len(), 2, "should find exactly 2 gpus, got {:?}", prof.gpus);
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
}
