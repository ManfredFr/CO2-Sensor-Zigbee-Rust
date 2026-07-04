#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_hal::gpio::Level;
use esp_hal::rmt::{PulseCode, Rmt, TxChannel, TxChannelConfig, TxChannelCreator};
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_hal::uart::{Config as UartConfig, Uart};
use log::{info, warn};

// ESP-IDF v5.x app descriptor — must be the first symbol in DROM (.rodata_desc)
// so the second-stage bootloader can find it at app partition offset +0x20.
// Magic word changed from 0xABCD5AA5 (v4.x) to 0xABCD5432 (v5.x).
#[repr(C)]
struct EspAppDesc {
    magic_word:           u32,
    secure_version:       u32,
    reserv1:              [u32; 2],
    version:              [u8; 32],
    project_name:         [u8; 32],
    time:                 [u8; 16],
    date:                 [u8; 16],
    idf_ver:              [u8; 32],
    app_elf_sha256:       [u8; 32],
    min_efuse_blk_rev:    u16,
    max_efuse_blk_rev:    u16,
    mmu_page_size:        u8,
    reserv3:              [u8; 3],
    reserv2:              [u32; 18],
}

#[unsafe(link_section = ".rodata_desc")]
#[unsafe(no_mangle)]
static ESP_APP_DESC: EspAppDesc = EspAppDesc {
    magic_word:        0xABCD5432,
    secure_version:    0,
    reserv1:           [0; 2],
    version:           *b"0.3.0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0",
    project_name:      *b"co2-sensor\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0",
    time:              [0; 16],
    date:              [0; 16],
    idf_ver:           [0; 32],
    app_elf_sha256:    [0; 32],
    min_efuse_blk_rev: 0,
    max_efuse_blk_rev: 0xFFFF,
    mmu_page_size:     0,
    reserv3:           [0; 3],
    reserv2:           [0; 18],
};

// Senseair S8 Modbus RTU: read input register 3 (CO2 concentration, 1 register)
// Address 0xFE = broadcast, function 0x04 = read input registers
const S8_READ_CO2: [u8; 8] = [0xFE, 0x04, 0x00, 0x03, 0x00, 0x01, 0xD5, 0xC5];

// WS2812B on GPIO8 — RMT base 32 MHz, clk_divider=4 → 8 MHz (125 ns/tick)
// This board's LED uses RGB byte order (not the standard WS2812 GRB order).
// T0: high=3 (375 ns), low=7 (875 ns)
// T1: high=6 (750 ns), low=4 (500 ns)
// Reset: low=400 ticks (50 µs)
fn ws2812_frame(r: u8, g: u8, b: u8) -> [u32; 26] {
    let mut data = [PulseCode::empty(); 26];
    let mut i = 0usize;
    for byte in [r, g, b] {
        for bit in (0..8).rev() {
            data[i] = if (byte >> bit) & 1 == 1 {
                PulseCode::new(Level::High, 6, Level::Low, 4)
            } else {
                PulseCode::new(Level::High, 3, Level::Low, 7)
            };
            i += 1;
        }
    }
    // Reset pulse: hold low for >50 µs (400 ticks at 125 ns = 50 µs)
    data[24] = PulseCode::new(Level::Low, 400, Level::Low, 0);
    data[25] = PulseCode::empty();
    data
}

#[esp_hal_embassy::main]
async fn main(_spawner: Spawner) {
    let peripherals = esp_hal::init(esp_hal::Config::default());
    esp_println::logger::init_logger(log::LevelFilter::Info);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_hal_embassy::init(timg0.timer0);

    // UART1: GPIO4 = RX (← S8 TxD), GPIO5 = TX (→ S8 RxD), 9600 8N1
    let mut uart = Uart::new(
        peripherals.UART1,
        UartConfig::default().with_baudrate(9600),
    )
    .unwrap()
    .with_rx(peripherals.GPIO4)
    .with_tx(peripherals.GPIO5);

    // RMT → WS2812B RGB LED on GPIO8
    let rmt = Rmt::new(peripherals.RMT, Rate::from_mhz(32)).unwrap();
    let mut led = rmt
        .channel0
        .configure_tx(
            peripherals.GPIO8,
            TxChannelConfig::default().with_clk_divider(4),
        )
        .unwrap();

    // Boot indicator: white briefly
    led = led.transmit(&ws2812_frame(30, 30, 30)).unwrap().wait().unwrap();
    info!("CO2 sensor booted");

    loop {
        // Drain stale RX bytes
        let mut discard = [0u8; 1];
        while uart.read_buffered(&mut discard).unwrap_or(0) > 0 {}

        // Send Modbus read command
        let mut sent = 0;
        while sent < S8_READ_CO2.len() {
            sent += uart.write(&S8_READ_CO2[sent..]).unwrap_or(0);
        }
        uart.flush().ok();

        Timer::after(Duration::from_millis(500)).await;

        let mut buf = [0u8; 16];
        let n = uart.read_buffered(&mut buf).unwrap_or(0);

        if n >= 7 && buf[0] == 0xFE && buf[1] == 0x04 && buf[2] == 0x02 {
            let ppm = ((buf[3] as u16) << 8) | buf[4] as u16;
            info!("CO2: {} ppm", ppm);

            // CO2 quality → LED color
            // <= 1000 ppm : green  (good)
            // <= 2000 ppm : yellow (fair/poor)
            // <= 5000 ppm : red    (bad)
            //  > 5000 ppm : flashing red (dangerous)
            if ppm > 5000 {
                // Flash red 5× over the next 5 s (~500 ms on/off)
                for _ in 0..5 {
                    led = led.transmit(&ws2812_frame(180, 0, 0)).unwrap().wait().unwrap();
                    Timer::after(Duration::from_millis(400)).await;
                    led = led.transmit(&ws2812_frame(0, 0, 0)).unwrap().wait().unwrap();
                    Timer::after(Duration::from_millis(400)).await;
                }
            } else {
                let (r, g, b) = if ppm <= 1000 {
                    (0u8, 80u8, 0u8)    // green
                } else if ppm <= 2000 {
                    (120u8, 30u8, 0u8)  // orange
                } else {
                    (150u8, 0u8, 0u8)   // red
                };
                led = led.transmit(&ws2812_frame(r, g, b)).unwrap().wait().unwrap();
                Timer::after(Duration::from_secs(5)).await;
            }
        } else {
            if n > 0 {
                warn!("S8 unexpected response ({} bytes): {:02X?}", n, &buf[..n]);
            } else {
                warn!("S8 no response");
            }
            Timer::after(Duration::from_secs(5)).await;
        }
    }
}
