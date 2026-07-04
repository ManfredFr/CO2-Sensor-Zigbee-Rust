#include "ZigbeeCore.h"
#include "ep/ZigbeeTempSensor.h"
#include "ep/ZigbeeAnalog.h"

#define VERSION "v1.3"

// — S8 UART pins —
#define S8_RX_PIN  4   // GPIO4 — connects to S8 TX
#define S8_TX_PIN  5   // GPIO5 — connects to S8 RX
#define S8_BAUD    9600

// — Onboard RGB LED (WS2812, RGB byte order) —
#define LED_PIN             8
#define LED_BRIGHTNESS_MIN  0
#define LED_BRIGHTNESS_MAX  100
#define LED_BRIGHTNESS_DEFAULT 50

// — Zigbee endpoints —
#define CO2_ENDPOINT        1   // CO2 measurement — uses ZigbeeTempSensor as carrier
#define INTERVAL_ENDPOINT   2   // report interval setting (read/write)
#define BRIGHTNESS_ENDPOINT 3   // LED brightness 0–100 % (read/write)

// — Report interval —
#define INTERVAL_MIN_S     10
#define INTERVAL_MAX_S     300
#define INTERVAL_DEFAULT_S 30

// Senseair S8 Modbus RTU request — reads CO2 ppm register
static const uint8_t CO2_REQUEST[] = {0xFE, 0x04, 0x00, 0x03, 0x00, 0x01, 0xD5, 0xC5};

HardwareSerial S8Serial(1);

// Endpoint 1: ZigbeeTempSensor used as carrier for CO2 value.
// CO2 ppm is stored as temperature (ppm / 100.0), so the ZCL INT16
// measuredValue equals ppm directly. The converter reads it back as ppm.
ZigbeeTempSensor zbCO2(CO2_ENDPOINT);
// Endpoint 2: report interval in seconds (read/write, analog output)
ZigbeeAnalog zbInterval(INTERVAL_ENDPOINT);
// Endpoint 3: LED brightness 0–100 % (read/write, analog output)
ZigbeeAnalog zbBrightness(BRIGHTNESS_ENDPOINT);

volatile int reportIntervalMs  = INTERVAL_DEFAULT_S * 1000;
volatile int ledBrightness     = LED_BRIGHTNESS_DEFAULT;
int readCount = 0;
int failCount = 0;

// Called when HA writes a new interval value via Zigbee.
// Keep this minimal — no Zigbee calls allowed inside a stack callback.
void onIntervalWrite(float value) {
  int seconds = (int)value;
  if (seconds < INTERVAL_MIN_S) seconds = INTERVAL_MIN_S;
  if (seconds > INTERVAL_MAX_S) seconds = INTERVAL_MAX_S;
  reportIntervalMs = seconds * 1000;
  Serial.printf("[INTERVAL] Updated to %d seconds\n", seconds);
}

// Called when HA writes a new brightness value via Zigbee.
void onBrightnessWrite(float value) {
  int pct = (int)value;
  if (pct < LED_BRIGHTNESS_MIN) pct = LED_BRIGHTNESS_MIN;
  if (pct > LED_BRIGHTNESS_MAX) pct = LED_BRIGHTNESS_MAX;
  ledBrightness = pct;
  Serial.printf("[LED] Brightness set to %d%%\n", pct);
}

// Set the LED color, scaled by ledBrightness %.
// neopixelWrite is built into the ESP32 Arduino core — no external library needed.
void setLed(uint8_t r, uint8_t g, uint8_t b) {
  int br = ledBrightness;
  // neopixelWrite sends GRB on wire; this LED uses RGB byte order, so swap r↔g
  neopixelWrite(LED_PIN, (g * br) / 100, (r * br) / 100, (b * br) / 100);
}

// Map CO2 ppm to LED color and apply it.
void updateLed(int ppm) {
  if      (ppm <= 1000) setLed(0,   120,  0);   // green  — good
  else if (ppm <= 2000) setLed(200,  50,  0);   // orange — fair/poor
  else                  setLed(220,   0,  0);   // red    — bad/dangerous
}

// Read CO2 ppm from S8 via Modbus RTU
// Returns ppm on success, -1 on timeout or bad response
int readCO2() {
  while (S8Serial.available()) S8Serial.read();  // flush stale bytes

  Serial.println("[S8] Sending CO2 request...");
  S8Serial.write(CO2_REQUEST, sizeof(CO2_REQUEST));

  unsigned long start = millis();
  while (S8Serial.available() < 7) {
    if (millis() - start > 1000) {
      Serial.println("[S8] Timeout — no response within 1000ms");
      return -1;
    }
    delay(10);
  }

  uint8_t resp[7];
  S8Serial.readBytes(resp, 7);

  Serial.printf("[S8] Response: %02X %02X %02X %02X %02X %02X %02X\n",
    resp[0], resp[1], resp[2], resp[3], resp[4], resp[5], resp[6]);

  if (resp[0] != 0xFE || resp[1] != 0x04 || resp[2] != 0x02) {
    Serial.println("[S8] Bad response header");
    return -1;
  }

  return (resp[3] << 8) | resp[4];
}

