# vcontrol-mqtt user guide

`vcontrol-mqtt` connects a Viessmann heating controller to an MQTT broker. It reads the commands you choose at a fixed interval and publishes their values. It can also write a list of commands you explicitly allow. Home Assistant finds everything automatically through MQTT discovery.

This guide is for running the program. The design is described in [`mqtt-architecture.md`](mqtt-architecture.md).

## What you need

- A Viessmann controller with an Optolink adapter, reachable as a serial device (e.g. `/dev/ttyUSB0`) or through a TCP serial bridge.
- An MQTT broker, e.g. mosquitto.
- A Linux machine next to the heating, e.g. a Raspberry Pi.

Only one program can use the Optolink interface at a time. Stop `vcontrold` or other tools that use the same adapter before you start `vcontrol-mqtt`.

## Install

Build the Debian package in this repository. You need `docker` and `just`; the build runs in the devcontainer, which `just` creates:

```sh
just dist
```

This builds one package per Raspberry Pi target, for a 32-bit OS. Pick the one that matches what `uname -m` prints on the Pi:

| `uname -m` | Pi models | Package |
|---|---|---|
| `armv7l` | 2, 3, 4 | `target/dist/armv7-unknown-linux-musleabihf/vcontrol-rs_<version>-1_armhf.deb` |
| `armv6l` | 1, Zero | `target/dist/arm-unknown-linux-musleabihf/vcontrol-rs_<version>-1_armhf.deb` |

`just dist armv7-unknown-linux-musleabihf` builds only one of them. Copy the package to the Pi and install it:

```sh
scp target/dist/armv7-unknown-linux-musleabihf/vcontrol-rs_*.deb pi@raspberrypi:/tmp/
ssh pi@raspberrypi sudo dpkg -i /tmp/vcontrol-rs_*.deb
```

The package installs:

| Path | What |
|---|---|
| `/usr/bin/vcontrol-mqtt`, `/usr/bin/vcontrol` | the bridge and the command line tool, statically linked |
| `/etc/vcontrol/vcontrol-mqtt.toml` | the configuration, for a Vitotronic 200 KO2B; your edits survive upgrades |
| `/usr/lib/systemd/system/vcontrol-mqtt.service` | the service, installed but not enabled |
| `/usr/share/doc/vcontrol-rs/` | this guide and the [real device test](vcontrol-mqtt-real-device-test.md) |

After the first install, `dpkg` prints the remaining steps: configure, set the password, try it by hand, enable the service. They are described below. An upgrade restarts the service only if it is running.

## Find the command names

Command names depend on the controller model. List all commands of your controller, with their current values, using the `vcontrol` command line tool from this repository:

```sh
vcontrol --device /dev/ttyUSB0 cat
```

Use the names exactly as printed, including upper and lower case.

## Configure

Edit `/etc/vcontrol/vcontrol-mqtt.toml`. The installed file ([`vitotronic-200-ko2b.toml`](../dist/config/vitotronic-200-ko2b.toml)) lists all options, the optional ones commented out; set at least `[mqtt] host`. Its commands are for a Vitotronic 200 KO2B; for another controller, start from [`vcontrol-mqtt.example.toml`](../dist/config/vcontrol-mqtt.example.toml). A minimal configuration:

```toml
poll_interval = 60
writable = ["Bedien_WW_Solltemperatur"]

[optolink]
device = "/dev/ttyUSB0"

[mqtt]
host = "broker.local"

[[command]]
name = "ADC_Isttemperaturwert_KTS"

[[command]]
name = "Bedien_WW_Solltemperatur"
```

`poll_interval` and `writable` must come before the first `[section]`.

### Top level

| Key | Required | Meaning |
|---|---|---|
| `poll_interval` | yes | How often each command is read, in whole seconds. |
| `writable` | no | Names of the commands that may be written from MQTT. Each must also have a `[[command]]` entry. |

### `[optolink]`

Use either `device`, or `port` with an optional `host`.

| Key | Meaning |
|---|---|
| `device` | Path of the serial device. |
| `host` | Host name of a TCP serial bridge. Default: `localhost`. |
| `port` | Port of the TCP serial bridge. |

### `[mqtt]`

| Key | Default | Meaning |
|---|---|---|
| `host` | (required) | Host name of the broker. |
| `port` | `1883` | Port of the broker. Usually `8883` with TLS. |
| `client_id` | `vcontrol` | MQTT client id. It is also part of the Home Assistant ids, so give each controller its own. Only letters, digits, `_` and `-`. |
| `username` | none | User name for the broker. |
| `password` | none | Password for the broker. The environment variable `VCONTROL_MQTT_PASSWORD` overrides it. |
| `tls` | `false` | Encrypt the connection. |
| `ca_file` | none | Certificate of the authority that signed the broker's certificate, in PEM format. Required when `tls = true`. |
| `topic_prefix` | `vcontrol` | First part of all topics. Must differ from `discovery_prefix`. |

To keep the password out of the config file, put it into `/etc/vcontrol/vcontrol-mqtt.env`, which the systemd unit reads:

