# Claude Instructions

When starting a session in this folder, always read the following files first:
- `CHANGELOG.md` — version history and key lessons learned
- `README.md` — full project documentation

This is an ESP32-H2 Zigbee CO2 sensor project using a Senseair S8 sensor. Current firmware is **v2.0 (Rust)** in `Rust/`; the legacy Arduino firmware (v1.3) was removed and lives only in git history.
Key files: `Rust/src/main.rs`, `Rust/src/zigbee.rs`, `Co2-Sensor.js` (Z2M converter, shared by both firmwares), `Rust/README.md` (build/flash instructions and Rust-specific lessons).
The working device in Zigbee2MQTT is `0x4831b7fffec56026`; a second, separate CO2 sensor also exists — do not touch it.
