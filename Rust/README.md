# ESP32-H2 CO2 Sensor — Rust Firmware (v2.0)

Rust port of the Arduino firmware: a Zigbee End Device on the ESP32-H2-DevKit-N4
with a Senseair S8 CO2 sensor, integrated into Home Assistant via Zigbee2MQTT.

Built on **std / ESP-IDF** (`esp-idf-svc` + `esp-idf-sys`) with Espressif's
closed-source Zigbee stack (`esp-zigbee-lib` 1.6) pulled in as an ESP-IDF
component; bindgen generates the `esp_zb_*` FFI (module `esp_idf_svc::sys::zb`).

> The earlier `no_std`/esp-hal prototype was replaced: the Zigbee stack is a
> C library that requires ESP-IDF/FreeRTOS, which means `std`.

## Endpoints (unchanged from Arduino — same Z2M converter `../Co2-Sensor.js`)

| EP | Cluster | Purpose |
|---|---|---|
| 1 | Temperature Measurement | CO2 carrier: ppm stored as ppm/100 °C, ZCL INT16 = ppm |
| 2 | Analog Output | Report interval 10–300 s (read/write) |
| 3 | Analog Output | LED brightness 0–100 % (read/write) |

## Wiring

| S8 Pin | ESP32-H2 J1 Pin | Notes |
|---|---|---|
| VCC (G+) | 5V (pin 14) | Power |
| GND (G0) | GND (pin 13) | Ground |
| TxD | GPIO4 (pin 9) | Sensor → ESP |
| RxD | GPIO5 (pin 10) | ESP → Sensor |

WS2812 RGB LED on GPIO8 (RGB byte order on this board, not GRB).

## Prerequisites

```bash
brew install rustup ninja cmake
rustup toolchain install nightly --component rust-src
cargo install ldproxy espflash
```

## Build

```bash
# rustup's proxies must shadow Homebrew's rust (build-std needs nightly)
export PATH="$(brew --prefix rustup)/bin:$PATH"
cargo build --release
```

First build downloads ESP-IDF v5.3.3 (~1 GB) into `~/.espressif` plus the
Zigbee components, and takes a while. Subsequent builds are fast.

### Spaces-in-path workaround (important)

ESP-IDF's build system cannot handle spaces in paths, and this project lives
under Dropbox. Therefore:

- the cargo target dir is `/Users/manfred/.cache/co2-sensor-rust/target`
  (set in `.cargo/config.toml`)
- esp-idf-sys treats the **parent of the target dir** as the workspace anchor:
  it runs `cargo metadata` there and resolves `sdkconfig.defaults` relative to
  it. Symlinks in `~/.cache/co2-sensor-rust/` (`Cargo.toml`, `Cargo.lock`,
  `src`, `build.rs`, `bindings.h`, `sdkconfig.defaults`, `partitions.csv`)
  make that work. Without them the Zigbee components are silently skipped.

Recreate the symlinks if the cache dir is ever deleted:

```bash
P="$(pwd)"; W=~/.cache/co2-sensor-rust; mkdir -p "$W"
for f in Cargo.toml Cargo.lock src build.rs bindings.h sdkconfig.defaults partitions.csv; do
  ln -sfn "$P/$f" "$W/$f"
done
```

## Flash

```bash
OUT=$(ls -d ~/.cache/co2-sensor-rust/target/riscv32imac-esp-espidf/release/build/esp-idf-sys-*/out | head -1)
espflash flash --port /dev/cu.usbmodem* --chip esp32h2 \
  --bootloader "$OUT/build/bootloader/bootloader.bin" \
  --partition-table partitions.csv \
  ~/.cache/co2-sensor-rust/target/riscv32imac-esp-espidf/release/co2-sensor
```

The partition table adds the `zb_storage`/`zb_fct` partitions the Zigbee stack
requires (equivalent of Arduino's "Zigbee 4MB" scheme).

## Monitor

```bash
python3 -c "
import serial
s = serial.Serial('/dev/cu.usbmodem3111401', 115200, timeout=1)
s.setDTR(False); s.setRTS(False)
while True:
    line = s.readline()
    if line: print(line.decode(errors='replace').rstrip())
"
```

## Hard-won lessons (Rust-specific; ZCL lessons are in ../CHANGELOG.md)

- **esp-idf-hal 0.45 UART panics on ESP32-H2**: the H2's default UART source
  clock (PLL_F48M) has no `SourceClock` variant, and `UartConfig::new()` hits
  `unreachable!()` before any builder method can override it. The firmware
  uses raw ESP-IDF UART FFI instead (`uart_param_config` & co).
- **esp-idf-hal's legacy RMT driver produces wrong WS2812 timing on the H2**
  (same clock-assumption class of bug — LED silently stays dark). The
  firmware uses the new RMT TX driver via FFI (`rmt_new_tx_channel` +
  `rmt_new_bytes_encoder` at 10 MHz resolution) instead of the `rmt-legacy`
  feature.
- **Zigbee join must not race explicit reports**: an explicit
  `esp_zb_zcl_report_attr_cmd_req` before Z2M's converter `configure` has
  created the binding asserts inside the closed-source stack
  (`zcl_general_commands.c:612`) and reboots the device. Only set attributes
  with `esp_zb_zcl_set_attribute_val`; the stack auto-reports once reporting
  is configured.
- **`esp_zb_app_signal_handler` is resolved by the linker**: define it as
  `#[no_mangle] extern "C"`. The signal type is `*(*signal).p_app_signal`
  (single deref — a double deref reads garbage and load-faults).
- **`--undefined=vsnprintf` link flag**: the closed-source phy lib (`-lphy`)
  is scanned after the last `-lc` and needs vsnprintf; the early marker makes
  libc donate the symbol (see `.cargo/config.toml`).
- **esp-zigbee-lib must be pinned `^1.6`**: version `*` resolves to 2.x with a
  restructured API ("ezbee") that matches no documentation or examples.

## Project structure

```
src/main.rs         — app entry, S8 Modbus reads, WS2812 LED, main loop
src/zigbee.rs       — Zigbee endpoints, signal/action handlers, reporting
bindings.h          — headers exposed to bindgen (esp_zigbee_core.h & co)
sdkconfig.defaults  — Zigbee ED role, 4MB flash, custom partition table
partitions.csv      — adds zb_storage / zb_fct partitions
.cargo/config.toml  — target, ldproxy, build-std, workarounds (see comments)
```
