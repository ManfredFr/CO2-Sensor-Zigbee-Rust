//! # Zigbee layer — endpoints, stack lifecycle, and reporting
//!
//! This module drives Espressif's closed-source Zigbee stack
//! (`esp-zigbee-lib`) through the bindgen-generated FFI in
//! `esp_idf_svc::sys::zb`. There is no Rust Zigbee stack; everything here is
//! a thin, carefully-ordered wrapper around the same C calls the ESP-IDF
//! examples make.
//!
//! ## Device model
//!
//! The device is a **Zigbee End Device** with three endpoints, chosen to
//! match the Zigbee2MQTT external converter (`Co2-Sensor.js`) exactly:
//!
//! | EP | Cluster | Role |
//! |----|---------|------|
//! | 1  | Temperature Measurement | CO2 carrier: ppm stored as ppm/100 °C, so the ZCL INT16 `measuredValue` equals ppm directly |
//! | 2  | Analog Output | Report interval in seconds (read/write from HA) |
//! | 3  | Analog Output | LED brightness in percent (read/write from HA) |
//!
//! Why a *temperature* cluster for CO2? The SDK's standard CO2 cluster
//! attribute is not marked reportable in its attribute tables, so configured
//! reporting can't be attached to it — the temperature cluster's
//! `measuredValue` is reportable. Scaling ppm by 1/100 makes the wire format
//! (INT16, centi-degrees) numerically identical to whole ppm, which the
//! converter simply reads back.
//!
//! ## Threading rules (violating these crashes the closed-source stack)
//!
//! - The stack runs in its own thread (`zigbee_task` → never returns).
//! - Any Zigbee API call from *another* thread must be wrapped in
//!   `esp_zb_lock_acquire` / `esp_zb_lock_release`.
//! - **Never** call Zigbee APIs from inside a stack callback
//!   (`esp_zb_app_signal_handler`, the action handler): the lock is already
//!   held there. Callbacks only write to atomics; the main thread picks the
//!   values up later.
//! - **Never** send an explicit report command
//!   (`esp_zb_zcl_report_attr_cmd_req`) before the coordinator has created a
//!   binding — the stack hits an internal assert
//!   (`zcl_general_commands.c:612`) and reboots. We avoid the whole class of
//!   bug by only ever *setting* attribute values and letting configured
//!   reporting (established by the converter's `configure` step on join) do
//!   the sending.

use esp_idf_svc::sys::zb::*;
use log::{info, warn};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

// ---------------------------------------------------------------------------
// Constants shared with the Z2M converter — do not change one without the
// other, the endpoint numbers and value ranges are part of the contract.
// ---------------------------------------------------------------------------

pub const CO2_ENDPOINT: u8 = 1;
pub const INTERVAL_ENDPOINT: u8 = 2;
pub const BRIGHTNESS_ENDPOINT: u8 = 3;

pub const INTERVAL_MIN_S: u32 = 10;
pub const INTERVAL_MAX_S: u32 = 300;
pub const INTERVAL_DEFAULT_S: u32 = 30;
pub const LED_BRIGHTNESS_DEFAULT: u32 = 50;


// ---------------------------------------------------------------------------
// Cross-thread state
//
// Written by the Zigbee callbacks (stack thread), read by the main loop.
// Plain atomics are enough: each value is an independent scalar and slight
// staleness (one loop iteration) is harmless.
// ---------------------------------------------------------------------------

pub static REPORT_INTERVAL_S: AtomicU32 = AtomicU32::new(INTERVAL_DEFAULT_S);
pub static LED_BRIGHTNESS: AtomicU32 = AtomicU32::new(LED_BRIGHTNESS_DEFAULT);
pub static CONNECTED: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------
// ZCL strings
// ---------------------------------------------------------------------------

