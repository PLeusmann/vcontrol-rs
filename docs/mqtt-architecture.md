# vcontrol-mqtt architecture

`vcontrol-mqtt` is a Bridge between a Viessmann controller (via Optolink) and an MQTT broker, with Home Assistant Discovery. Terms are defined in [`CONTEXT.md`](../CONTEXT.md).

## Scope

In scope:

- Polling configured Commands of simple types (numbers, strings, mapped values) and publishing their values.
- Writing an explicit allowlist of Writable Commands from MQTT.
- Home Assistant Discovery and Availability.
- Broker username/password and TLS.
- Local build in the devcontainer for a Raspberry Pi (armv7, 32-bit OS).

Out of scope (for now):

- Complex types (schedules, dates, errors, byte arrays). They cannot be polled or written.
- Client certificates (mTLS).
- Reconnecting to Optolink inside the process; the process exits and systemd restarts it.
- CI/release changes; this lives in the fork.

## 1. Boundaries

- The library (`src/`) knows nothing about MQTT. Its only change is that `Command::set` returns `Error::InvalidArgument` instead of panicking on a failed `convert_back`.
- The Bridge uses only the library's public API: `VControl`, `Device`, `Command`, `Value`, `OutputValue`.
- Everything MQTT-related sits behind the `mqtt` feature, which includes `cli`:

  ```toml
  [features]
  mqtt = ["cli", "dep:rumqttc", "dep:toml", "tokio/signal"]

  [[bin]]
  name = "vcontrol-mqtt"
  required-features = ["mqtt"]
  ```

- Pin `rumqttc` and build it with rustls using the `ring` backend, never `aws-lc-rs`, which is hard to cross-compile for armv7 musl. `cargo tree -i aws-lc-sys` must come back empty.

## 2. Modules

Modules live in `src/bin/vcontrol-mqtt/`, mirroring `src/bin/vcontrol/`.

| Module | Job | Does I/O |
|---|---|---|
| `config.rs` | Parse the TOML, then check it against the detected Device to produce a checked config | no |
| `payload.rs` | Convert values to and from payloads, including Label → number and write validation | no |
| `topics.rs` | Build topics and discovery ids; turn an incoming topic into a Writable Command | no |
| `discovery.rs` | Build the Home Assistant Discovery JSON for each Command | no |
| `controller.rs` | `Controller` trait (`device`, `get`, `set`), implemented for `VControl` with timeouts | yes |
| `bridge.rs` | The single main loop: scheduling, polling, writes, publishing | yes |
| `main.rs` | Startup order, signals, exit codes | yes |

Rule: every decision lives in the modules without I/O and is unit tested. `bridge.rs` only connects them.

## 3. Concurrency: one loop, one step per pass

A single task owns both the Optolink link and the `rumqttc` event loop:

```
loop {
  select! {
    biased;
    _ = shutdown_signal          => shut down (see §8)
    event = mqtt.poll()          => handle it (a write is done immediately)
    _ = sleep_until(next_due)    => read ONE due Command, publish it
  }
}
```

- **One read per pass, never a batch.** The MQTT event loop must be polled well within the keep-alive (60 s by default). A single read takes at most about 0.5 s; a whole round can take far longer. The code carries a comment explaining this rule.
- **Writes first.** `biased;` makes the loop check MQTT before reads, so a pending `/set` is handled before the next scheduled read.
- **Every Optolink call has a timeout.** `connect`, `get` and `set` are each wrapped in `tokio::time::timeout` (5 s). A timeout means the link is lost (see §8).
- **Publishing never waits.** `AsyncClient` feeds a bounded channel that only `mqtt.poll()` drains, so awaiting `publish()` inside this loop can deadlock. Use `try_publish` everywhere. The channel capacity is at least 2 × (number of Commands) + 10. When the channel is full, log a warning and drop the message.
- No locks or channels of our own are needed: only one task touches the Optolink link, so reads and writes cannot overlap.
- **Risk:** the `mqtt.poll()` future is dropped whenever a read becomes due. The broker test (step 5) must confirm that `rumqttc` loses nothing when this happens. If it does, fall back to a separate MQTT task feeding an `mpsc` channel.

## 4. Scheduling

