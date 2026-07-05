use esp_idf_svc::hal::delay::TickType;
use esp_idf_svc::hal::gpio::AnyIOPin;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::hal::prelude::*;
use esp_idf_svc::hal::rmt::{FixedLengthSignal, PinState, Pulse, TxRmtDriver};
use esp_idf_svc::hal::uart::{UartConfig, UartDriver};
use log::{info, warn};
use std::time::Duration;

const VERSION: &str = "v2.0";

// Senseair S8 Modbus RTU: read input register 3 (CO2 ppm)
const S8_READ_CO2: [u8; 8] = [0xFE, 0x04, 0x00, 0x03, 0x00, 0x01, 0xD5, 0xC5];

const LED_BRIGHTNESS_DEFAULT: u32 = 50;
const INTERVAL_DEFAULT_S: u32 = 30;

// Send one WS2812 frame via RMT. This board's LED uses RGB byte order.
fn ws2812_write(tx: &mut TxRmtDriver, r: u8, g: u8, b: u8) -> anyhow::Result<()> {
    let ticks_hz = tx.counter_clock()?;
    let t0h = Pulse::new_with_duration(ticks_hz, PinState::High, &Duration::from_nanos(350))?;
    let t0l = Pulse::new_with_duration(ticks_hz, PinState::Low, &Duration::from_nanos(800))?;
    let t1h = Pulse::new_with_duration(ticks_hz, PinState::High, &Duration::from_nanos(700))?;
    let t1l = Pulse::new_with_duration(ticks_hz, PinState::Low, &Duration::from_nanos(600))?;

    let mut signal = FixedLengthSignal::<24>::new();
    // RGB byte order on the wire for this board
    let color: u32 = ((r as u32) << 16) | ((g as u32) << 8) | b as u32;
    for i in (0..24).rev() {
        let bit = (color >> i) & 1 == 1;
        let (h, l) = if bit { (t1h, t1l) } else { (t0h, t0l) };
        signal.set(23 - i as usize, &(h, l))?;
    }
    tx.start_blocking(&signal)?;
    Ok(())
}

fn set_led(tx: &mut TxRmtDriver, r: u8, g: u8, b: u8, brightness: u32) {
    let scale = |c: u8| ((c as u32 * brightness) / 100) as u8;
    if let Err(e) = ws2812_write(tx, scale(r), scale(g), scale(b)) {
        warn!("[LED] write failed: {e}");
    }
}

// Read CO2 ppm from the S8 over Modbus RTU. Returns None on timeout/bad frame.
fn read_co2(uart: &UartDriver) -> Option<u16> {
    // Flush stale RX bytes
    let mut discard = [0u8; 16];
    while uart.read(&mut discard, TickType::new_millis(0).ticks()).unwrap_or(0) > 0 {}

    if uart.write(&S8_READ_CO2).is_err() {
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
        got += uart
            .read(&mut resp[got..], TickType::new_millis(100).ticks())
            .unwrap_or(0);
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

    let p = Peripherals::take()?;

    // S8 UART: GPIO4 = RX (← S8 TX), GPIO5 = TX (→ S8 RX), 9600 8N1
    let uart = UartDriver::new(
        p.uart1,
        p.pins.gpio5, // TX
        p.pins.gpio4, // RX
        Option::<AnyIOPin>::None,
        Option::<AnyIOPin>::None,
        &UartConfig::new().baudrate(Hertz(9_600)),
    )?;
    info!("[INIT] S8 UART started (RX=GPIO4 TX=GPIO5 @ 9600)");

    // WS2812 LED on GPIO8 via RMT
    let mut led = TxRmtDriver::new(p.rmt.channel0, p.pins.gpio8, &Default::default())?;

    // Boot flash: brief white
    set_led(&mut led, 30, 30, 30, 100);
    std::thread::sleep(Duration::from_millis(500));
    set_led(&mut led, 0, 0, 0, 100);
    info!("[INIT] LED ready (GPIO8)");

    let brightness = LED_BRIGHTNESS_DEFAULT;
    let interval = Duration::from_secs(INTERVAL_DEFAULT_S as u64);
    let mut read_count = 0u32;
    let mut fail_count = 0u32;

    loop {
        read_count += 1;
        info!("[LOOP] Read #{read_count}");

        match read_co2(&uart) {
            Some(ppm) => {
                info!("[CO2] {ppm} ppm");
                if ppm > 5000 {
                    // Dangerous: flash red for the whole interval
                    let end = std::time::Instant::now() + interval;
                    let mut on = true;
                    while std::time::Instant::now() < end {
                        if on {
                            set_led(&mut led, 220, 0, 0, brightness);
                        } else {
                            set_led(&mut led, 0, 0, 0, brightness);
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
                    set_led(&mut led, r, g, b, brightness);
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
