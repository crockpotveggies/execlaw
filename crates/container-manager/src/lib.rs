//! execlaw-container-manager
//!
//! Single entry-point for all Docker interactions + tiered hardware
//! detection (§5.3).
//!
//! - Tier 1 **sysfs** reads (`/sys/class/drm/card*/device/vendor` etc.) —
//!   implemented here, pure-Rust, no vendor tooling in the control plane.
//! - Tier 2 **docker info** runtime query via bollard.
//! - Tier 3 **probe containers** — spawned on demand; the probe images are
//!   `hardware_probe` plugins.
//! - Tier 4 **manual override** — reads from `config_hardware_profile_overrides`.
//!
//! Phase 0 ships Tier 1 (which is the load-bearing one) + data shapes for
//! Tiers 2–4 + a stubbed bollard wrapper. Probe containers are Phase 2.

#![forbid(unsafe_code)]

pub mod hardware;

pub use hardware::{GpuDevice, GpuId, GpuVendor, HardwareProfile, SysfsSource, detect_sysfs};