/// Convert a Rust string into a ZCL character string: length-prefixed (first
/// byte = length), not NUL-terminated.
///
/// The stack stores the *pointer* we hand it rather than copying, so the
/// buffer must live for the program's lifetime — `Box::leak` provides that.
/// This is called a handful of times during endpoint construction, so the
/// deliberate leak amounts to a few dozen bytes, once.
fn zcl_string(s: &str) -> *mut c_void {
    assert!(s.len() <= u8::MAX as usize, "ZCL string too long");
    let mut bytes = Vec::with_capacity(s.len() + 1);
    bytes.push(s.len() as u8);
    bytes.extend_from_slice(s.as_bytes());
    Box::leak(bytes.into_boxed_slice()).as_mut_ptr() as *mut c_void
}

// ---------------------------------------------------------------------------
// Stack callbacks
// ---------------------------------------------------------------------------

/// Application signal handler — the stack's lifecycle event channel.
///
/// The Zigbee C library *declares* this symbol `extern` and calls it for
/// every stack signal; defining it here with `#[no_mangle] extern "C"` is
/// what links the two worlds together (same pattern as a C `main`
/// component). There is no registration call.
///
/// The commissioning flow driven from here:
///
/// 1. `SKIP_STARTUP` — stack is initialized; kick off BDB initialization.
/// 2. `DEVICE_FIRST_START` / `DEVICE_REBOOT` — device is up (factory-new or
///    with persisted network data); start network steering.
/// 3. `STEERING` — steering finished: either we joined (flag `CONNECTED`) or
///    it failed (e.g. permit-join disabled) and we schedule a retry in 1 s.
/// 4. `LEAVE` — kicked or reset from the coordinator; clear `CONNECTED`.
#[no_mangle]
pub unsafe extern "C" fn esp_zb_app_signal_handler(signal: *mut esp_zb_app_signal_t) {
    // `p_app_signal` points to a u32 holding the signal type. A single
    // dereference — the C headers use a double pointer in the struct type
    // but the payload is one level down. (A double deref here reads a
    // garbage address and load-faults; ask us how we know.)
    let sig_type = *(*signal).p_app_signal;
    let status = (*signal).esp_err_status;
    let ok = status == 0;

    match sig_type {
        esp_zb_app_signal_type_t_ESP_ZB_ZDO_SIGNAL_SKIP_STARTUP => {
            info!("[ZIGBEE] Stack initialized, starting commissioning");
            esp_zb_bdb_start_top_level_commissioning(
                esp_zb_bdb_commissioning_mode_t_ESP_ZB_BDB_MODE_INITIALIZATION as u8,
            );
        }
        esp_zb_app_signal_type_t_ESP_ZB_BDB_SIGNAL_DEVICE_FIRST_START
        | esp_zb_app_signal_type_t_ESP_ZB_BDB_SIGNAL_DEVICE_REBOOT => {
            if ok {
                info!("[ZIGBEE] Device started, steering network");
                esp_zb_bdb_start_top_level_commissioning(
                    esp_zb_bdb_commissioning_mode_t_ESP_ZB_BDB_MODE_NETWORK_STEERING as u8,
                );
            } else {
                warn!("[ZIGBEE] Device start failed (status {status}), retrying");
                esp_zb_bdb_start_top_level_commissioning(
                    esp_zb_bdb_commissioning_mode_t_ESP_ZB_BDB_MODE_INITIALIZATION as u8,
                );
            }
        }
        esp_zb_app_signal_type_t_ESP_ZB_BDB_SIGNAL_STEERING => {
            if ok {
                info!("[ZIGBEE] Joined network!");
                CONNECTED.store(true, Ordering::SeqCst);
            } else {
                // Normal while permit-join is off on the coordinator. Retry
                // via the stack's own scheduler — calling commissioning
                // directly here would recurse into the stack mid-callback.
                warn!("[ZIGBEE] Steering failed (status {status}), retrying in 1s");
                esp_zb_scheduler_alarm(Some(retry_steering), 0, 1000);
            }
        }
        esp_zb_app_signal_type_t_ESP_ZB_ZDO_SIGNAL_LEAVE => {
            warn!("[ZIGBEE] Left network");
            CONNECTED.store(false, Ordering::SeqCst);
        }
        // Plenty of other signals arrive (PERMIT_JOIN_STATUS, CAN_SLEEP, …);
        // none of them require action for this device.
        _ => {}
    }
}

