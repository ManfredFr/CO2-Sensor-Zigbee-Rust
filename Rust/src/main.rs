//! # ESP32-H2 Zigbee CO2 Sensor — firmware entry point
//!
//! The firmware has three jobs, split across two threads:
//!
//! 1. **Main thread (this file):** poll the Senseair S8 CO2 sensor over
//!    UART/Modbus, drive the WS2812 status LED, and hand each reading to the
//!    Zigbee layer.
//! 2. **Zigbee thread ([`zigbee`]):** run Espressif's Zigbee stack main loop
//!    and service its callbacks (network signals, attribute writes from Home
//!    Assistant).
//!
//! The two threads communicate exclusively through atomics defined in
//! [`zigbee`] (`CONNECTED`, `REPORT_INTERVAL_S`, `LED_BRIGHTNESS`), so there
//! are no locks to get wrong on the hot path.
//!
//! ## Why raw FFI instead of esp-idf-hal drivers?
//!
//! Two esp-idf-hal 0.45 abstractions are broken on the ESP32-H2 (both are
//! chip-specific clock-source gaps, reported upstream):
//!
//! - **UART** (`UartConfig::new()` panics): the H2's default UART source
//!   clock is PLL_F48M, which has no `SourceClock` variant. The panic happens
//!   inside the config constructor, so no builder call can avoid it.
//!   → <https://github.com/esp-rs/esp-idf-hal/issues/588>
//! - **RMT** (`rmt-legacy` feature, wrong timing): `counter_clock()` derives
//!   the tick rate from a wrong base frequency on the H2, so the WS2812
//!   pulses are out of spec and the LED silently stays dark.
//!   → <https://github.com/esp-rs/esp-idf-hal/issues/589>
//!
//! Both peripherals are therefore configured through `esp-idf-sys` FFI
//! directly, which is stable and matches Espressif's C examples one-to-one.

mod zigbee;

use esp_idf_svc::hal::delay::TickType;
use esp_idf_svc::sys as idf;
use log::{info, warn};
use std::sync::atomic::Ordering;
use std::time::Duration;

const VERSION: &str = "v2.0";

// ---------------------------------------------------------------------------
// Senseair S8 (CO2 sensor, Modbus RTU over UART)
// ---------------------------------------------------------------------------

/// Modbus RTU request: read input register 3 (CO2 concentration in ppm).
///
/// Byte layout: `FE` any-address, `04` read-input-registers, `00 03` start
/// register, `00 01` register count, `D5 C5` CRC-16. The S8 answers with 7
/// bytes: `FE 04 02 <hi> <lo> <crc> <crc>` where `<hi><lo>` is the ppm value.
const S8_READ_CO2: [u8; 8] = [0xFE, 0x04, 0x00, 0x03, 0x00, 0x01, 0xD5, 0xC5];

/// UART port connected to the S8. UART0 is the boot console, so we use UART1.
const S8_UART: idf::uart_port_t = 1;

/// Configure UART1 for the S8: 9600 baud, 8 data bits, no parity, 1 stop bit,
/// GPIO4 = RX (from S8 TxD), GPIO5 = TX (to S8 RxD).
fn s8_uart_init() -> anyhow::Result<()> {
    // uart_config_t has more fields than we care about (RTS/CTS thresholds,
    // clock source, ...). Zeroing it and setting only what matters mirrors
    // the C idiom `uart_config_t cfg = { .baud_rate = 9600, ... }`.
    let mut cfg: idf::uart_config_t = unsafe { core::mem::zeroed() };
    cfg.baud_rate = 9_600;
    cfg.data_bits = idf::uart_word_length_t_UART_DATA_8_BITS;
    cfg.parity = idf::uart_parity_t_UART_PARITY_DISABLE;
    cfg.stop_bits = idf::uart_stop_bits_t_UART_STOP_BITS_1;
    cfg.flow_ctrl = idf::uart_hw_flowcontrol_t_UART_HW_FLOWCTRL_DISABLE;

    unsafe {
        idf::esp!(idf::uart_param_config(S8_UART, &cfg))?;
        // Pin argument order: TX, RX, RTS (-1 = unused), CTS (-1 = unused).
        idf::esp!(idf::uart_set_pin(S8_UART, 5, 4, -1, -1))?;
        // 256-byte RX ring buffer; no TX buffer (writes block until sent),
        // no event queue, no interrupt flags — the simplest driver mode.
        idf::esp!(idf::uart_driver_install(S8_UART, 256, 0, 0, std::ptr::null_mut(), 0))?;
    }
    Ok(())
}