```sh
echo 'VCONTROL_MQTT_PASSWORD=...' | sudo install -m 600 /dev/stdin /etc/vcontrol/vcontrol-mqtt.env
```

### `[homeassistant]`

| Key | Default | Meaning |
|---|---|---|
| `discovery_prefix` | `homeassistant` | Discovery prefix configured in Home Assistant's MQTT integration. |

### `[[command]]`

One entry for each command to read.

| Key | Required | Meaning |
|---|---|---|
| `name` | yes | Name of the command. |
| `interval` | no | Seconds between reads of this command, instead of `poll_interval`. Use a long interval for values that rarely change. |

Only commands with simple values can be used: numbers, texts and values with named states. Schedules, dates, error lists and byte arrays are not supported.

## Run

Start it by hand first, in read-only mode. Values are published, but writes received from MQTT are only checked and logged:

```sh
vcontrol-mqtt --config /etc/vcontrol/vcontrol-mqtt.toml --read-only
```

| Option | Meaning |
|---|---|
| `--config <path>` | Path of the configuration file. Required. |
| `--read-only` | Never write to the controller. A valid write is logged as "would write". |

For the very first runs on a controller, follow the checklist in [`vcontrol-mqtt-real-device-test.md`](vcontrol-mqtt-real-device-test.md).

When the values look right, run it as a service:

```sh
sudo systemctl enable --now vcontrol-mqtt
journalctl -u vcontrol-mqtt -f
```

The log level is `info` by default. Set the environment variable `RUST_LOG` (e.g. `RUST_LOG=debug`) for more detail.

## MQTT topics

With the default `topic_prefix`:

| Topic | Direction | Payload |
|---|---|---|
| `vcontrol/<command>` | published | The current value. Retained. |
| `vcontrol/<command>/set` | received | A new value. Only for commands listed in `writable`. |
| `vcontrol/status` | published | `online` while the program runs, `offline` otherwise. Retained. |

Values:

- Numbers are published without their unit, e.g. `21.5`.
- Commands with named states publish the name, e.g. `Cylinder priority`. If several numbers share one name, the number is appended: `Modulating priority (4)`.
- `None` means the controller has no valid value at the moment.

Try it with the mosquitto tools:

```sh
mosquitto_sub -h broker.local -v -t 'vcontrol/#'
mosquitto_pub -h broker.local -t vcontrol/Bedien_WW_Solltemperatur/set -m 55
```

## Writing values

A command accepts writes only if it is listed in `writable`. Each write is checked before it is sent to the controller:

- A number must be within the command's limits and must be a value the controller can store. A command that counts in steps of 5 rejects `9`.
- A command with named states accepts one of its names, or the number behind a name.
- Retained messages are ignored. Publish to `/set` topics without the retain flag.

A rejected write is logged with the reason and changes nothing. After a successful write, the program reads the value back and publishes what the controller now has.

Not every command can be made writable. The program refuses to start if a command in `writable`:

- is read-only on the controller,
- is a number without known limits,
- is a text,
- is a single flag that shares its byte with other flags on the controller,
- has a value that cannot be converted back for writing.

The error message names the command and the reason.

## Home Assistant

With the MQTT integration set up in Home Assistant, the controller appears as one device with one entity for each command:

| Command | Entity |
|---|---|
| not in `writable` | sensor |
| in `writable`, a number | number, with minimum, maximum and step |
| in `writable`, with named states | select |

Entities become unavailable while the program is not running. If you change the list of commands, old entities stay in Home Assistant until you delete them there.

## Security

Anyone who can publish to `vcontrol/+/set` can change the heating settings you listed in `writable`. The program checks every value, but it cannot know who sent it. Protect the broker:

- Require a user name and password, and use TLS if the broker is reachable from other machines.
- Restrict who may publish to the set topics. For mosquitto, in an ACL file:

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

- Keep `writable` as short as possible.

## Troubleshooting

The program ends with an exit code that tells what happened:

| Exit code | Meaning | What systemd does |
|---|---|---|
| 0 | Stopped by a signal. | nothing |
| 1 | The connection to the controller could not be opened, or was lost. | restarts after 10 seconds |
| 2 | The configuration is wrong. | nothing; fix the file and start again |

| Symptom | Cause and fix |
|---|---|
| `command "..." is unknown on device ...` | The name does not exist on your controller model. Check it with `vcontrol cat`. |
| `command "..." cannot be writable: ...` | See [Writing values](#writing-values). Remove it from `writable`. |
| `Optolink: ... timed out` or `Permission denied` at start | Wrong device path, another program is using the adapter, or the user may not open the serial device. The systemd unit runs with the `dialout` group. |
| The service restarts again and again | The controller does not answer. Check the position of the Optolink adapter on the controller. |
| `MQTT: ...` warnings in the log | The broker is unreachable or refuses the login. The program keeps trying every 5 seconds and keeps reading the controller. |
| `Rejected write on ...` | The value was outside the limits, not storable, or not a known name. The log line gives the reason. |
| A value shows `None` or "unknown" | The controller reports no valid value, e.g. for a sensor that is not connected. |
| A write from Home Assistant has no effect | Check the log. In `--read-only` mode writes are only logged. |
