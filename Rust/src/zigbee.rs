//! Zigbee End Device: three endpoints matching the Arduino firmware, so the
//! existing Zigbee2MQTT converter (Co2-Sensor.js) keeps working unchanged.
//!
//! - EP1: Temperature Measurement cluster used as CO2 carrier
//!        (ppm stored as ppm/100 °C, so the ZCL INT16 equals ppm directly)
//! - EP2: Analog Output cluster — report interval in seconds (read/write)
//! - EP3: Analog Output cluster — LED brightness 0–100 % (read/write)
//!
//! Safety rules (learned the hard way on the Arduino side, see CHANGELOG):
//! - esp_zb_zcl_set_attribute_val / esp_zb_zcl_report_attr_cmd_req only after
//!   the stack is running, and only while holding the Zigbee lock.
//! - Never call Zigbee APIs from inside a stack callback — the lock is
//!   already held there.

use esp_idf_svc::sys::zb::*;
use log::{info, warn};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

pub const CO2_ENDPOINT: u8 = 1;
pub const INTERVAL_ENDPOINT: u8 = 2;
pub const BRIGHTNESS_ENDPOINT: u8 = 3;

pub const INTERVAL_MIN_S: u32 = 10;
pub const INTERVAL_MAX_S: u32 = 300;
pub const INTERVAL_DEFAULT_S: u32 = 30;
pub const LED_BRIGHTNESS_DEFAULT: u32 = 50;
const FIRMWARE_APP_VERSION: u8 = 20; // v2.0 encoded as MAJOR*10+MINOR

// Written from the Zigbee callback (stack task), read from the main loop.
pub static REPORT_INTERVAL_S: AtomicU32 = AtomicU32::new(INTERVAL_DEFAULT_S);
pub static LED_BRIGHTNESS: AtomicU32 = AtomicU32::new(LED_BRIGHTNESS_DEFAULT);
pub static CONNECTED: AtomicBool = AtomicBool::new(false);

// ZCL character strings are length-prefixed (first byte = length).
static MANUFACTURER: &[u8] = b"\x09Espressif";
static MODEL: &[u8] = b"\x09Co2Sensor";
static DESC_INTERVAL: &[u8] = b"\x0FReport interval";
static DESC_BRIGHTNESS: &[u8] = b"\x0ELED brightness";

/// Called by the Zigbee stack (C library) for every stack signal.
/// The library declares this symbol extern and the linker resolves it here.
#[no_mangle]
pub unsafe extern "C" fn esp_zb_app_signal_handler(signal: *mut esp_zb_app_signal_t) {
    let sig_type = **((*signal).p_app_signal as *mut *mut u32) as u32;
    let status = (*signal).esp_err_status;
    let ok = status == 0;

    match sig_type {
        esp_zb_app_signal_type_t_ESP_ZB_ZDO_SIGNAL_SKIP_STARTUP => {
            info!("[ZIGBEE] Stack initialized, starting commissioning");
            esp_zb_bdb_start_top_level_commissioning(esp_zb_bdb_commissioning_mode_t_ESP_ZB_BDB_MODE_INITIALIZATION as u8);
        }
        esp_zb_app_signal_type_t_ESP_ZB_BDB_SIGNAL_DEVICE_FIRST_START
        | esp_zb_app_signal_type_t_ESP_ZB_BDB_SIGNAL_DEVICE_REBOOT => {
            if ok {
                info!("[ZIGBEE] Device started, steering network");
                esp_zb_bdb_start_top_level_commissioning(esp_zb_bdb_commissioning_mode_t_ESP_ZB_BDB_MODE_NETWORK_STEERING as u8);
            } else {
                warn!("[ZIGBEE] Device start failed (status {status}), retrying");
                esp_zb_bdb_start_top_level_commissioning(esp_zb_bdb_commissioning_mode_t_ESP_ZB_BDB_MODE_INITIALIZATION as u8);
            }
        }
        esp_zb_app_signal_type_t_ESP_ZB_BDB_SIGNAL_STEERING => {
            if ok {
                info!("[ZIGBEE] Joined network!");
                CONNECTED.store(true, Ordering::SeqCst);
            } else {
                warn!("[ZIGBEE] Steering failed (status {status}), retrying in 1s");
                esp_zb_scheduler_alarm(Some(retry_steering), 0, 1000);
            }
        }
        esp_zb_app_signal_type_t_ESP_ZB_ZDO_SIGNAL_LEAVE => {
            warn!("[ZIGBEE] Left network");
            CONNECTED.store(false, Ordering::SeqCst);
        }
        _ => {}
    }
}

