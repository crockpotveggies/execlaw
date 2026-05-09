# execlaw-container-manager

Single Rust crate owning every Docker interaction and the tiered hardware
profile detection (§5.3).

Phase 0 scope: Tier 1 sysfs reads (pure Rust, no vendor tooling in the
control plane) + data shapes for Tiers 2–4. Bollard integration + probe
containers land in Phase 2.