/// Scheduler-alarm callback used by the steering retry above. Runs inside
/// the stack task, so calling the commissioning API directly is fine here.
unsafe extern "C" fn retry_steering(_param: u8) {
    esp_zb_bdb_start_top_level_commissioning(
        esp_zb_bdb_commissioning_mode_t_ESP_ZB_BDB_MODE_NETWORK_STEERING as u8,
    );
}

/// Core action handler — invoked by the stack for ZCL-level events.
///
/// The only event this device cares about is `SET_ATTR_VALUE`: Home
/// Assistant (via the converter's `toZigbee` handlers) writing a new
/// `presentValue` to one of the two Analog Output endpoints. ZCL Analog
/// Output's `presentValue` is a single-precision float, hence the `*const
/// f32` read.
///
/// Runs inside the stack task with the Zigbee lock held → touch atomics
/// only, return quickly, make no Zigbee calls.
unsafe extern "C" fn zb_action_handler(
    callback_id: esp_zb_core_action_callback_id_t,
    message: *const c_void,
) -> esp_err_t {
    if callback_id == esp_zb_core_action_callback_id_s_ESP_ZB_CORE_SET_ATTR_VALUE_CB_ID {
        let msg = &*(message as *const esp_zb_zcl_set_attr_value_message_t);
        if msg.info.cluster == esp_zb_zcl_cluster_id_t_ESP_ZB_ZCL_CLUSTER_ID_ANALOG_OUTPUT as u16
            && msg.attribute.id
                == esp_zb_zcl_analog_output_attr_t_ESP_ZB_ZCL_ATTR_ANALOG_OUTPUT_PRESENT_VALUE_ID
                    as u16
            && !msg.attribute.data.value.is_null()
        {
            let value = *(msg.attribute.data.value as *const f32);
            match msg.info.dst_endpoint {
                INTERVAL_ENDPOINT => {
                    // Clamp defensively: the converter exposes 10–300 s, but
                    // nothing stops a raw MQTT write from sending 0 or 10^6.
                    let s = (value as u32).clamp(INTERVAL_MIN_S, INTERVAL_MAX_S);
                    REPORT_INTERVAL_S.store(s, Ordering::SeqCst);
                    info!("[INTERVAL] Updated to {s} seconds");
                }
                BRIGHTNESS_ENDPOINT => {
                    let pct = (value as u32).min(100);
                    LED_BRIGHTNESS.store(pct, Ordering::SeqCst);
                    info!("[LED] Brightness set to {pct}%");
                }
                _ => {}
            }
        }
    }
    0 // ESP_OK — tell the stack the write was accepted
}

// ---------------------------------------------------------------------------
// Endpoint construction
//
// The esp_zb_*_create functions allocate on the stack's heap and return raw
// pointers that esp_zb_device_register takes ownership of. Registration
// happens once at startup, so the "leaks" here are intentional one-time
// allocations, exactly like in the C examples.
// ---------------------------------------------------------------------------