unsafe extern "C" fn retry_steering(_param: u8) {
    esp_zb_bdb_start_top_level_commissioning(esp_zb_bdb_commissioning_mode_t_ESP_ZB_BDB_MODE_NETWORK_STEERING as u8);
}

/// ZCL write callback: HA writes to an Analog Output presentValue.
/// Keep minimal — no Zigbee calls allowed here (stack lock is held).
unsafe extern "C" fn zb_action_handler(
    callback_id: esp_zb_core_action_callback_id_t,
    message: *const c_void,
) -> esp_err_t {
    if callback_id == esp_zb_core_action_callback_id_s_ESP_ZB_CORE_SET_ATTR_VALUE_CB_ID {
        let msg = &*(message as *const esp_zb_zcl_set_attr_value_message_t);
        if msg.info.cluster == esp_zb_zcl_cluster_id_t_ESP_ZB_ZCL_CLUSTER_ID_ANALOG_OUTPUT as u16
            && msg.attribute.id == esp_zb_zcl_analog_output_attr_t_ESP_ZB_ZCL_ATTR_ANALOG_OUTPUT_PRESENT_VALUE_ID as u16
            && !msg.attribute.data.value.is_null()
        {
            let value = *(msg.attribute.data.value as *const f32);
            match msg.info.dst_endpoint {
                INTERVAL_ENDPOINT => {
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
    0
}

unsafe fn make_basic_identify(cluster_list: *mut esp_zb_cluster_list_t) {
    let mut basic_cfg = esp_zb_basic_cluster_cfg_t {
        zcl_version: 3,
        power_source: 4, // DC source
    };
    let basic = esp_zb_basic_cluster_create(&mut basic_cfg);
    esp_zb_basic_cluster_add_attr(
        basic,
        esp_zb_zcl_basic_attr_t_ESP_ZB_ZCL_ATTR_BASIC_MANUFACTURER_NAME_ID as u16,
        MANUFACTURER.as_ptr() as *mut c_void,
    );
    esp_zb_basic_cluster_add_attr(
        basic,
        esp_zb_zcl_basic_attr_t_ESP_ZB_ZCL_ATTR_BASIC_MODEL_IDENTIFIER_ID as u16,
        MODEL.as_ptr() as *mut c_void,
    );
    static APP_VERSION: u8 = FIRMWARE_APP_VERSION;
    esp_zb_basic_cluster_add_attr(
        basic,
        esp_zb_zcl_basic_attr_t_ESP_ZB_ZCL_ATTR_BASIC_APPLICATION_VERSION_ID as u16,
        &APP_VERSION as *const u8 as *mut c_void,
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

unsafe fn make_analog_output_ep(
    ep_list: *mut esp_zb_ep_list_t,
    endpoint: u8,
    description: &'static [u8],
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
        description.as_ptr() as *mut c_void,
    );
    // min/max leak intentionally: the ZCL attribute table keeps the pointer
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

    let mut ep_cfg: esp_zb_endpoint_config_t = core::mem::zeroed();
    ep_cfg.endpoint = endpoint;
    ep_cfg.app_profile_id = 0x0104; // Home Automation profile
    ep_cfg.app_device_id = 0x000C; // Simple Sensor
    esp_zb_ep_list_add_ep(ep_list, cluster_list, ep_cfg);
}

/// Build endpoints, start the stack, and run its main loop forever.
/// Must be called from a dedicated thread.
pub fn zigbee_task() -> ! {
    unsafe {
        let mut platform_cfg: esp_zb_platform_config_t = core::mem::zeroed();
        platform_cfg.radio_config.radio_mode = esp_zb_radio_mode_t_ZB_RADIO_MODE_NATIVE;
        platform_cfg.host_config.host_connection_mode =
            esp_zb_host_connection_mode_t_ZB_HOST_CONNECTION_MODE_NONE;
        esp_zb_platform_config(&mut platform_cfg);

        let mut zb_cfg: esp_zb_cfg_t = core::mem::zeroed();
        zb_cfg.esp_zb_role = esp_zb_nwk_device_type_t_ESP_ZB_DEVICE_TYPE_ED;
        zb_cfg.install_code_policy = false;
        zb_cfg.nwk_cfg.zed_cfg.ed_timeout = esp_zb_aging_timeout_t_ESP_ZB_ED_AGING_TIMEOUT_64MIN as u8;
        zb_cfg.nwk_cfg.zed_cfg.keep_alive = 3000;
        esp_zb_init(&mut zb_cfg);

        let ep_list = esp_zb_ep_list_create();

        // EP1: temperature sensor as CO2 carrier (0–32700 "centidegrees" = ppm)
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
        ep_cfg.app_device_id = 0x0302; // Temperature Sensor
        esp_zb_ep_list_add_ep(ep_list, cluster_list, ep_cfg);

        // EP2 + EP3: analog outputs (interval / brightness)
        make_analog_output_ep(
            ep_list,
            INTERVAL_ENDPOINT,
            DESC_INTERVAL,
            INTERVAL_DEFAULT_S as f32,
            INTERVAL_MIN_S as f32,
            INTERVAL_MAX_S as f32,
        );
        make_analog_output_ep(
            ep_list,
            BRIGHTNESS_ENDPOINT,
            DESC_BRIGHTNESS,
            LED_BRIGHTNESS_DEFAULT as f32,
            0.0,
            100.0,
        );

        esp_zb_device_register(ep_list);
        esp_zb_core_action_handler_register(Some(zb_action_handler));
        esp_zb_set_primary_network_channel_set(ESP_ZB_TRANSCEIVER_ALL_CHANNELS_MASK);

        info!("[ZIGBEE] Starting stack...");
        esp_zb_start(false);
        esp_zb_stack_main_loop();
    }
    unreachable!()
}

/// Report the CO2 value (as temperature, ppm/100). Call only when connected.
pub fn report_co2(ppm: u16) {
    let value: i16 = ppm as i16; // ZCL INT16 centidegrees == ppm directly
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
            false,
        );
        let mut cmd: esp_zb_zcl_report_attr_cmd_t = core::mem::zeroed();
        cmd.zcl_basic_cmd.src_endpoint = CO2_ENDPOINT;
        cmd.address_mode =
            esp_zb_aps_address_mode_t_ESP_ZB_APS_ADDR_MODE_DST_ADDR_ENDP_NOT_PRESENT;
        cmd.clusterID = esp_zb_zcl_cluster_id_t_ESP_ZB_ZCL_CLUSTER_ID_TEMP_MEASUREMENT as u16;
        cmd.attributeID = esp_zb_zcl_temp_measurement_attr_t_ESP_ZB_ZCL_ATTR_TEMP_MEASUREMENT_VALUE_ID as u16;
        let err = esp_zb_zcl_report_attr_cmd_req(&mut cmd);
        esp_zb_lock_release();
        if err != 0 {
            warn!("[ZIGBEE] Report failed: {err}");
        }
    }
}

/// Push the current interval/brightness values so HA reads correct defaults.
pub fn report_settings() {
    unsafe {
        if !esp_zb_lock_acquire(u32::MAX) {
            return;
        }
        for (ep, value) in [
            (INTERVAL_ENDPOINT, REPORT_INTERVAL_S.load(Ordering::SeqCst) as f32),
            (BRIGHTNESS_ENDPOINT, LED_BRIGHTNESS.load(Ordering::SeqCst) as f32),
        ] {
            esp_zb_zcl_set_attribute_val(
                ep,
                esp_zb_zcl_cluster_id_t_ESP_ZB_ZCL_CLUSTER_ID_ANALOG_OUTPUT as u16,
                esp_zb_zcl_cluster_role_t_ESP_ZB_ZCL_CLUSTER_SERVER_ROLE as u8,
                esp_zb_zcl_analog_output_attr_t_ESP_ZB_ZCL_ATTR_ANALOG_OUTPUT_PRESENT_VALUE_ID as u16,
                &value as *const f32 as *mut c_void,
                false,
            );
        }
        esp_zb_lock_release();
    }
}
