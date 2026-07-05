mod zigbee;

use esp_idf_svc::hal::delay::TickType;
use esp_idf_svc::sys as idf;
use log::{info, warn};
use std::time::Duration;

const VERSION: &str = "v2.0";

// Senseair S8 Modbus RTU: read input register 3 (CO2 ppm)
const S8_READ_CO2: [u8; 8] = [0xFE, 0x04, 0x00, 0x03, 0x00, 0x01, 0xD5, 0xC5];


// WS2812 on GPIO8 via the new ESP-IDF RMT TX driver (raw FFI — esp-idf-hal's
// legacy RMT driver derives wrong pulse timing on the ESP32-H2, same class of
// clock bug as its UART config). 10 MHz resolution → 1 tick = 100 ns.
struct Ws2812 {
    channel: idf::rmt_channel_handle_t,
    encoder: idf::rmt_encoder_handle_t,
}

// One RMT symbol: duration0(15b) | level0(1b) | duration1(15b) | level1(1b)
const fn rmt_symbol(d0: u32, l0: u32, d1: u32, l1: u32) -> idf::rmt_symbol_word_t {
    let mut s: idf::rmt_symbol_word_t = unsafe { core::mem::transmute(0u32) };
    s.val = d0 | (l0 << 15) | (d1 << 16) | (l1 << 31);
    s
}

fn ws2812_init() -> anyhow::Result<Ws2812> {
    unsafe {
        let mut ch_cfg: idf::rmt_tx_channel_config_t = core::mem::zeroed();
        ch_cfg.gpio_num = 8;
        ch_cfg.clk_src = idf::soc_periph_rmt_clk_src_t_RMT_CLK_SRC_DEFAULT;
        ch_cfg.resolution_hz = 10_000_000; // 100 ns per tick
        ch_cfg.mem_block_symbols = 48;
        ch_cfg.trans_queue_depth = 4;
        let mut channel: idf::rmt_channel_handle_t = core::ptr::null_mut();
        idf::esp!(idf::rmt_new_tx_channel(&ch_cfg, &mut channel))?;

        let mut enc_cfg: idf::rmt_bytes_encoder_config_t = core::mem::zeroed();
        enc_cfg.bit0 = rmt_symbol(3, 1, 9, 0); // 0: 300 ns high, 900 ns low
        enc_cfg.bit1 = rmt_symbol(9, 1, 3, 0); // 1: 900 ns high, 300 ns low
        enc_cfg.flags.set_msb_first(1);
        let mut encoder: idf::rmt_encoder_handle_t = core::ptr::null_mut();
        idf::esp!(idf::rmt_new_bytes_encoder(&enc_cfg, &mut encoder))?;

        idf::esp!(idf::rmt_enable(channel))?;
        Ok(Ws2812 { channel, encoder })
    }
}

fn set_led(led: &Ws2812, r: u8, g: u8, b: u8, brightness: u32) {
    let scale = |c: u8| ((c as u32 * brightness) / 100) as u8;
    // RGB byte order on the wire for this board (not the usual GRB)
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
        idf::rmt_tx_wait_all_done(led.channel, 100);
    }
}

const S8_UART: idf::uart_port_t = 1;

// Raw ESP-IDF UART driver. esp-idf-hal 0.45's UartConfig panics on the
// ESP32-H2 (its default source clock PLL_F48M has no SourceClock variant),
// so the driver is set up via FFI directly: UART1, 9600 8N1, GPIO4 RX / GPIO5 TX.
fn s8_uart_init() -> anyhow::Result<()> {
    let mut cfg: idf::uart_config_t = unsafe { core::mem::zeroed() };
    cfg.baud_rate = 9_600;
    cfg.data_bits = idf::uart_word_length_t_UART_DATA_8_BITS;
    cfg.parity = idf::uart_parity_t_UART_PARITY_DISABLE;
    cfg.stop_bits = idf::uart_stop_bits_t_UART_STOP_BITS_1;
    cfg.flow_ctrl = idf::uart_hw_flowcontrol_t_UART_HW_FLOWCTRL_DISABLE;
    unsafe {
        idf::esp!(idf::uart_param_config(S8_UART, &cfg))?;
        idf::esp!(idf::uart_set_pin(S8_UART, 5, 4, -1, -1))?; // TX=5 RX=4
        idf::esp!(idf::uart_driver_install(S8_UART, 256, 0, 0, std::ptr::null_mut(), 0))?;
    }
    Ok(())
}