/// Add Basic + Identify clusters to a cluster list. Every Zigbee endpoint is
/// expected to carry these two; the Basic cluster additionally identifies
/// the device to Zigbee2MQTT (`zigbeeModel: ['Co2Sensor']` in the converter
/// matches the model string set here).
unsafe fn make_basic_identify(cluster_list: *mut esp_zb_cluster_list_t) {
    let mut basic_cfg = esp_zb_basic_cluster_cfg_t {
        zcl_version: 3,
        power_source: 4, // 4 = DC source (the S8 + board run from USB 5 V)
    };
    let basic = esp_zb_basic_cluster_create(&mut basic_cfg);
    esp_zb_basic_cluster_add_attr(
        basic,
        esp_zb_zcl_basic_attr_t_ESP_ZB_ZCL_ATTR_BASIC_MANUFACTURER_NAME_ID as u16,
        zcl_string("Espressif"),
    );
    esp_zb_basic_cluster_add_attr(
        basic,
        esp_zb_zcl_basic_attr_t_ESP_ZB_ZCL_ATTR_BASIC_MODEL_IDENTIFIER_ID as u16,
        zcl_string("Co2Sensor"),
    );
    // Version attributes — all derived from src/version.rs. appVersion is
    // the converter's `firmware_version` expose; swBuildId + dateCode show
    // as "Firmware ID" on Zigbee2MQTT's device About page.
    // (`static` so the pointer stays valid for the stack's lifetime.)
    static APP_VERSION: u8 = crate::version::APP_VERSION;
    esp_zb_basic_cluster_add_attr(
        basic,
        esp_zb_zcl_basic_attr_t_ESP_ZB_ZCL_ATTR_BASIC_APPLICATION_VERSION_ID as u16,
        &APP_VERSION as *const u8 as *mut c_void,
    );
    esp_zb_basic_cluster_add_attr(
        basic,
        esp_zb_zcl_basic_attr_t_ESP_ZB_ZCL_ATTR_BASIC_SW_BUILD_ID as u16,
        zcl_string(&crate::version::string()),
    );
    esp_zb_basic_cluster_add_attr(
        basic,
        esp_zb_zcl_basic_attr_t_ESP_ZB_ZCL_ATTR_BASIC_DATE_CODE_ID as u16,
        zcl_string(crate::version::DATE_CODE),
    );
    esp_zb_cluster_list_add_basic_cluster(
        cluster_list,
        basic,
        esp_zb_zcl_cluster_role_t_ESP_ZB_ZCL_CLUSTER_SERVER_ROLE as u8,
    );

    let mut identify_cfg = esp_zb_identify_cluster_cfg_t { identify_time: 0 };
    esp_zb_cluster_list_add_identify_cluster(
        cluster_list,
        esp_zb_identify_cluster_create(&mut identify_cfg),
        esp_zb_zcl_cluster_role_t_ESP_ZB_ZCL_CLUSTER_SERVER_ROLE as u8,
    );
}

/// Build one Analog Output endpoint (used for both the interval and the
/// brightness setting) and append it to `ep_list`.
///
/// Besides the mandatory `presentValue`, we add the optional `description`,
/// `minPresentValue` and `maxPresentValue` attributes so generic Zigbee
/// tooling can render a labelled, bounded slider without knowing anything
/// about this device.
unsafe fn make_analog_output_ep(
    ep_list: *mut esp_zb_ep_list_t,
    endpoint: u8,
    description: &str,
    default_value: f32,
    min: f32,
    max: f32,
) {
    let cluster_list = esp_zb_zcl_cluster_list_create();
    make_basic_identify(cluster_list);

    let mut ao_cfg = esp_zb_analog_output_cluster_cfg_t {
        out_of_service: false,
        present_value: default_value,
        status_flags: 0,
    };
    let ao = esp_zb_analog_output_cluster_create(&mut ao_cfg);
    esp_zb_analog_output_cluster_add_attr(
        ao,
        esp_zb_zcl_analog_output_attr_t_ESP_ZB_ZCL_ATTR_ANALOG_OUTPUT_DESCRIPTION_ID as u16,
        zcl_string(description),
    );
    // The attribute table stores these pointers rather than copying the
    // values, so leak the boxes deliberately — two f32s per endpoint, once.
    let min = Box::leak(Box::new(min));
    let max = Box::leak(Box::new(max));
    esp_zb_analog_output_cluster_add_attr(
        ao,
        esp_zb_zcl_analog_output_attr_t_ESP_ZB_ZCL_ATTR_ANALOG_OUTPUT_MIN_PRESENT_VALUE_ID as u16,
        min as *mut f32 as *mut c_void,
    );
    esp_zb_analog_output_cluster_add_attr(
        ao,
        esp_zb_zcl_analog_output_attr_t_ESP_ZB_ZCL_ATTR_ANALOG_OUTPUT_MAX_PRESENT_VALUE_ID as u16,
        max as *mut f32 as *mut c_void,
    );
    esp_zb_cluster_list_add_analog_output_cluster(
        cluster_list,
        ao,
        esp_zb_zcl_cluster_role_t_ESP_ZB_ZCL_CLUSTER_SERVER_ROLE as u8,
    );

    // Endpoint descriptor: Home Automation profile (0x0104), generic
    // "Simple Sensor" device id — nothing reads the device id here, but it
    // must be a valid HA profile value.
    let mut ep_cfg: esp_zb_endpoint_config_t = core::mem::zeroed();
    ep_cfg.endpoint = endpoint;
    ep_cfg.app_profile_id = 0x0104;
    ep_cfg.app_device_id = 0x000C;
    esp_zb_ep_list_add_ep(ep_list, cluster_list, ep_cfg);
}

