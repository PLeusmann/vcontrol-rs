# 🔥 vcontrol-rs

[![Crates.io](https://img.shields.io/crates/v/vcontrol.svg)](https://crates.io/crates/vcontrol)
[![Documentation](https://docs.rs/vcontrol/badge.svg)](https://docs.rs/vcontrol)

This is a Rust library for communication with Viessmann heating controllers.

The included `Optolink` struct is a low-level abstraction for an Optolink connection over either a TCP socket or a serial port.

The `VControl` struct is a high-level abstraction for a complete system, which can be configured with a YAML file, much like what [`vcontrold`](https://github.com/openv/vcontrold) does with an XML file.

## `vcontrol-mqtt`

`vcontrol-mqtt` is a bridge between a controller and an MQTT broker, with Home Assistant discovery. It polls the configured commands, publishes their values and applies writes for an explicit allowlist of commands. The full user guide, with all configuration options, is [`docs/vcontrol-mqtt.md`](docs/vcontrol-mqtt.md); the design is described in [`docs/mqtt-architecture.md`](docs/mqtt-architecture.md).

| Topic | Payload |
|---|---|
| `<topic_prefix>/<command>` | the value without unit, or the label for mapped commands; `None` if there is no value |
| `<topic_prefix>/<command>/set` | a new value or label, only for commands listed in `writable` |
| `<topic_prefix>/status` | `online` / `offline` |

### Build

Inside the devcontainer, for a Raspberry Pi with a 32-bit OS:

```sh
cargo build --release --features mqtt --bin vcontrol-mqtt --target armv7-unknown-linux-musleabihf
```

The binary is statically linked. The tests need the devcontainer too (they start `mosquitto`): `cargo test --features mqtt`.

### Install on the Pi

```sh
scp target/armv7-unknown-linux-musleabihf/release/vcontrol-mqtt pi@raspberrypi:/tmp/
scp vcontrol-mqtt.example.toml vcontrol-mqtt.service pi@raspberrypi:/tmp/

# On the Pi:
sudo install /tmp/vcontrol-mqtt /usr/local/bin/
sudo install -D -m 644 /tmp/vcontrol-mqtt.example.toml /etc/vcontrol/vcontrol-mqtt.toml   # then edit it
echo 'VCONTROL_MQTT_PASSWORD=...' | sudo install -m 600 /dev/stdin /etc/vcontrol/vcontrol-mqtt.env
sudo install -m 644 /tmp/vcontrol-mqtt.service /etc/systemd/system/
```

### Run

Run it by hand first, in read-only mode. Writes received from MQTT are then validated and logged, but never sent to the controller:

```sh
vcontrol-mqtt --config /etc/vcontrol/vcontrol-mqtt.toml --read-only
```

`RUST_LOG` sets the log level (default `info`). Only one process can use the Optolink interface at a time, so stop other users of the serial device first. Then enable the service:

```sh
sudo systemctl enable --now vcontrol-mqtt
```

The bridge refuses to start if the configuration does not fit the detected controller, e.g. an unknown command, or a writable command without bounds. It exits when the Optolink connection is lost; systemd restarts it.

### Security

Anyone who can publish to `<topic_prefix>/+/set` can change the listed heating settings. The bridge validates every write, but it cannot know who sent it. Restrict publishing to these topics with ACLs on the broker, e.g. for mosquitto:

```
user vcontrol
topic readwrite vcontrol/#
topic write homeassistant/#
topic read homeassistant/status

user homeassistant
topic read vcontrol/#
topic write vcontrol/+/set
topic readwrite homeassistant/#
```
