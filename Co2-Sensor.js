const definition = {
    zigbeeModel: ['Co2Sensor'],
    model: 'Co2Sensor',
    vendor: 'Espressif',
    description: 'ESP32-H2 CO2 Sensor (Senseair S8)',
    image: 'https://compassionate.nz/assets/CO2-Logo.png',
    fromZigbee: [
        {
            // Temperature cluster used as CO2 carrier.
            // ZCL measuredValue = temp * 100 (INT16), and we store ppm/100 as temp,
            // so measuredValue == ppm directly.
            cluster: 'msTemperatureMeasurement',
            type: ['attributeReport', 'readResponse'],
            convert: (model, msg, publish, options, meta) => {
                if (msg.endpoint.ID === 1) {
                    return { co2: msg.data.measuredValue };
                }
            },
        },
        {
            // Analog Output cluster (endpoint 2) — report interval in seconds
            cluster: 'genAnalogOutput',
            type: ['attributeReport', 'readResponse'],
            convert: (model, msg, publish, options, meta) => {
                if (msg.endpoint.ID === 2) {
                    return { report_interval: Math.round(msg.data.presentValue) };
                }
            },
        },
        {
            // Analog Output cluster (endpoint 3) — LED brightness 0–100 %
            cluster: 'genAnalogOutput',
            type: ['attributeReport', 'readResponse'],
            convert: (model, msg, publish, options, meta) => {
                if (msg.endpoint.ID === 3) {
                    return { led_brightness: Math.round(msg.data.presentValue) };
                }
            },
        },
        {
            // Basic cluster — appVersion encodes firmware version as MAJOR*10+MINOR (e.g. 13 = v1.3)
            cluster: 'genBasic',
            type: ['attributeReport', 'readResponse'],
            convert: (model, msg, publish, options, meta) => {
                if (msg.data.appVersion !== undefined) {
                    const v = msg.data.appVersion;
                    return { firmware_version: `v${Math.floor(v / 10)}.${v % 10}` };
                }
            },
        },
    ],
    toZigbee: [
        {
            key: ['report_interval'],
            convertSet: async (entity, key, value, meta) => {
                const endpoint = meta.device.getEndpoint(2);
                await endpoint.write('genAnalogOutput', { presentValue: value });
                return { state: { report_interval: value } };
            },
            convertGet: async (entity, key, meta) => {
                const endpoint = meta.device.getEndpoint(2);
                await endpoint.read('genAnalogOutput', ['presentValue']);
            },
        },
        {
            key: ['firmware_version'],
            convertGet: async (entity, key, meta) => {
                const endpoint = meta.device.getEndpoint(1);
                await endpoint.read('genBasic', ['appVersion']);
            },
        },
        {
            key: ['led_brightness'],
            convertSet: async (entity, key, value, meta) => {
                const endpoint = meta.device.getEndpoint(3);
                await endpoint.write('genAnalogOutput', { presentValue: value });
                return { state: { led_brightness: value } };
            },
            convertGet: async (entity, key, meta) => {
                const endpoint = meta.device.getEndpoint(3);
                await endpoint.read('genAnalogOutput', ['presentValue']);
            },
        },
    ],
    exposes: [
        {
            type: 'numeric',
            name: 'co2',
            property: 'co2',
            access: 1,
            unit: 'ppm',
            description: 'CO2 concentration',
            value_min: 0,
            value_max: 10000,
        },
        {
            type: 'numeric',
            name: 'report_interval',
            property: 'report_interval',
            access: 7,
            unit: 's',
            description: 'How often the sensor reports CO2 (seconds)',
            value_min: 10,
            value_max: 300,
            value_step: 10,
        },
        {
            type: 'text',
            name: 'firmware_version',
            property: 'firmware_version',
            access: 1,
            description: 'Firmware version',
        },
        {
            type: 'numeric',
            name: 'led_brightness',
            property: 'led_brightness',
            access: 7,
            unit: '%',
            description: 'LED brightness (0 = off, 100 = full)',
            value_min: 0,
            value_max: 100,
            value_step: 5,
        },
    ],
    meta: { multiEndpoint: true },
    configure: async (device, coordinatorEndpoint, logger) => {
        const endpoint = device.getEndpoint(1);
        // Bind the temperature cluster to the coordinator and configure reporting.
        // This tells the device where to send its reports.
        await endpoint.bind('msTemperatureMeasurement', coordinatorEndpoint);
        await endpoint.configureReporting('msTemperatureMeasurement', [{
            attribute: 'measuredValue',
            minimumReportInterval: 10,
            maximumReportInterval: 300,
            reportableChange: 0,
        }]);
    },
};

module.exports = definition;