void setup() {
  Serial.begin(115200);
  delay(2000);

  Serial.println("========================================");
  Serial.printf("  Co2-Sensor %s\n", VERSION);
  Serial.println("========================================");
  Serial.printf("[INIT] S8 UART: RX=GPIO%d TX=GPIO%d @ %d baud\n", S8_RX_PIN, S8_TX_PIN, S8_BAUD);
  Serial.printf("[INIT] Report interval: %d-%ds, default %ds\n", INTERVAL_MIN_S, INTERVAL_MAX_S, INTERVAL_DEFAULT_S);
  Serial.printf("[INIT] LED pin: GPIO%d, default brightness: %d%%\n", LED_PIN, LED_BRIGHTNESS_DEFAULT);

  // Brief white boot flash so the LED is visibly alive at startup
  neopixelWrite(LED_PIN, 30, 30, 30);
  delay(500);
  neopixelWrite(LED_PIN, 0, 0, 0);

  S8Serial.begin(S8_BAUD, SERIAL_8N1, S8_RX_PIN, S8_TX_PIN);
  Serial.println("[INIT] S8 serial started");

  // CO2 endpoint — ZigbeeTempSensor used as carrier; CO2 ppm = measuredValue in ZCL
  zbCO2.setManufacturerAndModel("Espressif", "Co2Sensor");
  zbCO2.setVersion(13);  // v1.3
  zbCO2.setMinMaxValue(0, 327);   // 0–32700 ppm range (stored as /100)
  zbCO2.setTolerance(0);
  Zigbee.addEndpoint(&zbCO2);
  Serial.println("[ZIGBEE] CO2 endpoint registered (ep 1, temp carrier)");

  // Interval endpoint — analog output, read/write
  zbInterval.addAnalogOutput();
  zbInterval.setAnalogOutputDescription("Report interval");
  zbInterval.setAnalogOutputMinMax(INTERVAL_MIN_S, INTERVAL_MAX_S);
  zbInterval.onAnalogOutputChange(onIntervalWrite);
  Zigbee.addEndpoint(&zbInterval);
  Serial.println("[ZIGBEE] Interval endpoint registered (ep 2)");

  // Brightness endpoint — analog output, read/write
  zbBrightness.addAnalogOutput();
  zbBrightness.setAnalogOutputDescription("LED brightness");
  zbBrightness.setAnalogOutputMinMax(LED_BRIGHTNESS_MIN, LED_BRIGHTNESS_MAX);
  zbBrightness.onAnalogOutputChange(onBrightnessWrite);
  Zigbee.addEndpoint(&zbBrightness);
  Serial.println("[ZIGBEE] Brightness endpoint registered (ep 3)");

  Serial.println("[ZIGBEE] Starting stack...");
  if (!Zigbee.begin(ZIGBEE_END_DEVICE)) {
    Serial.println("[ZIGBEE] ERROR: Failed to start stack!");
    while (1) delay(100);
  }

  Serial.println("[ZIGBEE] Waiting to join network...");
  while (!Zigbee.connected()) {
    delay(100);
  }
  Serial.println("[ZIGBEE] Joined network!");

  zbCO2.setReporting(1, INTERVAL_DEFAULT_S, 0);
  Serial.println("[ZIGBEE] Temperature reporting configured");

  // Push initial values so HA reads the correct defaults immediately after join
  zbInterval.setAnalogOutput(reportIntervalMs / 1000.0f);
  zbInterval.reportAnalogOutput();
  zbBrightness.setAnalogOutput(ledBrightness);
  zbBrightness.reportAnalogOutput();
  Serial.println("[ZIGBEE] Initial interval and brightness reported");

  Serial.println("========================================");
  Serial.println("  Ready");
  Serial.println("========================================");
}

void loop() {
  readCount++;
  Serial.printf("[LOOP] Read #%d (interval: %dms)\n", readCount, reportIntervalMs);

  int ppm = readCO2();

  if (ppm <= 0) {
    failCount++;
    Serial.printf("[CO2] Read failed (fail #%d) — skipping report\n", failCount);
    delay(reportIntervalMs);
    return;
  }

  Serial.printf("[CO2] %d ppm — reporting via Zigbee\n", ppm);

  // Store ppm as temperature (ppm / 100.0) so ZCL measuredValue = ppm
  zbCO2.setTemperature(ppm / 100.0);
  zbCO2.reportTemperature();
  Serial.printf("[ZIGBEE] Reported %d ppm (stored as %.2f°C)\n", ppm, ppm / 100.0);

  Serial.printf("[LOOP] Sleeping %ds until next read\n\n", reportIntervalMs / 1000);

  // For >5000 ppm (dangerous), flash red during the interval instead of solid color.
  if (ppm > 5000) {
    unsigned long start = millis();
    bool ledOn = true;
    while (millis() - start < (unsigned long)reportIntervalMs) {
      ledOn ? setLed(220, 0, 0) : setLed(0, 0, 0);
      ledOn = !ledOn;
      delay(500);
    }
  } else {
    updateLed(ppm);
    delay(reportIntervalMs);
  }
}