- A global `poll_interval`, with an optional per-Command `interval` override. Both are whole seconds.
- Each Command has a next-due instant; the loop sleeps until the earliest one.
- A read is rescheduled at its interval whether it succeeded or failed, so a failing Command never spins.
- Tests run on tokio's paused clock (`start_paused`, which needs `tokio/test-util` as a dev feature).

## 5. Configuration

Configuration is a TOML file passed as `--config <path>` and read via serde with the `toml` crate.

```toml
poll_interval = 60             # seconds
writable = ["..."]             # top-level keys must come before the first table

[optolink]
device = "/dev/ttyUSB0"        # or: host = "...", port = 1234

[mqtt]
host = "broker.local"
port = 8883
client_id = "vcontrol"         # also used in discovery ids, so must be unique per controller
username = "vcontrol"
password = "..."               # VCONTROL_MQTT_PASSWORD overrides this
tls = true
ca_file = "/etc/vcontrol/ca.pem"   # required when tls = true
topic_prefix = "vcontrol"

[homeassistant]
discovery_prefix = "homeassistant"

[[command]]
name = "..."
interval = 3600                # optional, seconds
```

Startup order:

1. Read the TOML.
2. Connect Optolink (with timeout).
3. Detect the Device.
4. Check the config.
5. Connect MQTT.

The check produces a type that can only exist if the config is valid: resolved `&'static Command`s, the writable set and the intervals. The program refuses to start if any of these fail:

- A Command is unknown on the Device.
- A polled Command has a complex type.
- A writable entry is not also polled.
- A writable Command has no write access mode.
- A writable numeric Command has no `lower_bound`/`upper_bound`. Without bounds, out-of-range values wrap around silently (`as` casts in `Command::set`).
- A writable Command's value conversion cannot be reversed. The startup check verifies this by round-tripping a sample value, since `Conversion` is not public.
- `tls = true` without a `ca_file`.

## 6. MQTT contract

| Topic | Payload | Flags |
|---|---|---|
| `<prefix>/<command>` | Plain value (`21.5`), or the Label for mapped Commands. `None` for `Value::Empty`. | retained, QoS 1 |
| `<prefix>/<command>/set` | New value or Label. Subscribed only for Writable Commands. | QoS 1 |
| `<prefix>/status` | `online` / `offline` (Last Will) | retained |
| `<discovery_prefix>/<component>/<client_id>/<object_id>/config` | Discovery JSON | retained |

Payloads:

- Built in `payload.rs` straight from `Value`, never through `OutputValue`'s `Display`. `Display` adds the unit ("21.5 °C").
- `Value::Empty` (all-0xFF data, or a value outside the bounds) is published as `None`, which Home Assistant shows as unknown. An empty payload would delete the retained message instead.

Discovery ids:

- HA requires node and object ids to match `[a-zA-Z0-9_-]+`. Some Command names contain `.`, `~` or umlauts (e.g. `Ecotronic_DA_Förderschnecke`).
- `object_id` is the Command name sanitised to that character set. The raw name stays in the state and set topics.
- `unique_id` is `<client_id>_<object_id>`. It stays stable across restarts and does not collide when two controllers of the same type are connected.
- A test checks every Command name of every Device against the HA regex, and checks that sanitising produces no collisions within a Device. State topics are safe already: no Command name contains `+`, `#` or `/`.

Entity types:

- **Read-only → `sensor`.** Unit comes from `Command::unit()`, except for mapped Commands, which publish Labels.
- **Writable number → `number`.** `min`/`max` from the bounds, and a `step` (see §11, open point).
- **Writable mapped → `select`.** Options are the Mapping's Labels. If the Mapping does not cover the full value range, so that unmapped raw values can occur (e.g. K03 on V200KO2B), the Command is shown as a `sensor` instead, and the startup check refuses it as writable.
- One HA device per controller.

## 7. Trust boundary: incoming writes

Every `/set` message passes through one gate, in this order:

1. Ignore the message if it is retained. A retained `/set` would be re-applied on every reconnect.
2. The topic must match a Writable Command.
3. Parse the payload by the Command's type:
   - Numbers must be finite (`NaN` and `inf` are rejected) and within `lower_bound`/`upper_bound`.
   - Labels are turned back into their number here in the Bridge; unknown Labels are rejected.
   - A raw number for a mapped Command must exist in the Mapping.
4. In `--read-only` mode, log "would write" and stop. Everything above has already been validated at this point.
5. Call `set()` (with timeout).
6. Read the value back and publish what the controller actually has.

