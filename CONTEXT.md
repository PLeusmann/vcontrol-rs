# vcontrol

## Glossary

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
