//! # Offline CO2 history logger
//!
//! Writes one record per minute into the dedicated `co2_log` flash partition
//! (384 KB), independent of Zigbee connectivity — the sensor keeps a full
//! history even with no network in range (e.g. in a car).
//!
//! ## Storage layout
//!
//! - Sector 0 (4 KB): header — magic bytes identifying an initialized log.
//!   If the magic is missing at boot (fresh chip, layout change), the whole
//!   partition is erased and re-initialized.
//! - Sectors 1..: a ring of fixed 8-byte records, written append-only:
//!
//!   | bytes | field       | notes                                   |
//!   |-------|-------------|-----------------------------------------|
//!   | 0..2  | boot_count  | LE u16, increments every boot (from NVS)|
//!   | 2..6  | uptime_s    | LE u32, seconds since this boot         |
//!   | 6..8  | ppm         | LE u16, CO2 reading                     |
//!
//! Erased flash reads 0xFF, and a real record can never be all-0xFF (ppm
//! caps at 32700), so "first all-0xFF record" marks the write position at
//! boot. Before writing into a new sector the ring erases it, which after
//! wrap-around discards the oldest ~8.5 hours (one 4 KB sector = 512
//! records) — fine for a ~34-day capacity.
//!
//! ## Timestamps
//!
//! There is no RTC and no network time offline, so records carry
//! (boot_count, uptime) instead of absolute time. The decoder script
//! (`tools/read-log.py`) anchors the newest record to "now" when reading
//! the log out over USB and reconstructs wall-clock times for the current
//! boot session; earlier sessions are listed with relative times.
//!
//! Flash wear: one erase per sector per ~8.5 h of logging ≈ 1000 cycles/yr
//! against a 100k-cycle rating — negligible.

use esp_idf_svc::sys as idf;
use log::{info, warn};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

/// Magic bytes in the header sector marking an initialized log partition.
/// Bump the trailing version byte if the record format ever changes.
const MAGIC: &[u8; 8] = b"C2LOG\x01\0\0";

const SECTOR: u32 = 4096;
const RECORD: u32 = 8;
const HEADER_SIZE: u32 = SECTOR; // records start after the header sector

/// Interval between log records. Fixed by design (not a Zigbee setting):
/// a constant cadence keeps offline timestamp reconstruction trivial.
const LOG_INTERVAL: Duration = Duration::from_secs(60);

/// Latest CO2 reading, written by the main loop after each successful S8
/// poll. 0 means "no reading yet" (the S8 never reports 0 ppm) — the logger
/// skips until the first real value arrives.
pub static LAST_PPM: AtomicU32 = AtomicU32::new(0);

struct LogPartition {
    part: *const idf::esp_partition_t,
    size: u32,
    /// Byte offset (within the partition) of the next record to write.
    write_pos: u32,
    boot_count: u16,
}

// The esp_partition_t pointer is a static ESP-IDF table entry; safe to move
// across threads.
unsafe impl Send for LogPartition {}

impl LogPartition {
    /// Locate the `co2_log` partition and find the current write position.
    fn open(boot_count: u16) -> anyhow::Result<Self> {
        let part = unsafe {
            idf::esp_partition_find_first(
                idf::esp_partition_type_t_ESP_PARTITION_TYPE_DATA,
                idf::esp_partition_subtype_t_ESP_PARTITION_SUBTYPE_DATA_UNDEFINED,
                b"co2_log\0".as_ptr(),
            )
        };
        anyhow::ensure!(!part.is_null(), "co2_log partition not found");
        let size = unsafe { (*part).size };

        let mut log = LogPartition { part, size, write_pos: HEADER_SIZE, boot_count };

        // Fresh or foreign partition content? Erase and stamp the header.
        let mut header = [0u8; 8];
        log.read(0, &mut header)?;
        if &header != MAGIC {
            info!("[LOG] Initializing log partition ({} KB)", size / 1024);
            unsafe { idf::esp!(idf::esp_partition_erase_range(part, 0, size as usize))? };
            log.write(0, MAGIC)?;
        }

        // Find the first empty (all-0xFF) record — that's where we resume.
        // Scans at most 384 KB in 4 KB chunks; takes a few ms at boot.
        log.write_pos = log.find_first_empty()?;
        info!(
            "[LOG] Ready — boot #{}, resuming at offset {:#x} ({} records stored)",
            boot_count,
            log.write_pos,
            (log.write_pos - HEADER_SIZE) / RECORD
        );
        Ok(log)
    }