/// Read up to `buf.len()` bytes from the S8 UART, waiting at most
/// `timeout_ms`. Returns the number of bytes actually read (0 on timeout).
fn s8_read(buf: &mut [u8], timeout_ms: u32) -> usize {
    let n = unsafe {
        idf::uart_read_bytes(
            S8_UART,
            buf.as_mut_ptr() as *mut core::ffi::c_void,
            buf.len() as u32,
            TickType::new_millis(timeout_ms as u64).ticks(),
        )
    };
    // uart_read_bytes returns -1 on parameter errors; fold that into
    // "nothing read" so callers only ever deal with a byte count.
    if n < 0 {
        0
    } else {
        n as usize
    }
}

/// Poll the S8 once. Returns the CO2 concentration in ppm, or `None` on
/// timeout or a malformed response (both are logged, not fatal).
fn read_co2() -> Option<u16> {
    // A previously timed-out response may still sit in the ring buffer, and
    // the S8 occasionally emits stray bytes. Start clean so the 7 bytes we
    // read below definitely belong to the request we are about to send.
    unsafe { idf::uart_flush_input(S8_UART) };

    let written = unsafe {
        idf::uart_write_bytes(
            S8_UART,
            S8_READ_CO2.as_ptr() as *const core::ffi::c_void,
            S8_READ_CO2.len(),
        )
    };
    if written != S8_READ_CO2.len() as i32 {
        warn!("[S8] UART write failed");
        return None;
    }

    // Collect exactly 7 response bytes, giving the sensor up to 1 s overall.
    // The inner 100 ms timeout keeps this loop responsive without spinning.
    let mut resp = [0u8; 7];
    let mut got = 0;
    let deadline = std::time::Instant::now() + Duration::from_millis(1000);
    while got < 7 {
        if std::time::Instant::now() > deadline {
            warn!("[S8] Timeout — {got}/7 bytes received");
            return None;
        }
        got += s8_read(&mut resp[got..], 100);
    }

    info!(
        "[S8] Response: {:02X} {:02X} {:02X} {:02X} {:02X} {:02X} {:02X}",
        resp[0], resp[1], resp[2], resp[3], resp[4], resp[5], resp[6]
    );

    // Sanity-check the Modbus header: same address (FE), same function (04),
    // 2 payload bytes. A CRC check would be stricter, but in practice a
    // corrupted frame fails the header check first, and an off value would
    // be corrected by the next report one interval later anyway.
    if resp[0] != 0xFE || resp[1] != 0x04 || resp[2] != 0x02 {
        warn!("[S8] Bad response header");
        return None;
    }
    Some(((resp[3] as u16) << 8) | resp[4] as u16)
}

// ---------------------------------------------------------------------------
// WS2812 status LED (RMT peripheral)
// ---------------------------------------------------------------------------

/// Handles for the RMT TX channel and the bytes encoder that translates an
/// `[r, g, b]` payload into a WS2812 pulse train.
struct Ws2812 {
    channel: idf::rmt_channel_handle_t,
    encoder: idf::rmt_encoder_handle_t,
}