Every failure is logged and dropped. Nothing received from MQTT may panic the process. Validating in the gate also matters because `VControl` marks the link as disconnected and renegotiates after *any* error, including a rejected value.

## 8. Lifecycle and failures

- **On every ConnAck** (including reconnects):
  - subscribe to `/set` topics and `<discovery_prefix>/status`
  - publish `online` and all Discovery messages

  Values are not re-sent: they are retained on the broker and refreshed by the next poll.
- **When `<discovery_prefix>/status` is `online`** (Home Assistant restarted): publish Discovery again, as HA recommends.
- **MQTT session:** clean session, so `/set` messages sent while the Bridge was offline are not replayed later.
- **When the broker is unreachable:** `poll()` returns errors immediately. Reconnect attempts wait behind a backoff deadline that is a branch of the `select!`, not a blocking sleep, so polling Optolink continues.
- **When the Optolink link is lost:** exit non-zero, and the systemd unit (`Restart=on-failure`) restarts the process. "Lost" means:
  - an Optolink call times out, or
  - 3 consecutive failures across different Commands.

  A single Command failing on its own is only logged; it can't be told apart from a lost link on one error, because `VControl` returns the same `Err` and reconnects silently.
- **On SIGTERM/SIGINT:** publish `offline`, keep polling MQTT until it is sent (bounded by a short timeout), then disconnect.

## 9. Security

- TLS via rustls; `ca_file` is required when TLS is on.
- Username/password; the environment variable takes priority over the file.
- `/set` topics exist only for allowlisted Writable Commands, and every write passes the gate in §7.
- `--read-only` for the first runs.
- Broker ACLs that restrict who can publish to `<prefix>/+/set` are the operator's job; the ReadMe says so.

## 10. Logging

- `log` + `env_logger` (`RUST_LOG`), as in the CLI.
- Every write is logged at `info` with the Command, the value and the source topic.
- Every rejected write is logged at `warn` with the reason.

## 11. Open points

- **`step` for `number` entities:** the value resolution (e.g. 0.1 for Div10) is hidden in the `pub(crate)` `Conversion`. Either expose a small `Command::step()`-like accessor in the library, or default to `step = 1` and `0.1` for `Double` Commands. Decide in step 3.
- **Cancel-safety of `rumqttc`'s `EventLoop::poll`:** unverified for the pinned version; step 5 settles it.

## 12. Testing (test-first, inside out)

| Step | What | How |
|---|---|---|
| 1 | Config parsing and all startup checks in §5 | Unit tests against the real V200KO2B Device via `Device::detect` |
| 2 | Payloads (`None` for Empty, no unit), Labels, write validation (NaN, bounds, unknown Label, unmapped number), topic routing, retained `/set` ignored | Unit tests |
| 3 | Discovery JSON per entity type; id sanitising against the HA regex across all Devices | Unit tests |
| 4 | Bridge loop: intervals, failed reads rescheduled, writes first, read-back, `--read-only`, timeout → exit, 3 consecutive failures → exit, full publish channel doesn't block | Fake `Controller` (records writes, can hang or fail), paused clock |
| 5 | MQTT end to end: retained state, `/set` round trip, resubscribe after reconnect, Last Will, offline on SIGTERM, Discovery re-publish, cancel-safety | Fake `Controller` + mosquitto in the devcontainer |
| 6 | `Command::set` no longer panics | Library unit test, then the fix |
| 7 | Real controller | On the Pi, first with `--read-only`, then live |

All tests run with `cargo test --features mqtt` inside the devcontainer.

## 13. Build and deploy

- **Devcontainer `Dockerfile`:**
  - the `armv7-unknown-linux-musleabihf` Rust target
  - an armv7 C cross compiler for `ring`. Debian ships no musl cross gcc, so set `CC_armv7_unknown_linux_musleabihf` and `CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_LINKER` explicitly.
  - `mosquitto` for the broker test
- **Build command:** `cargo build --release --features mqtt --bin vcontrol-mqtt --target armv7-unknown-linux-musleabihf`
- **Repo files:**
  - `vcontrol-mqtt.example.toml`
  - `vcontrol-mqtt.service` (`Restart=on-failure`, `EnvironmentFile` for the password)
  - a ReadMe section: build, copy to the Pi, run, and the broker ACL advice