fn s8_read(buf: &mut [u8], timeout_ms: u32) -> usize {
    let n = unsafe {
        idf::uart_read_bytes(
            S8_UART,
            buf.as_mut_ptr() as *mut core::ffi::c_void,
            buf.len() as u32,
            TickType::new_millis(timeout_ms as u64).ticks(),
        )
    };
    if n < 0 { 0 } else { n as usize }
}

// Read CO2 ppm from the S8 over Modbus RTU. Returns None on timeout/bad frame.
fn read_co2() -> Option<u16> {
    unsafe { idf::uart_flush_input(S8_UART) }; // drop stale RX bytes

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

    if resp[0] != 0xFE || resp[1] != 0x04 || resp[2] != 0x02 {
        warn!("[S8] Bad response header");
        return None;
    }
    Some(((resp[3] as u16) << 8) | resp[4] as u16)
}

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    info!("========================================");
    info!("  Co2-Sensor {VERSION} (Rust)");
    info!("========================================");

    // S8 UART: GPIO4 = RX (← S8 TX), GPIO5 = TX (→ S8 RX), 9600 8N1
    s8_uart_init()?;
    info!("[INIT] S8 UART started (RX=GPIO4 TX=GPIO5 @ 9600)");

    // WS2812 LED on GPIO8 via RMT
    let led = ws2812_init()?;

    // Boot flash: brief white
    set_led(&led, 30, 30, 30, 100);
    std::thread::sleep(Duration::from_millis(500));
    set_led(&led, 0, 0, 0, 100);
    info!("[INIT] LED ready (GPIO8)");

    // Zigbee stack runs its own main loop in a dedicated thread
    std::thread::Builder::new()
        .name("zigbee".into())
        .stack_size(8192)
        .spawn(zigbee::zigbee_task)?;

    info!("[ZIGBEE] Waiting to join network...");
    while !zigbee::CONNECTED.load(std::sync::atomic::Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(100));
    }
    zigbee::report_settings();
    info!("[ZIGBEE] Initial interval and brightness reported");

    let mut read_count = 0u32;
    let mut fail_count = 0u32;

    loop {
        read_count += 1;
        let interval = Duration::from_secs(
            zigbee::REPORT_INTERVAL_S.load(std::sync::atomic::Ordering::SeqCst) as u64,
        );
        let brightness = zigbee::LED_BRIGHTNESS.load(std::sync::atomic::Ordering::SeqCst);
        info!("[LOOP] Read #{read_count} (interval: {}s)", interval.as_secs());

        match read_co2() {
            Some(ppm) => {
                info!("[CO2] {ppm} ppm — reporting via Zigbee");
                if zigbee::CONNECTED.load(std::sync::atomic::Ordering::SeqCst) {
                    zigbee::report_co2(ppm);
                }
                if ppm > 5000 {
                    // Dangerous: flash red for the whole interval
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
                    let (r, g, b) = match ppm {
                        0..=1000 => (0, 120, 0),    // green — good
                        1001..=2000 => (200, 50, 0), // orange — fair/poor
                        _ => (220, 0, 0),            // red — bad
                    };
                    set_led(&led, r, g, b, brightness);
                    std::thread::sleep(interval);
                }
            }
            None => {
                fail_count += 1;
                warn!("[CO2] Read failed (fail #{fail_count})");
                std::thread::sleep(interval);
            }
        }
    }
}