// ---------------------------------------------------------------------------
// Stack lifecycle
// ---------------------------------------------------------------------------

/// Bring up the Zigbee stack and run its main loop forever.
///
/// Must run in a dedicated thread — `esp_zb_stack_main_loop()` never
/// returns. Everything before it is one-time setup, in the order the SDK
/// requires: platform config → esp_zb_init → endpoint registration →
/// handler registration → channel mask → start.
pub fn zigbee_task() -> ! {
    unsafe {
        // Radio and host config. The H2 has a native 802.15.4 radio and no
        // separate host MCU, so: native radio, no host connection.
        let mut platform_cfg: esp_zb_platform_config_t = core::mem::zeroed();
        platform_cfg.radio_config.radio_mode = esp_zb_radio_mode_t_ZB_RADIO_MODE_NATIVE;
        platform_cfg.host_config.host_connection_mode =
            esp_zb_host_connection_mode_t_ZB_HOST_CONNECTION_MODE_NONE;
        esp_zb_platform_config(&mut platform_cfg);

        // Role: End Device (ZED). `ed_timeout` is the aging timeout the
        // parent router uses to forget us if we go silent; `keep_alive` is
        // how often (ms) we poll the parent to stay remembered.
        let mut zb_cfg: esp_zb_cfg_t = core::mem::zeroed();
        zb_cfg.esp_zb_role = esp_zb_nwk_device_type_t_ESP_ZB_DEVICE_TYPE_ED;
        zb_cfg.install_code_policy = false;
        zb_cfg.nwk_cfg.zed_cfg.ed_timeout =
            esp_zb_aging_timeout_t_ESP_ZB_ED_AGING_TIMEOUT_64MIN as u8;
        zb_cfg.nwk_cfg.zed_cfg.keep_alive = 3000;
        esp_zb_init(&mut zb_cfg);

        let ep_list = esp_zb_ep_list_create();

        // --- EP1: CO2 via Temperature Measurement cluster ----------------
        // min/max are in centi-degrees, i.e. 0–32700 "°C·100" == 0–32700 ppm
        // in our carrier encoding (the INT16 tops out at 32767).
        let cluster_list = esp_zb_zcl_cluster_list_create();
        make_basic_identify(cluster_list);
        let mut temp_cfg = esp_zb_temperature_meas_cluster_cfg_t {
            measured_value: 0,
            min_value: 0,
            max_value: 32700,
        };
        esp_zb_cluster_list_add_temperature_meas_cluster(
            cluster_list,
            esp_zb_temperature_meas_cluster_create(&mut temp_cfg),
            esp_zb_zcl_cluster_role_t_ESP_ZB_ZCL_CLUSTER_SERVER_ROLE as u8,
        );
        let mut ep_cfg: esp_zb_endpoint_config_t = core::mem::zeroed();
        ep_cfg.endpoint = CO2_ENDPOINT;
        ep_cfg.app_profile_id = 0x0104; // Home Automation profile
        ep_cfg.app_device_id = 0x0302; // HA Temperature Sensor
        esp_zb_ep_list_add_ep(ep_list, cluster_list, ep_cfg);

        // --- EP2 + EP3: writable settings ---------------------------------
        make_analog_output_ep(
            ep_list,
            INTERVAL_ENDPOINT,
            "Report interval",
            INTERVAL_DEFAULT_S as f32,
            INTERVAL_MIN_S as f32,
            INTERVAL_MAX_S as f32,
        );
        make_analog_output_ep(
            ep_list,
            BRIGHTNESS_ENDPOINT,
            "LED brightness",
            LED_BRIGHTNESS_DEFAULT as f32,
            0.0,
            100.0,
        );

        esp_zb_device_register(ep_list);
        esp_zb_core_action_handler_register(Some(zb_action_handler));

        // Scan all 16 channels (11–26): the coordinator picks the channel,
        // we have no reason to restrict the search.
        esp_zb_set_primary_network_channel_set(ESP_ZB_TRANSCEIVER_ALL_CHANNELS_MASK);

        info!("[ZIGBEE] Starting stack...");
        // autostart=false: commissioning is driven explicitly from the
        // signal handler (SKIP_STARTUP fires first with this setting).
        esp_zb_start(false);
        esp_zb_stack_main_loop();
    }
    unreachable!("esp_zb_stack_main_loop never returns")
}