/// Build one RMT symbol word. A symbol is two (duration, level) pairs packed
/// into 32 bits: `duration0` (15 bits) | `level0` (1 bit) | `duration1`
/// (15 bits) | `level1` (1 bit). Durations are in RMT ticks (100 ns here).
const fn rmt_symbol(d0: u32, l0: u32, d1: u32, l1: u32) -> idf::rmt_symbol_word_t {
    // rmt_symbol_word_t is a bindgen union of bitfields; writing the packed
    // u32 through `.val` is the simplest const-friendly way to fill it.
    let mut s: idf::rmt_symbol_word_t = unsafe { core::mem::transmute(0u32) };
    s.val = d0 | (l0 << 15) | (d1 << 16) | (l1 << 31);
    s
}

/// Set up the RMT TX channel on GPIO8 and a bytes encoder with WS2812 bit
/// timings.
///
/// WS2812 encodes each bit as a high pulse followed by a low pulse: a `0` is
/// ~300 ns high + ~900 ns low, a `1` is ~900 ns high + ~300 ns low (with
/// generous ±150 ns tolerances). At the chosen 10 MHz RMT resolution one
/// tick = 100 ns, so those are 3/9 and 9/3 ticks. The >50 µs reset latch the
/// LED needs after a frame happens naturally in the idle gap between our
/// transmissions.
fn ws2812_init() -> anyhow::Result<Ws2812> {
    unsafe {
        let mut ch_cfg: idf::rmt_tx_channel_config_t = core::mem::zeroed();
        ch_cfg.gpio_num = 8; // onboard WS2812 of the DevKit
        ch_cfg.clk_src = idf::soc_periph_rmt_clk_src_t_RMT_CLK_SRC_DEFAULT;
        ch_cfg.resolution_hz = 10_000_000; // 100 ns per tick
        ch_cfg.mem_block_symbols = 48; // hardware minimum; one LED needs 24
        ch_cfg.trans_queue_depth = 4;
        let mut channel: idf::rmt_channel_handle_t = core::ptr::null_mut();
        idf::esp!(idf::rmt_new_tx_channel(&ch_cfg, &mut channel))?;

        // The bytes encoder walks the payload bit by bit and emits `bit0` or
        // `bit1` symbols. WS2812 expects the most significant bit first.
        let mut enc_cfg: idf::rmt_bytes_encoder_config_t = core::mem::zeroed();
        enc_cfg.bit0 = rmt_symbol(3, 1, 9, 0); // 0-bit: 300 ns high, 900 ns low
        enc_cfg.bit1 = rmt_symbol(9, 1, 3, 0); // 1-bit: 900 ns high, 300 ns low
        enc_cfg.flags.set_msb_first(1);
        let mut encoder: idf::rmt_encoder_handle_t = core::ptr::null_mut();
        idf::esp!(idf::rmt_new_bytes_encoder(&enc_cfg, &mut encoder))?;

        idf::esp!(idf::rmt_enable(channel))?;
        Ok(Ws2812 { channel, encoder })
    }
}

/// Show a color on the LED, scaled by `brightness` percent (0 turns it off).
///
/// Note: this board's LED expects **RGB byte order on the wire**, unlike the
/// standard WS2812 GRB order — sending GRB shows red and green swapped.
fn set_led(led: &Ws2812, r: u8, g: u8, b: u8, brightness: u32) {
    let scale = |c: u8| ((c as u32 * brightness) / 100) as u8;
    let bytes = [scale(r), scale(g), scale(b)];
    unsafe {
        let tx_cfg: idf::rmt_transmit_config_t = core::mem::zeroed();
        let err = idf::rmt_transmit(
            led.channel,
            led.encoder,
            bytes.as_ptr() as *const core::ffi::c_void,
            bytes.len(),
            &tx_cfg,
        );
        if err != 0 {
            warn!("[LED] transmit failed: {err}");
            return;
        }
        // Block until the 24 bits are on the wire (a few tens of µs; the
        // 100-tick timeout is just a safety net). Waiting also keeps `bytes`
        // alive for the duration of the transfer — rmt_transmit reads the
        // payload asynchronously.
        idf::rmt_tx_wait_all_done(led.channel, 100);
    }
}

