# ESP32-H2 CO2 Sensor — Rust Firmware

Rust firmware for the ESP32-H2-DevKit-N4 with a Senseair S8 CO2 sensor.
Uses `esp-hal` (no_std) with the Embassy async executor.

> **Status:** dummy mode — simulates CO2 readings every 5 s. Real S8 UART reads not yet implemented.

## Wiring

| S8 Pin   | ESP32-H2 J1 Pin | Notes            |
|----------|-----------------|------------------|
| VCC (G+) | 5V (pin 14)     | Power            |
| GND (G0) | GND (pin 13)    | Ground           |
| TxD (TX) | GPIO4 (pin 9)   | Sensor → ESP     |
| RxD (RX) | GPIO5 (pin 10)  | ESP → Sensor     |

S8 runs on 5V; UART logic is 3.3V — no level shifter needed.
All four connections are on the J1 (left) header.

## Prerequisites

### Homebrew

```bash
brew install rustup ninja cmake libusb
rustup-init   # accept defaults, then restart your shell
```

> You may already have `rust` via Homebrew. `rustup` is also needed to manage
> toolchain targets — run `rustup update stable` if the installed version is old.

### Rust toolchain & tools

```bash
# RISC-V target for ESP32-H2 (no Xtensa fork needed — H2 is RISC-V)
rustup target add riscv32imac-unknown-none-elf

# Flashing tool — pin to v3.x; v4.x has an app-descriptor validation bug
# with esp-hal 1.0.0-rc.0 that requires --ignore-app-descriptor at flash time
cargo install espflash --version "^4" --locked
```

### Verify

```bash
rustup target list --installed | grep riscv
# should show: riscv32imac-unknown-none-elf
```

## Build & Flash

```bash
# Build (use rustup's cargo, not the Homebrew one)
PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" \
  cargo build --release

# Flash
espflash flash \
  --port /dev/tty.usbmodem312401 \
  --ignore-app-descriptor \
  target/riscv32imac-unknown-none-elf/release/co2-sensor
```

> `--ignore-app-descriptor` is required because esp-hal 1.0.0-rc.0 places the
> descriptor in `.rodata_desc` (not `.flash.appdesc`) — espflash 4.x looks for
> the latter. The bootloader finds the descriptor correctly at runtime.

### Monitor serial output

```bash
python3 -c "
import serial, time
s = serial.Serial('/dev/tty.usbmodem312401', 115200, timeout=1)
for _ in range(60):
    line = s.readline()
    if line: print(line.decode('utf-8', errors='replace').rstrip())
s.close()
"
```

Expected output (cycling every 5 s):
```
INFO - CO2 sensor booted (dummy mode)
INFO - CO2: 412 ppm
INFO - CO2: 480 ppm
...
```

### Finding the port

```bash
ls /dev/tty.usbmodem* 2>/dev/null
```

The ESP32-H2 DevKit-N4 uses the native USB-Serial port (`tty.usbmodem*`).
The auto-reset via DTR/RTS does **not** work on native USB — if espflash
fails to connect, hold **BOOT**, tap **RST**, release BOOT to enter
bootloader mode manually.

## Dependency notes

esp-hal uses tightly coupled version pins:

| Crate             | Version      | Reason                              |
|-------------------|--------------|-------------------------------------|
| esp-hal           | 1.0.0-rc.0   | Forced by esp-hal-embassy 0.9.1     |
| esp-hal-embassy   | 0.9.1        | Latest published; requires rc.0     |
| embassy-executor  | 0.7          | Version bundled by esp-hal-embassy  |
| embassy-time      | 0.4          | Matches executor 0.7                |
| esp-backtrace     | 0.19         | Latest; `exception-handler` removed |
| esp-println       | 0.17         | `log` feature renamed to `log-04`   |

## App descriptor

The ESP-IDF v5.x second-stage bootloader requires an `esp_app_desc_t` struct
at flash offset `app_partition_start + 0x20`. It is placed manually in
`.rodata_desc` (the first section in DROM per the esp-hal linker script) with
magic word `0xABCD5432` (changed from `0xABCD5AA5` in v4.x).

## Project structure

```
src/main.rs         — firmware entry point + app descriptor + dummy CO2 loop
Cargo.toml          — dependencies
.cargo/config.toml  — RISC-V target + linker flags
rust-toolchain.toml — pins to stable toolchain
README.md           — this file
```
