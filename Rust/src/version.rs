//! Single source of truth for the firmware version.
//!
//! Bump `MAJOR`/`MINOR` (and `DATE_CODE`) here for a release — everything
//! else derives from them:
//!
//! - the boot banner (`version::string()`, e.g. "v2.0")
//! - the Basic cluster `appVersion` attribute (`APP_VERSION`, MAJOR*10+MINOR,
//!   decoded by the Z2M converter as `v{n/10}.{n%10}`)
//! - the Basic cluster `swBuildId` (same "v2.0" string) and `dateCode`,
//!   shown as "Firmware ID" on Zigbee2MQTT's device About page

pub const MAJOR: u8 = 2;
pub const MINOR: u8 = 1;
pub const PATCH: u8 = 0;

/// Firmware build date (YYYYMMDD) for the Basic cluster's `dateCode`.
pub const DATE_CODE: &str = "20260705";

/// Encoding used by the Basic cluster's `appVersion` attribute (one byte),
/// matching the converter's `v{n/10}.{n%10}` decoding. Limits MINOR to 0–9;
/// the patch level doesn't fit and is only visible in `string()` (swBuildId).
pub const APP_VERSION: u8 = MAJOR * 10 + MINOR;

/// Human-readable version, e.g. "v2.0.1".
pub fn string() -> String {
    format!("v{MAJOR}.{MINOR}.{PATCH}")
}
