# Changelog

## v2.0.1 — Current (5 July 2026)
- Standalone mode: measuring and the LED indicator start immediately at
  boot instead of waiting for a Zigbee join, so the sensor is usable with
  no network in range (e.g. in a car). Zigbee keeps steering in the
  background and reporting starts whenever a join succeeds.
- Versioning switched to MAJOR.MINOR.PATCH (patch visible in swBuildId).

## v2.0 (5 July 2026) — Rust port
- Firmware fully ported from Arduino C++ to Rust (`Rust/`), running on
  std/ESP-IDF with Espressif's Zigbee SDK via esp-idf-sys FFI bindings
- Same three endpoints and same Z2M converter (`Co2-Sensor.js`) as v1.3 —
  verified end-to-end: CO2 reporting, interval + brightness read/write
- Real S8 readings over Modbus RTU (no more simulation)
- CO2 reporting now relies on ZCL configured reporting (attribute set only);
  explicit report commands removed — they assert inside the stack when sent
  before the Z2M binding exists (`zcl_general_commands.c:612`)
- Rust-specific lessons (esp-idf-hal UART panic on H2, spaces-in-path
  workarounds, linker flags) documented in `Rust/README.md`

## v1.2 (31 May 2026)
- CO2 reporting working end-to-end via Zigbee2MQTT
- Used ZigbeeTempSensor as carrier for CO2 value (ppm stored as ppm/100.0°C)
- Converter `configure` section added to trigger Z2M binding + Configure Reporting command on join
- Report interval slider (10–300s) working via ZigbeeAnalog endpoint 2
- Simulated CO2 readings (400–1200 ppm) active until real S8 sensor is wired up

## Key Lessons Learned

### ZigbeeTempSensor as CO2 carrier
The `genAnalogInput` cluster's `presentValue` attribute is not marked reportable by the Arduino library, causing `esp_zb_zcl_report_attr_cmd_req` to assert. Used `ZigbeeTempSensor` instead — its `measuredValue` attribute IS reportable. CO2 ppm stored as `ppm/100.0` so ZCL INT16 = ppm directly. Converter reads `msg.data.measuredValue` as raw ppm.

### Z2M Configure Reporting required
Without a `configure` section in the converter, Z2M never sends a Configure Reporting command to the device. The device's `reportTemperature()` uses `DST_ADDR_ENDP_NOT_PRESENT` which requires a binding. The `configure` section binds endpoint 1 to the coordinator and configures reporting, which establishes the binding automatically on join.

### Zigbee stack lock assertion
`esp_zb_zcl_set_attribute_val` and `esp_zb_zcl_report_attr_cmd_req` must not be called before the Zigbee stack is fully running. Call them only after `Zigbee.connected()` returns true. Never call Zigbee API functions from inside a ZCL write callback (e.g. `onAnalogOutputChange`) — the stack lock is already held.

## v0.1 (31 May 2026)
- Initial project scaffold
