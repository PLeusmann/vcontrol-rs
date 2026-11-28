# vcontrol

## Glossary

- **Controller** — the Viessmann heating control unit the Optolink connects to (here a Vitotronic 200 KO2B). Its firmware decides what can be read and written. Avoid "Regelung", "boiler".
- **Command** — a named value on a Viessmann heating controller (e.g. outside temperature, operating mode), read and optionally written over the Optolink link. Users sometimes say "channel"; the canonical term is Command.
- **Optolink** — the optical diagnostic interface of the controller, reached via a serial device or a TCP serial bridge. Only one process may use it at a time.
- **Mapping** — the list that names a Command's numeric values (e.g. `2` = "Heizen und Warmwasser"). A named value is called a **Label**.
- **Bridge** — the `vcontrol-mqtt` program: owns the Optolink link, publishes Command values to MQTT and applies writes received from MQTT.
- **Polled Command** — a Command the Bridge reads periodically and publishes.
- **Writable Command** — a Polled Command explicitly listed as writable in the Bridge configuration. Only these accept writes from MQTT, and only if the controller allows writing them.
- **State topic** — the MQTT topic carrying a Command's current value.
- **Set topic** — the MQTT topic on which a Writable Command receives new values.
- **Discovery** — Home Assistant MQTT discovery: config messages that make Commands appear as Home Assistant entities automatically.
- **Read-only mode** — a Bridge run in which received writes are validated and logged but never sent to the controller.
- **Availability** — whether the Bridge is online, published on a status topic and set to offline by the broker when the Bridge disconnects.

### Controller terms

These follow the controller's service manual (Vitotronic 200 KO1B/KO2B, 5727177) and stay in German.

- **Zeitprogramm** — the weekly schedule of one function: a Heizkreis (normal vs. reduced room temperature), the Trinkwassererwärmung, or the Trinkwasserzirkulationspumpe. Each day has up to 4 Zeitphasen. Avoid "Schaltzeiten", "time program", "circuit times".
- **Zeitphase** — one period within a day of a Zeitprogramm during which the function is in normal operation. Avoid "Schaltzeit", "switching period", "time span".
- **Ferienprogramm** — the period between a start and an end date during which a Heizkreis runs in holiday operation. The controller stores the start and the end as two separate Commands. Avoid "Urlaub", "holiday program".
- **Ferienbetrieb** — the state of a Heizkreis while its Ferienprogramm is running. It is what the controller reports, not what is set. Avoid using "Ferienprogramm" for it.
- **Störung** — a fault the controller has detected, identified by a **Störungscode**. Avoid "error", "fault".
- **Störungsmeldungen** — the Störungen currently pending at the controller.
- **Störungsspeicher** — the last 10 Störungen, including resolved ones, newest first. The manual also calls it "Fehlerhistorie". Avoid "error history".
