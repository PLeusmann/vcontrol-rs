# vcontrol-mqtt: tests on the real controller

This is step 7 of [`mqtt-architecture.md`](mqtt-architecture.md): the first runs of `vcontrol-mqtt` against a real controller. Everything before it was tested with a fake controller, so these runs are the first contact with real hardware and real timing.

Do the parts in order. Stop at the first failed check and keep the log.

## Before you start

- [ ] The code is reviewed (pull request merged or approved).
- [ ] The Optolink reader is back on the heater.
- [ ] No other program uses the reader (`vcontrold`, `vcontrol cat`, ...).
- [ ] The binary, config and unit are installed on the Pi as described in the [user guide](vcontrol-mqtt.md#install).
- [ ] The config lists a few read-only commands, and in `writable` exactly one harmless command whose current value you know. A room or hot-water setpoint is a good choice.
- [ ] `systemctl is-active vcontrol-mqtt` says `inactive`. The first runs are by hand.

Open two more terminals for the whole session:

```sh
# All messages, with topic:
mosquitto_sub -h <broker> -u <user> -P <password> -v -t 'vcontrol/#' -t 'homeassistant/#'

# The log, when running as a service (part 6):
journalctl -u vcontrol-mqtt -f
```

## Part 1: read-only start and timing

```sh
time RUST_LOG=debug vcontrol-mqtt --config /etc/vcontrol/vcontrol-mqtt.toml --read-only 2>&1 | ts '%H:%M:%.S' | tee /tmp/run1.log
```

(`ts` is in the `moreutils` package. Without it, the log lines carry whole seconds only.)

| # | Check | Expected |
|---|---|---|
| 1.1 | Time from start to `Connected to '<device>'` | Well below 20 s. **Write the time down**: the connect timeout is 20 s and was never measured. |
| 1.2 | The device name in that line | Your controller model, e.g. `V200KO2B`. |
| 1.3 | The detected protocol (`Protocol detected: ...` in the debug log) | Write it down (VS1 or VS2). |
| 1.4 | `Connected to the MQTT broker.` appears | Yes. |
| 1.5 | No `timed out` and no `consecutive failures` in the first 10 minutes | Yes. |
| 1.6 | Time for a single read: the gap between two `VControl::get` lines (set `RUST_LOG=trace` for a minute) | Well below 5 s. **Write the longest one down**: the read timeout is 5 s and was never measured. On VS1 expect up to about 2 s. |
| 1.7 | The process keeps running | Yes. Exit code 1 means the link was considered lost; see the last log lines. |

If 1.1 or 1.6 come close to their limit, stop here. The constants are `CONNECT_TIMEOUT` and `TIMEOUT` in `src/bin/vcontrol-mqtt/controller.rs`.

## Part 2: published values

Keep the read-only run from part 1 going.

| # | Check | Expected |
|---|---|---|
| 2.1 | `vcontrol/status` | `online`, retained. |
| 2.2 | Each `vcontrol/<command>` | Appears once at start, then at its interval. |
| 2.3 | Compare each value with the controller's own display, or with `vcontrol --device /dev/ttyUSB0 get <command>` run while the bridge is stopped | Same value. Numbers have no unit; commands with named states show the name. |
| 2.4 | A sensor that is not connected | `None`. |
| 2.5 | A command with its own `interval` | Follows that interval, not `poll_interval`. |
| 2.6 | `homeassistant/<component>/<client_id>/<command>/config` for each command | Present, retained. `sensor` for read-only commands, `number` or `select` for the writable one. |

## Part 3: Home Assistant

| # | Check | Expected |
|---|---|---|
| 3.1 | Settings → Devices | One device named after the controller, with one entity for each command. |
| 3.2 | Entity values | Same as in part 2. |
| 3.3 | The writable command | A number field with sensible minimum, maximum and step, or a selection with the state names. |
| 3.4 | Restart Home Assistant | The entities come back without restarting the bridge. |
| 3.5 | Stop the bridge with Ctrl+C | All entities become unavailable; `vcontrol/status` is `offline`; exit code 0 (`echo $?`). |

## Part 4: writes in read-only mode

Start the read-only run again. Nothing in this part may change the controller.

```sh
mosquitto_pub -h <broker> -u <user> -P <password> -t vcontrol/<writable command>/set -m <value>
```

| # | Send | Expected in the log |
|---|---|---|
| 4.1 | A valid value, different from the current one | `Read-only mode: would write <command> = ...`. The controller still shows the old value. |
| 4.2 | A value above the maximum | `Rejected write ...: ... is outside ...` |
| 4.3 | `NaN` | `Rejected write ...: ... is not a finite number` |
| 4.4 | A value between two steps, e.g. `20.55` for a command with step 0.5 | `Rejected write ...: the controller cannot store ... exactly` |
| 4.5 | The same valid value with the retain flag (`-r`), then restart the bridge | While running, it is logged like 4.1 (the broker delivers it as a normal message). After the restart the broker delivers the retained copy, and it is ignored: `Rejected write ...: retained, or not a set topic`. Afterwards remove it: `mosquitto_pub ... -r -n`. |
| 4.6 | A value to the `/set` topic of a command that is not in `writable` | Nothing happens; the bridge is not subscribed to it. |
| 4.7 | Change the writable command in Home Assistant | Same as 4.1. |

## Part 5: the first live write

Only after parts 1 to 4 passed. Stop the read-only run.

Before: write down the current value of the writable command as the controller shows it.

```sh
RUST_LOG=info vcontrol-mqtt --config /etc/vcontrol/vcontrol-mqtt.toml 2>&1 | tee /tmp/run5.log
```

| # | Check | Expected |
|---|---|---|
| 5.1 | Send a valid value one step away from the current one | Log: `Writing <command> = ...`. |
| 5.2 | `vcontrol/<command>` right after it | The new value (this is read back from the controller). |
| 5.3 | The controller's own display | The new value. |
| 5.4 | Other settings on the controller, in particular those near the written one | Unchanged. |
| 5.5 | Send the original value | The controller shows the original value again. |
| 5.6 | Repeat 4.2 to 4.4 | Rejected as before; the controller is unchanged. |
| 5.7 | Change the value in Home Assistant | The controller follows; the entity shows the read-back value. |
| 5.8 | The bridge keeps running and reading | Yes, no `timed out`. |

If 5.3 or 5.4 fail, stop the bridge, restore the value on the controller by hand, and remove the command from `writable`.

## Part 6: as a service, and failures

```sh
sudo systemctl enable --now vcontrol-mqtt
```

| # | Do | Expected |
|---|---|---|
| 6.1 | `systemctl status vcontrol-mqtt` | `active (running)`; the unit can open the serial device (group `dialout`). |
| 6.2 | `sudo systemctl stop vcontrol-mqtt` | `vcontrol/status` is `offline` within about 2 s. Start it again. |
| 6.3 | Stop the broker for a minute, then start it | The log shows `MQTT: ...` warnings every 5 s, the service keeps running; after the broker is back: `online`, discovery and all values again. A write works afterwards. |
| 6.4 | Take the reader off the heater | Within 5 to 20 s the log shows `timed out, the Optolink link is lost`; the service exits with code 1; `vcontrol/status` is `offline`; systemd starts it again every 10 s. |
| 6.5 | Put the reader back | The next start connects and publishes again without help. |
| 6.6 | Put a typing error into a command name in the config and restart | The service fails with exit code 2 and is **not** restarted. Fix the config, start again. |
| 6.7 | Reboot the Pi | The service comes up by itself. |
| 6.8 | Let it run for 24 hours | No restarts (`systemctl show vcontrol-mqtt -p NRestarts`), no `timed out`, no `Dropping message` in the log. |

## What to record

Add the results to the pull request or to `mqtt-architecture.md`:

- controller model and detected protocol (1.2, 1.3)
- time to connect (1.1) and longest single read (1.6)
- the command used for the live write, and the outcome of 5.1 to 5.5
- every failed check, with the log lines around it
