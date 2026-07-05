# ESP32-H2 CO2 Sensor

A Zigbee End Device built on the ESP32-H2-DevKit-N4 with a Senseair S8 CO2 sensor, integrated into Home Assistant via Zigbee2MQTT.

## Hardware

| Component | Details |
|---|---|
| Microcontroller | ESP32-H2-DevKit-N4 |
| CO2 Sensor | Senseair S8 |
| Zigbee Coordinator | Sonoff Zigbee 3.0 USB Dongle Plus |
| Integration | Zigbee2MQTT + Home Assistant |

## Wiring

| S8 Pin | ESP32-H2 Pin | Notes |
|---|---|---|
| G+ | 5V | Power |
| G0 | GND | Ground |
| TX | GPIO4 | Data from sensor to ESP |
| RX | GPIO5 | Data from ESP to sensor |

> S8 runs on 5V but UART is 3.3V logic — connects directly, no level shifter needed.

## Software

The firmware (**v2.0**) is written in **Rust** on std/ESP-IDF with Espressif's
Zigbee SDK — see [`Rust/README.md`](Rust/README.md) for build, flash, and
monitoring instructions plus the hard-won ESP32-H2 lessons.

| Component | Version |
|---|---|
| Rust firmware | `Rust/` (esp-idf-svc 0.51, ESP-IDF v5.3.3, esp-zigbee-lib 1.6) |
| Zigbee2MQTT | via Home Assistant add-on |

> The original Arduino firmware (v1.3, `Co2-Sensor.ino` + `sketch.yaml`) was
> removed after the Rust port was verified end-to-end; it remains available
> in git history.

## Project Files

| File | Description |
|---|---|
| `Rust/` | Rust firmware (current) |
| `Co2-Sensor.js` | Zigbee2MQTT external converter (shared, unchanged since v1.3) |
| `CHANGELOG.md` | Version history |


## Adding a Custom Device Icon in Zigbee2MQTT
Here is a clean, formatted summary that you can copy and paste directly into your `README.md` file:

### Adding a Custom Device Icon in Zigbee2MQTT

To display a custom logo or image for your device in the Zigbee2MQTT frontend, follow these steps:

**1. Prepare the Image Directory**

* Navigate to your Zigbee2MQTT configuration folder (typically `config/zigbee2mqtt/`).
* Create a new folder named `device_icons` (if it does not already exist).
* Place your custom image file (e.g., `co2-sensor.png`) inside this folder. The recommended format is a square PNG.

**2. Update the Configuration**

* Open your `zigbee2mqtt/configuration.yaml` file.
* Locate your device under the `devices:` section using its IEEE address.
* Add the `icon` property. **Important:** The path must explicitly start with `device_icons/` followed by your filename.

**Example `configuration.yaml` entry:**

```yaml
devices:
  '0xYOUR_DEVICE_IEEE_ADDRESS':
    friendly_name: Your_Device_Name
    icon: device_icons/co2-sensor.png

```

**3. Apply Changes**

* Restart the Zigbee2MQTT add-on to apply the configuration. Your custom icon will now appear in the Z2M dashboard.