// ---------------------------------------------------------------------------
// Reporting (called from the main thread)
// ---------------------------------------------------------------------------

/// Publish a CO2 reading. Call only while `CONNECTED` is true.
///
/// This only *sets* the attribute value (under the Zigbee lock). Once the
/// converter's `configure` step has bound EP1 to the coordinator and
/// configured reporting, the stack notices the change and sends the report
/// itself. Sending an explicit report command here instead would assert
/// inside the stack whenever the binding doesn't exist yet (fresh join,
/// interview still running) and reboot the device.
pub fn report_co2(ppm: u16) {
    // Carrier encoding: ZCL temperature INT16 is in centi-degrees, and we
    // define the value as ppm/100 °C — so the raw INT16 *is* the ppm value.
    let value: i16 = ppm as i16;
    unsafe {
        if !esp_zb_lock_acquire(u32::MAX) {
            warn!("[ZIGBEE] Could not acquire lock");
            return;
        }
        esp_zb_zcl_set_attribute_val(
            CO2_ENDPOINT,
            esp_zb_zcl_cluster_id_t_ESP_ZB_ZCL_CLUSTER_ID_TEMP_MEASUREMENT as u16,
            esp_zb_zcl_cluster_role_t_ESP_ZB_ZCL_CLUSTER_SERVER_ROLE as u8,
            esp_zb_zcl_temp_measurement_attr_t_ESP_ZB_ZCL_ATTR_TEMP_MEASUREMENT_VALUE_ID as u16,
            &value as *const i16 as *mut c_void,
            false, // don't range-check against the cluster's min/max
        );
        esp_zb_lock_release();
    }
}

/// Push the current interval and brightness into their ZCL attributes, so a
/// read from Home Assistant returns real values. Called once after joining;
/// subsequent changes flow the other way (HA writes → action handler).
pub fn report_settings() {
    unsafe {
        if !esp_zb_lock_acquire(u32::MAX) {
            return;
        }
        for (ep, value) in [
            (
                INTERVAL_ENDPOINT,
                REPORT_INTERVAL_S.load(Ordering::SeqCst) as f32,
            ),
            (
                BRIGHTNESS_ENDPOINT,
                LED_BRIGHTNESS.load(Ordering::SeqCst) as f32,
            ),
        ] {
            esp_zb_zcl_set_attribute_val(
                ep,
                esp_zb_zcl_cluster_id_t_ESP_ZB_ZCL_CLUSTER_ID_ANALOG_OUTPUT as u16,
                esp_zb_zcl_cluster_role_t_ESP_ZB_ZCL_CLUSTER_SERVER_ROLE as u8,
                esp_zb_zcl_analog_output_attr_t_ESP_ZB_ZCL_ATTR_ANALOG_OUTPUT_PRESENT_VALUE_ID
                    as u16,
                &value as *const f32 as *mut c_void,
                false,
            );
        }
        esp_zb_lock_release();
    }
}
