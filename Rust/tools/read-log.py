#!/usr/bin/env python3
"""Read the CO2 history log out of the device over USB and decode it to CSV.

The firmware logs one record per minute into the `co2_log` flash partition
(see src/logger.rs for the format). This script pulls the raw partition with
`espflash read-flash` and reconstructs timestamps.

Timestamps: the device has no clock, so records carry (boot_count, uptime).
The newest record is anchored to *now* (the moment you run this script,
assuming the device was running until you plugged it in); everything in the
same boot session gets an absolute timestamp from its uptime delta. Earlier
sessions can't be anchored (unknown power-off gaps) and get uptime-relative
times only.

Usage:
    python3 tools/read-log.py [--port /dev/cu.usbmodemXXX] [-o co2-log.csv]

The device may need manual bootloader mode: hold BOOT, tap RST, release BOOT.
After reading, tap RST to resume normal operation.
"""

import argparse
import csv
import datetime
import glob
import struct
import subprocess
import sys
import tempfile

PARTITION_OFFSET = 0x315000
PARTITION_SIZE = 0x60000
HEADER_SIZE = 0x1000
RECORD_SIZE = 8
MAGIC = b"C2LOG\x01\x00\x00"


def find_port() -> str:
    ports = glob.glob("/dev/cu.usbmodem*") + glob.glob("/dev/ttyACM*")
    if not ports:
        sys.exit("No USB serial port found — is the device plugged in?")
    return ports[0]


def read_partition(port: str) -> bytes:
    with tempfile.NamedTemporaryFile(suffix=".bin") as f:
        cmd = [
            "espflash", "read-flash",
            "--port", port,
            hex(PARTITION_OFFSET), str(PARTITION_SIZE), f.name,
        ]
        print("Running:", " ".join(cmd), file=sys.stderr)
        subprocess.run(cmd, check=True)
        f.seek(0)
        return f.read()


def decode(raw: bytes) -> list[tuple[int, int, int]]:
    """Return records as (boot_count, uptime_s, ppm), oldest first."""
    if raw[:8] != MAGIC:
        sys.exit("Log partition not initialized (magic mismatch) — no data.")

    records = []
    for off in range(HEADER_SIZE, PARTITION_SIZE, RECORD_SIZE):
        rec = raw[off:off + RECORD_SIZE]
        if rec == b"\xff" * RECORD_SIZE:
            continue  # empty slot
        boot, uptime, ppm = struct.unpack("<HIH", rec)
        records.append((off, boot, uptime, ppm))

    # The ring buffer means physical order != chronological order once it
    # has wrapped. Sort by (boot_count, uptime): boot counts only increase,
    # uptime increases within a boot. (boot_count wraps at 65535 — ignored;
    # that's ~180 years of daily reboots.)
    records.sort(key=lambda r: (r[1], r[2]))
    return [(b, u, p) for _, b, u, p in records]


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--port", help="serial port (default: autodetect)")
    ap.add_argument("-o", "--output", default="co2-log.csv", help="output CSV")
    args = ap.parse_args()

    raw = read_partition(args.port or find_port())
    records = decode(raw)
    if not records:
        sys.exit("Log is empty.")

    # Anchor the newest record to now and derive absolute timestamps for the
    # newest boot session only.
    now = datetime.datetime.now().astimezone()
    last_boot, last_uptime, _ = records[-1]

    with open(args.output, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["boot", "uptime_s", "co2_ppm", "estimated_time"])
        for boot, uptime, ppm in records:
            if boot == last_boot:
                ts = (now - datetime.timedelta(seconds=last_uptime - uptime))
                est = ts.isoformat(timespec="seconds")
            else:
                est = ""  # earlier session: no anchor across power-off gaps
            w.writerow([boot, uptime, ppm, est])

    sessions = len({b for b, _, _ in records})
    print(f"Wrote {len(records)} records ({sessions} boot session(s)) to {args.output}")


if __name__ == "__main__":
    main()