    fn read(&self, offset: u32, buf: &mut [u8]) -> anyhow::Result<()> {
        unsafe {
            idf::esp!(idf::esp_partition_read(
                self.part,
                offset as usize,
                buf.as_mut_ptr() as *mut core::ffi::c_void,
                buf.len(),
            ))?;
        }
        Ok(())
    }

    fn write(&self, offset: u32, buf: &[u8]) -> anyhow::Result<()> {
        unsafe {
            idf::esp!(idf::esp_partition_write(
                self.part,
                offset as usize,
                buf.as_ptr() as *const core::ffi::c_void,
                buf.len(),
            ))?;
        }
        Ok(())
    }

    fn find_first_empty(&self) -> anyhow::Result<u32> {
        // Scan in small chunks: this runs on the logger thread's stack, so a
        // full-sector (4 KB) buffer would overflow it.
        const CHUNK: u32 = 256;
        let mut chunk = [0u8; 256];
        let mut offset = HEADER_SIZE;
        while offset < self.size {
            self.read(offset, &mut chunk)?;
            for rec in (0..CHUNK).step_by(RECORD as usize) {
                let r = &chunk[rec as usize..(rec + RECORD) as usize];
                if r.iter().all(|&b| b == 0xFF) {
                    return Ok(offset + rec);
                }
            }
            offset += CHUNK;
        }
        // Partition full: wrap to the first record sector (it gets erased
        // by the next append).
        Ok(HEADER_SIZE)
    }

    /// Append one record, erasing ahead and wrapping as needed.
    fn append(&mut self, uptime_s: u32, ppm: u16) -> anyhow::Result<()> {
        if self.write_pos >= self.size {
            self.write_pos = HEADER_SIZE; // wrap around
        }
        // Entering a new sector: erase it first (this is what drops the
        // oldest data once the ring has wrapped).
        if self.write_pos % SECTOR == 0 {
            unsafe {
                idf::esp!(idf::esp_partition_erase_range(
                    self.part,
                    self.write_pos as usize,
                    SECTOR as usize,
                ))?;
            }
        }

        let mut rec = [0u8; RECORD as usize];
        rec[0..2].copy_from_slice(&self.boot_count.to_le_bytes());
        rec[2..6].copy_from_slice(&uptime_s.to_le_bytes());
        rec[6..8].copy_from_slice(&ppm.to_le_bytes());
        self.write(self.write_pos, &rec)?;
        self.write_pos += RECORD;
        Ok(())
    }
}

/// Read and increment the boot counter in NVS (survives power loss, unlike
/// uptime). Distinguishes log sessions in the decoded history.
fn next_boot_count() -> u16 {
    use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs};
    let count = (|| -> anyhow::Result<u16> {
        let nvs_part = EspDefaultNvsPartition::take()?;
        let nvs = EspNvs::new(nvs_part, "co2log", true)?;
        let count = nvs.get_u16("boot_count")?.unwrap_or(0).wrapping_add(1);
        nvs.set_u16("boot_count", count)?;
        Ok(count)
    })();
    match count {
        Ok(c) => c,
        Err(e) => {
            warn!("[LOG] NVS boot counter unavailable ({e}), using 0");
            0
        }
    }
}

/// Logger thread: one record per minute, forever. Never touches Zigbee.
pub fn logger_task() {
    let boot_count = next_boot_count();
    let mut log = match LogPartition::open(boot_count) {
        Ok(l) => l,
        Err(e) => {
            // Logging is a bonus feature — never take the sensor down for it.
            warn!("[LOG] Disabled: {e}");
            return;
        }
    };

    let started = std::time::Instant::now();
    loop {
        std::thread::sleep(LOG_INTERVAL);
        let ppm = LAST_PPM.load(Ordering::SeqCst);
        if ppm == 0 {
            continue; // no successful S8 reading yet
        }
        let uptime_s = started.elapsed().as_secs() as u32;
        if let Err(e) = log.append(uptime_s, ppm as u16) {
            warn!("[LOG] Write failed: {e}");
        }
    }
}