/// Map a CO2 reading to the indicator color (see the README's LED table).
fn co2_color(ppm: u16) -> (u8, u8, u8) {
    match ppm {
        0..=1000 => (0, 120, 0),     // green — good
        1001..=2000 => (200, 50, 0), // orange — fair/poor
        _ => (220, 0, 0),            // red — bad
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> anyhow::Result<()> {
    // Standard esp-idf-svc preamble: apply runtime patches the Rust std port
    // needs, and route `log` macros to the ESP-IDF logger (visible over USB).
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    info!("========================================");
    info!("  Co2-Sensor {VERSION} (Rust)");
    info!("========================================");

    s8_uart_init()?;
    info!("[INIT] S8 UART started (RX=GPIO4 TX=GPIO5 @ 9600)");

    let led = ws2812_init()?;

    // Brief white flash so the LED is visibly alive at startup — doubles as
    // a quick field check that the RMT timing is right.
    set_led(&led, 30, 30, 30, 100);
    std::thread::sleep(Duration::from_millis(500));
    set_led(&led, 0, 0, 0, 100);
    info!("[INIT] LED ready (GPIO8)");

    // The Zigbee stack owns its thread: esp_zb_stack_main_loop() never
    // returns. All interaction with it happens under the Zigbee lock (taken
    // inside zigbee.rs) or through the shared atomics.
    std::thread::Builder::new()
        .name("zigbee".into())
        .stack_size(8192)
        .spawn(zigbee::zigbee_task)?;

    // Nothing useful can happen until we're on the network — readings taken
    // before the join would be dropped anyway — so just wait.
    info!("[ZIGBEE] Waiting to join network...");
    while !zigbee::CONNECTED.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(100));
    }
    // Push the current settings once, so Home Assistant shows real values
    // right after the join instead of nulls.
    zigbee::report_settings();
    info!("[ZIGBEE] Initial interval and brightness reported");

    let mut read_count = 0u32;
    let mut fail_count = 0u32;

    loop {
        read_count += 1;
        // Interval and brightness can be changed from Home Assistant at any
        // moment (the Zigbee thread updates the atomics), so re-read them
        // every iteration instead of caching.
        let interval =
            Duration::from_secs(zigbee::REPORT_INTERVAL_S.load(Ordering::SeqCst) as u64);
        let brightness = zigbee::LED_BRIGHTNESS.load(Ordering::SeqCst);
        info!("[LOOP] Read #{read_count} (interval: {}s)", interval.as_secs());

        match read_co2() {
            Some(ppm) => {
                info!("[CO2] {ppm} ppm — reporting via Zigbee");
                // CONNECTED can flip back to false if the device leaves or
                // loses the network; skip reporting then, but keep the LED
                // honest either way.
                if zigbee::CONNECTED.load(Ordering::SeqCst) {
                    zigbee::report_co2(ppm);
                }

                if ppm > 5000 {
                    // Dangerous level: flash red for the whole interval
                    // instead of showing a steady color.
                    let end = std::time::Instant::now() + interval;
                    let mut on = true;
                    while std::time::Instant::now() < end {
                        if on {
                            set_led(&led, 220, 0, 0, brightness);
                        } else {
                            set_led(&led, 0, 0, 0, brightness);
                        }
                        on = !on;
                        std::thread::sleep(Duration::from_millis(500));
                    }
                } else {
                    let (r, g, b) = co2_color(ppm);
                    set_led(&led, r, g, b, brightness);
                    std::thread::sleep(interval);
                }
            }
            None => {
                // Sensor didn't answer (warming up, unplugged, wiring issue).
                // Keep the previous LED color — a stale indication beats a
                // dark LED that looks like a power failure — and retry after
                // the normal interval.
                fail_count += 1;
                warn!("[CO2] Read failed (fail #{fail_count})");
                std::thread::sleep(interval);
            }
        }
    }
}
