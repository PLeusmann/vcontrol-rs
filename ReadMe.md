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

The `justfile` runs everything in the devcontainer, which it builds and starts itself; the host needs only `docker` and `just`.

| Recipe | Does |
|---|---|
| `just build` / `just test` / `just clippy` | `cargo build` / `test` / `clippy` with `--features mqtt`. The tests start `mosquitto`. |
| `just dist [target...]` | the Debian packages for a Raspberry Pi with a 32-bit OS, in `target/dist/<target>/`: `armv7-unknown-linux-musleabihf` (Pi 2 and later) and `arm-unknown-linux-musleabihf` (Pi 1, Zero) |
| `just publish-dev` | moves the `dev` tag to HEAD and pushes it; GitHub Actions publishes the packages as the `dev` pre-release |

### Install on the Pi

```sh
# The latest release (Pi with a 32-bit OS); or copy one from `just dist`.
curl -fsSLO https://github.com/PLeusmann/vcontrol-rs/releases/latest/download/vcontrol-rs_$(uname -m).deb
sudo apt install ./vcontrol-rs_$(uname -m).deb
sudoedit /etc/vcontrol/vcontrol-mqtt.toml   # at least [mqtt] host
echo 'VCONTROL_MQTT_PASSWORD=...' | sudo install -m 600 /dev/stdin /etc/vcontrol/vcontrol-mqtt.env
```

The package installs `vcontrol-mqtt` and `vcontrol` in `/usr/bin`, the configuration for a Vitotronic 200 KO2B as `/etc/vcontrol/vcontrol-mqtt.toml` (kept on upgrades), and the systemd unit, not enabled.

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
