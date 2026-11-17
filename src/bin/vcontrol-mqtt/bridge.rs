//! The single main loop: scheduling, polling, writes, publishing. Decisions live in the modules without I/O.

use std::{future::Future, time::Duration};

use rumqttc::{
  AsyncClient, ConnectionError, Event, EventLoop, LastWill, MqttOptions, Packet, Publish, QoS,
  TlsConfiguration, Transport,
};
use tokio::time::{Instant, sleep_until, timeout};
use vcontrol::Value;

use crate::{
  config::Checked,
  controller::{Controller, Failure},
  discovery, payload,
  schedule::{Failures, Schedule},
  topics,
};

/// Wait between attempts to reach the broker.
const BACKOFF: Duration = Duration::from_secs(5);
/// How long shutting down may take to send `offline`.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

const ONLINE: &str = "online";
const OFFLINE: &str = "offline";

/// Why the main loop ended.
#[derive(Debug, PartialEq)]
pub enum Exit {
  Shutdown,
  LinkLost,
}

/// The MQTT event loop, replaceable in tests.
pub trait Events {
  async fn poll(&mut self) -> Result<Event, ConnectionError>;
}

impl Events for EventLoop {
  async fn poll(&mut self) -> Result<Event, ConnectionError> {
    EventLoop::poll(self).await
  }
}

/// `ca` is the content of `ca_file`.
pub fn mqtt_options(config: &Checked, ca: Option<Vec<u8>>) -> MqttOptions {
  let mqtt = config.mqtt();

  let mut options = MqttOptions::new(&mqtt.client_id, &mqtt.host, mqtt.port);
  // Clean session: `/set` messages sent while the Bridge was offline are not replayed later.
  options.set_clean_session(true);
  options.set_keep_alive(Duration::from_secs(60));
  options.set_last_will(LastWill::new(topics::status(&mqtt.topic_prefix), OFFLINE, QoS::AtLeastOnce, true));
  if let Some(username) = &mqtt.username {
    options.set_credentials(username, mqtt.password.as_deref().unwrap_or_default());
  }
  if let Some(ca) = ca {
    // rumqttc is built without a crypto provider, so that it is `ring` and never `aws-lc-rs`.
    let _ = rustls::crypto::ring::default_provider().install_default();
    options.set_transport(Transport::tls_with_config(TlsConfiguration::Simple { ca, alpn: None, client_auth: None }));
  }
  options
}

/// Capacity of the publish channel: room for everything sent on a ConnAck, plus a round of values.
pub fn capacity(config: &Checked) -> usize {
  2 * config.commands().len() + 10
}

pub struct Bridge<C> {
  controller: C,
  config: Checked,
  client: AsyncClient,
  read_only: bool,
  /// In the order of `config.commands()`.
  schedule: Schedule,
  failures: Failures,
  connected: bool,
  backoff: Duration,
}

impl<C: Controller> Bridge<C> {
  pub fn new(controller: C, config: Checked, client: AsyncClient, read_only: bool) -> Self {
    let intervals = config.commands().iter().map(|polled| polled.interval).collect();
    Self {
      controller,
      config,
      client,
      read_only,
      schedule: Schedule::new(intervals, Instant::now()),
      failures: Failures::default(),
      connected: false,
      backoff: BACKOFF,
    }
  }

  /// Everything the Bridge publishes is retained.
  fn publish(&self, topic: String, payload: &str) {
    // Never `publish().await`: only this task drains the channel, so waiting for room in it would deadlock.
    if let Err(err) = self.client.try_publish(&topic, QoS::AtLeastOnce, true, payload) {
      log::warn!("Dropping message for {topic}: {err}");
    }
  }

  fn publish_discovery(&self) {
    let device = self.controller.device().name();
    for polled in self.config.commands() {
      let (topic, payload) = discovery::message(&self.config, device, polled);
      self.publish(topic, &payload);
    }
  }

  /// Runs on every ConnAck, including reconnects.
  fn on_connect(&mut self) {
    log::info!("Connected to the MQTT broker.");
    self.connected = true;

    let prefix = &self.config.mqtt().topic_prefix;
    let set_topics =
      self.config.commands().iter().filter(|polled| polled.writable).map(|polled| topics::set(prefix, &polled.name));
    for topic in set_topics.chain([topics::status(self.config.discovery_prefix())]) {
      if let Err(err) = self.client.try_subscribe(&topic, QoS::AtLeastOnce) {
        log::warn!("Could not subscribe to {topic}: {err}");
      }
    }

    self.publish(topics::status(prefix), ONLINE);
    self.publish_discovery();

    // Nothing was published while the broker was away, and a restarted broker may have lost what it retained
    // (a Command with a long interval would then have no value for hours). So read everything again now.
    self.schedule.all_due(Instant::now());
  }

  /// Counts a controller call. `Err` when the Optolink link is lost, `Ok(None)` when only this call failed.
  fn check<T>(&mut self, command: &str, result: Result<T, Failure>) -> Result<Option<T>, Exit> {
    match result {
      Ok(value) => {
        self.failures.success();
        Ok(Some(value))
      },
      Err(Failure::Timeout) => {
        log::error!("{command}: timed out, the Optolink link is lost.");
        Err(Exit::LinkLost)
      },
      Err(Failure::Error(err)) => {
        log::warn!("{command}: {err}");
        if self.failures.failure(command) {
          log::error!("Too many consecutive failures, the Optolink link is lost.");
          return Err(Exit::LinkLost);
        }
        Ok(None)
      },
    }
  }

  /// Reads one Command and publishes its value.
  async fn read(&mut self, index: usize) -> Result<(), Exit> {
    let polled = &self.config.commands()[index];
    let (name, command) = (polled.name.clone(), polled.command);
    self.schedule.started(index, Instant::now());

    let result = self.controller.get(&name).await;
    let Some(value) = self.check(&name, result)? else { return Ok(()) };

    // While the broker is away nothing drains the channel; queued values would crowd out what a ConnAck sends.
    if self.connected {
      match payload::state(command, &value) {
        Some(payload) => self.publish(topics::state(&self.config.mqtt().topic_prefix, &name), &payload),
        None => log::warn!("{name}: cannot publish {value:?}"),
      }
    }
    Ok(())
  }

  /// The trust boundary: every incoming message passes through here. Failures are logged and dropped.
  async fn on_message(&mut self, message: Publish) -> Result<(), Exit> {
    let topic = message.topic.as_str();

    if topic == topics::status(self.config.discovery_prefix()) {
      // Home Assistant restarted.
      if message.payload == ONLINE {
        self.publish_discovery();
      }
      return Ok(());
    }

    let Some(name) = topics::set_target(&self.config.mqtt().topic_prefix, topic, message.retain) else {
      log::warn!("Rejected write on {topic}: retained, or not a set topic.");
      return Ok(());
    };
    let Some(index) = self.config.commands().iter().position(|polled| polled.writable && polled.name == name) else {
      log::warn!("Rejected write on {topic}: {name} is not a Writable Command.");
      return Ok(());
    };
    let value: Value = match payload::parse_write(self.config.commands()[index].command, &message.payload) {
      Ok(value) => value,
      Err(reason) => {
        log::warn!("Rejected write on {topic}: {reason}.");
        return Ok(());
      },
    };

    if self.read_only {
      log::info!("Read-only mode: would write {name} = {value:?} (from {topic}).");
      return Ok(());
    }

    log::info!("Writing {name} = {value:?} (from {topic}).");
    let result = self.controller.set(name, value).await;
    if self.check(name, result)?.is_none() {
      return Ok(());
    }

    // Publish what the controller actually has.
    self.read(index).await
  }

  /// Queues `offline` and the disconnect. The caller keeps polling MQTT until they are sent.
  fn say_goodbye(&self) {
    log::info!("Shutting down.");
    self.publish(topics::status(&self.config.mqtt().topic_prefix), OFFLINE);
    if let Err(err) = self.client.try_disconnect() {
      log::warn!("Could not disconnect from the MQTT broker: {err}");
    }
  }

  pub async fn run(mut self, mut events: impl Events, shutdown: impl Future<Output = ()>) -> Exit {
    tokio::pin!(shutdown);
    let mut retry_at = Instant::now();

    loop {
      let event = {
        // While the broker is unreachable, `poll()` fails immediately. The backoff is part of this future,
        // not a blocking sleep, so polling Optolink continues.
        //
        // The future is kept across reads instead of being dropped and recreated in every pass:
        // `EventLoop::poll` is not cancel-safe (it would lose a half-written packet or a connect in progress).
        let poll = async {
          sleep_until(retry_at).await;
          events.poll().await
        };
        tokio::pin!(poll);

        loop {
          let (index, due) = self.schedule.next();

          let shut_down = tokio::select! {
            // MQTT is checked before reads, so a pending `/set` is handled before the next scheduled read.
            biased;
            _ = &mut shutdown => true,
            event = &mut poll => break Some(event),
            // ONE read per pass, never a batch: MQTT must be polled well within the keep-alive (60 s).
            // A single read takes about 0.5 s, a whole round can take far longer.
            _ = sleep_until(due) => tokio::select! {
              biased;
              // A read may be abandoned for a shutdown; a hanging one must not turn it into "link lost".
              _ = &mut shutdown => true,
              result = self.read(index) => match result {
                Ok(()) => false,
                Err(exit) => return exit,
              },
            },
          };

          if shut_down {
            self.say_goodbye();
            // Keep polling until the broker closes the connection. Closing it ourselves right after sending the
            // disconnect would race with the broker's acknowledgement of `offline`. The poll in flight is
            // finished first, not dropped.
            break match timeout(SHUTDOWN_TIMEOUT, &mut poll).await {
              // The connection is still up: go on polling below, with `poll` complete.
              Ok(Ok(_)) => None,
              _ => return Exit::Shutdown,
            };
          }
        }
      };

      match event {
        None => {
          let sent = async { while events.poll().await.is_ok() {} };
          if timeout(SHUTDOWN_TIMEOUT, sent).await.is_err() {
            log::warn!("Could not send `offline` in time.");
          }
          return Exit::Shutdown;
        },
        Some(Ok(Event::Incoming(Packet::ConnAck(_)))) => self.on_connect(),
        Some(Ok(Event::Incoming(Packet::Publish(message)))) => {
          if let Err(exit) = self.on_message(message).await {
            return exit;
          }
        },
        Some(Ok(_)) => (),
        Some(Err(err)) => {
          log::warn!("MQTT: {err}");
          self.connected = false;
          retry_at = Instant::now() + self.backoff;
        },
      }
    }
  }
}

#[cfg(test)]
mod tests {
  mod broker;

  use std::{
    cell::Cell,
    collections::{HashMap, HashSet},
    rc::Rc,
    sync::{Arc, Mutex},
  };

  use rumqttc::{ConnAck, ConnectReturnCode, Request};
  use tokio::{
    sync::{mpsc, oneshot},
    task::{JoinHandle, LocalSet, spawn_local},
    time::sleep,
  };
  use vcontrol::Device;

  use super::*;
  use crate::{
    config::{
      Config,
      tests::{checked, device},
    },
    controller::with_timeout,
  };

  const SETPOINT: &str = "Bedien_WW_Solltemperatur";
  const TEMPERATURE: &str = "ADC_Isttemperaturwert_2";
  const PRIORITY: &str = "KA2_KonfiSpeichervorrangA1M1";

  #[derive(Debug, Clone, PartialEq)]
  pub enum Call {
    Get(String),
    Set(String, Value),
  }

  #[derive(Default)]
  pub struct FakeState {
    pub values: HashMap<String, Value>,
    pub calls: Vec<Call>,
    pub failing: HashSet<String>,
    pub hanging: HashSet<String>,
    /// Writes are acknowledged but do not change the value.
    pub ignore_writes: bool,
    pub read_time: Duration,
  }

  /// A controller that records calls and can fail or hang.
  #[derive(Clone, Default)]
  pub struct Fake(pub Arc<Mutex<FakeState>>);

  impl Fake {
    pub fn state(&self) -> std::sync::MutexGuard<'_, FakeState> {
      self.0.lock().unwrap()
    }

    pub fn gets(&self, command: &str) -> usize {
      self.state().calls.iter().filter(|call| **call == Call::Get(command.into())).count()
    }

    pub fn sets(&self) -> Vec<Call> {
      self.state().calls.iter().filter(|call| matches!(call, Call::Set(..))).cloned().collect()
    }

    async fn call(&self, call: Call) -> Result<Value, Failure> {
      let (name, read_time) = match &call {
        Call::Get(name) | Call::Set(name, _) => (name.clone(), self.state().read_time),
      };
      self.state().calls.push(call.clone());
      sleep(read_time).await;

      if self.state().hanging.contains(&name) {
        return with_timeout(crate::controller::TIMEOUT, std::future::pending::<Result<Value, String>>()).await;
      }
      let mut state = self.state();
      if state.failing.contains(&name) {
        return Err(Failure::Error("fake failure".into()));
      }
      if let Call::Set(_, value) = call
        && !state.ignore_writes
      {
        state.values.insert(name.clone(), value);
      }
      Ok(state.values.get(&name).cloned().unwrap_or(Value::Empty))
    }
  }

  impl Controller for Fake {
    fn device(&self) -> &'static Device {
      device()
    }

    async fn get(&mut self, command: &str) -> Result<Value, Failure> {
      self.call(Call::Get(command.into())).await
    }

    async fn set(&mut self, command: &str, value: Value) -> Result<(), Failure> {
      self.call(Call::Set(command.into(), value)).await.map(drop)
    }
  }

  /// MQTT events fed by the test.
  struct FakeEvents {
    events: mpsc::UnboundedReceiver<Result<Event, ConnectionError>>,
    polls: Rc<Cell<usize>>,
  }

  impl Events for FakeEvents {
    async fn poll(&mut self) -> Result<Event, ConnectionError> {
      self.polls.set(self.polls.get() + 1);
      match self.events.recv().await {
        Some(event) => event,
        None => std::future::pending().await,
      }
    }
  }

  struct Harness {
    fake: Fake,
    requests: flume::Receiver<Request>,
    events: mpsc::UnboundedSender<Result<Event, ConnectionError>>,
    polls: Rc<Cell<usize>>,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<Exit>,
  }

  fn connack() -> Result<Event, ConnectionError> {
    Ok(Event::Incoming(Packet::ConnAck(ConnAck { session_present: false, code: ConnectReturnCode::Success })))
  }

  fn message(topic: &str, payload: &str) -> Result<Event, ConnectionError> {
    Ok(Event::Incoming(Packet::Publish(Publish::new(topic, QoS::AtLeastOnce, payload))))
  }

  impl Harness {
    /// Starts a Bridge with `TEMPERATURE`, `SETPOINT` (writable) and `PRIORITY` (writable).
    fn start(read_only: bool) -> Self {
      let top = format!("writable = [\"{SETPOINT}\", \"{PRIORITY}\"]");
      Self::start_with(checked(&top, &[TEMPERATURE, SETPOINT, PRIORITY]), read_only, 100, Fake::default())
    }

    fn start_with(config: Checked, read_only: bool, capacity: usize, fake: Fake) -> Self {
      let (request_tx, requests) = flume::bounded(capacity);
      let (events, event_rx) = mpsc::unbounded_channel();
      let (shutdown, shutdown_rx) = oneshot::channel::<()>();
      let polls = Rc::new(Cell::new(0));

      let bridge = Bridge::new(fake.clone(), config, AsyncClient::from_senders(request_tx), read_only);
      let fake_events = FakeEvents { events: event_rx, polls: polls.clone() };
      let task = spawn_local(bridge.run(fake_events, async { shutdown_rx.await.unwrap_or(()) }));

      Self { fake, requests, events, polls, shutdown: Some(shutdown), task }
    }

    fn send(&self, event: Result<Event, ConnectionError>) {
      self.events.send(event).unwrap();
    }

    /// Everything published since the last call, as (topic, payload).
    fn published(&self) -> Vec<(String, String)> {
      self
        .requests
        .try_iter()
        .filter_map(|request| match request {
          Request::Publish(publish) => {
            assert!(publish.retain);
            assert_eq!(publish.qos, QoS::AtLeastOnce);
            Some((publish.topic, String::from_utf8(publish.payload.to_vec()).unwrap()))
          },
          _ => None,
        })
        .collect()
    }

    /// The state values published since the last call.
    fn states(&self) -> Vec<(String, String)> {
      let prefix = "vcontrol/";
      self
        .published()
        .into_iter()
        .filter(|(topic, _)| topic != "vcontrol/status")
        .filter_map(|(topic, payload)| Some((topic.strip_prefix(prefix)?.to_owned(), payload)))
        .collect()
    }
  }

  /// Runs a test on the paused clock.
  async fn local<F: Future>(test: F) -> F::Output {
    LocalSet::new().run_until(test).await
  }

  fn pair(a: &str, b: &str) -> (String, String) {
    (a.to_owned(), b.to_owned())
  }

  #[tokio::test(start_paused = true)]
  async fn connack_subscribes_and_announces() {
    local(async {
      let bridge = Harness::start(false);
      bridge.send(connack());
      sleep(Duration::from_millis(1)).await;

      let requests: Vec<Request> = bridge.requests.try_iter().collect();
      let subscriptions: Vec<String> = requests
        .iter()
        .filter_map(|request| match request {
          Request::Subscribe(subscribe) => Some(subscribe.filters[0].path.clone()),
          _ => None,
        })
        .collect();
      assert_eq!(
        subscriptions,
        [format!("vcontrol/{SETPOINT}/set"), format!("vcontrol/{PRIORITY}/set"), "homeassistant/status".to_owned()]
      );

      let topics: Vec<String> = requests
        .iter()
        .filter_map(|request| match request {
          Request::Publish(publish) => Some(publish.topic.clone()),
          _ => None,
        })
        .collect();
      assert_eq!(
        topics,
        [
          "vcontrol/status".to_owned(),
          format!("homeassistant/sensor/vcontrol/{TEMPERATURE}/config"),
          format!("homeassistant/number/vcontrol/{SETPOINT}/config"),
          format!("homeassistant/select/vcontrol/{PRIORITY}/config"),
          // The first values follow immediately.
          format!("vcontrol/{TEMPERATURE}"),
          format!("vcontrol/{SETPOINT}"),
          format!("vcontrol/{PRIORITY}"),
        ]
      );

      // A reconnect announces again and reads all values again: the broker may have lost what it retained.
      bridge.send(connack());
      sleep(Duration::from_millis(1)).await;
      let published = bridge.published();
      assert_eq!(published.len(), 7);
      assert_eq!(published[0], pair("vcontrol/status", "online"));
      assert_eq!(bridge.fake.gets(TEMPERATURE), 2);
    })
    .await
  }

  #[tokio::test(start_paused = true)]
  async fn home_assistant_restart_publishes_discovery_again() {
    local(async {
      let bridge = Harness::start(false);
      bridge.send(connack());
      sleep(Duration::from_millis(1)).await;
      bridge.published();

      bridge.send(message("homeassistant/status", "offline"));
      sleep(Duration::from_millis(1)).await;
      assert_eq!(bridge.published(), []);

      bridge.send(message("homeassistant/status", "online"));
      sleep(Duration::from_millis(1)).await;
      let published = bridge.published();
      assert_eq!(published.len(), 3);
      assert!(published.iter().all(|(topic, _)| topic.starts_with("homeassistant/") && topic.ends_with("/config")));
    })
    .await
  }

  #[tokio::test(start_paused = true)]
  async fn polls_at_the_global_and_per_command_interval() {
    local(async {
      let config = Config::parse(
        &format!(
          "poll_interval = 60\n[optolink]\nport = 1\n[mqtt]\nhost = \"h\"\n[[command]]\nname = \"{TEMPERATURE}\"\n[[command]]\nname = \"{SETPOINT}\"\ninterval = 3600\n"
        ),
        None,
      )
      .unwrap()
      .check(device())
      .unwrap();
      let fake = Fake::default();
      fake.state().values.insert(TEMPERATURE.into(), Value::Double(21.5));
      fake.state().values.insert(SETPOINT.into(), Value::Int(50));
      let bridge = Harness::start_with(config, false, 1000, fake);
      bridge.send(connack());

      sleep(Duration::from_secs(1)).await;
      assert_eq!(bridge.states(), [pair(TEMPERATURE, "21.5"), pair(SETPOINT, "50")]);

      sleep(Duration::from_secs(58)).await;
      assert_eq!(bridge.states(), []);
      sleep(Duration::from_secs(2)).await;
      assert_eq!(bridge.states(), [pair(TEMPERATURE, "21.5")]);

      sleep(Duration::from_secs(3540)).await;
      assert_eq!(bridge.states().len(), 60);
      assert_eq!((bridge.fake.gets(TEMPERATURE), bridge.fake.gets(SETPOINT)), (61, 2));
    })
    .await
  }

  #[tokio::test(start_paused = true)]
  async fn empty_value_is_published_as_none() {
    local(async {
      let bridge = Harness::start(false);
      bridge.send(connack());
      sleep(Duration::from_secs(1)).await;
      assert!(bridge.states().contains(&pair(TEMPERATURE, "None")));
    })
    .await
  }

  #[tokio::test(start_paused = true)]
  async fn failed_read_is_rescheduled_and_does_not_end_the_bridge() {
    local(async {
      let bridge = Harness::start(false);
      bridge.fake.state().failing.insert(TEMPERATURE.into());
      bridge.send(connack());

      sleep(Duration::from_secs(59)).await;
      assert_eq!(bridge.fake.gets(TEMPERATURE), 1);
      assert!(!bridge.states().iter().any(|(name, _)| name == TEMPERATURE));

      sleep(Duration::from_secs(600)).await;
      assert_eq!(bridge.fake.gets(TEMPERATURE), 11);
      assert_eq!(bridge.fake.gets(SETPOINT), 11);
      assert!(!bridge.task.is_finished());
    })
    .await
  }

  #[tokio::test(start_paused = true)]
  async fn three_consecutive_failures_across_commands_end_the_bridge() {
    local(async {
      let bridge = Harness::start(false);
      bridge.fake.state().failing.extend([TEMPERATURE.to_owned(), SETPOINT.to_owned(), PRIORITY.to_owned()]);
      assert_eq!(bridge.task.await.unwrap(), Exit::LinkLost);
    })
    .await
  }

  #[tokio::test(start_paused = true)]
  async fn two_failing_commands_do_not_end_the_bridge_while_another_succeeds() {
    local(async {
      let bridge = Harness::start(false);
      bridge.fake.state().failing.extend([TEMPERATURE.to_owned(), PRIORITY.to_owned()]);
      sleep(Duration::from_secs(600)).await;
      assert!(!bridge.task.is_finished());
    })
    .await
  }

  #[tokio::test(start_paused = true)]
  async fn timeout_ends_the_bridge() {
    local(async {
      let bridge = Harness::start(false);
      bridge.fake.state().hanging.insert(SETPOINT.into());
      let start = Instant::now();
      assert_eq!(bridge.task.await.unwrap(), Exit::LinkLost);
      assert_eq!(start.elapsed(), crate::controller::TIMEOUT);
    })
    .await
  }

  #[tokio::test(start_paused = true)]
  async fn write_comes_before_due_reads_and_is_read_back() {
    local(async {
      let bridge = Harness::start(false);
      bridge.fake.state().read_time = Duration::from_millis(100);
      bridge.send(connack());
      sleep(Duration::from_secs(1)).await;
      bridge.fake.state().calls.clear();
      bridge.published();

      // The write arrives during the first read of the next round, while the other reads are due.
      sleep(Duration::from_millis(59_050)).await;
      bridge.send(message(&format!("vcontrol/{SETPOINT}/set"), "55"));
      sleep(Duration::from_secs(1)).await;

      let get = |name: &str| Call::Get(name.to_owned());
      assert_eq!(
        bridge.fake.state().calls,
        [
          get(TEMPERATURE),
          Call::Set(SETPOINT.into(), Value::Int(55)),
          // The read-back; it also counts as this round's read.
          get(SETPOINT),
          get(PRIORITY)
        ]
      );
      assert_eq!(bridge.states()[1], pair(SETPOINT, "55"));
    })
    .await
  }

  #[tokio::test(start_paused = true)]
  async fn read_back_publishes_what_the_controller_has() {
    local(async {
      let bridge = Harness::start(false);
      bridge.fake.state().values.insert(SETPOINT.into(), Value::Int(50));
      bridge.fake.state().ignore_writes = true;
      bridge.send(connack());
      sleep(Duration::from_secs(1)).await;
      bridge.published();

      bridge.send(message(&format!("vcontrol/{SETPOINT}/set"), "55"));
      sleep(Duration::from_secs(1)).await;
      assert_eq!(bridge.states(), [pair(SETPOINT, "50")]);
    })
    .await
  }

  #[tokio::test(start_paused = true)]
  async fn label_is_written_as_its_number() {
    local(async {
      let bridge = Harness::start(false);
      bridge.send(connack());
      sleep(Duration::from_secs(1)).await;
      bridge.published();

      bridge.send(message(&format!("vcontrol/{PRIORITY}/set"), "Cylinder priority"));
      sleep(Duration::from_secs(1)).await;
      assert_eq!(bridge.fake.sets(), [Call::Set(PRIORITY.into(), Value::Int(2))]);
      assert_eq!(bridge.states(), [pair(PRIORITY, "Cylinder priority")]);
    })
    .await
  }

  #[tokio::test(start_paused = true)]
  async fn read_only_mode_never_writes() {
    local(async {
      let bridge = Harness::start(true);
      bridge.send(connack());
      sleep(Duration::from_secs(1)).await;
      bridge.published();

      bridge.send(message(&format!("vcontrol/{SETPOINT}/set"), "55"));
      sleep(Duration::from_secs(1)).await;
      assert_eq!(bridge.fake.sets(), []);
      assert_eq!(bridge.states(), []);
    })
    .await
  }

  #[tokio::test(start_paused = true)]
  async fn invalid_writes_are_dropped() {
    local(async {
      let bridge = Harness::start(false);
      bridge.send(connack());
      sleep(Duration::from_secs(1)).await;

      let set = format!("vcontrol/{SETPOINT}/set");
      let mut retained = Publish::new(&set, QoS::AtLeastOnce, "55");
      retained.retain = true;
      bridge.send(Ok(Event::Incoming(Packet::Publish(retained))));
      for payload in ["NaN", "inf", "9", "96", "50.5", "", "abc"] {
        bridge.send(message(&set, payload));
      }
      bridge.send(Ok(Event::Incoming(Packet::Publish(Publish::new(&set, QoS::AtLeastOnce, vec![0xff, 0xfe])))));
      bridge.send(message(&format!("vcontrol/{PRIORITY}/set"), "Nonsense"));
      bridge.send(message(&format!("vcontrol/{PRIORITY}/set"), "200"));
      // Not allowlisted, unknown, and not a set topic at all.
      bridge.send(message(&format!("vcontrol/{TEMPERATURE}/set"), "20"));
      bridge.send(message("vcontrol/Nope/set", "1"));
      bridge.send(message(&format!("vcontrol/{SETPOINT}"), "55"));
      bridge.send(message("other/topic", "55"));

      sleep(Duration::from_secs(1)).await;
      assert_eq!(bridge.fake.sets(), []);
      assert!(!bridge.task.is_finished());

      // The Bridge still works.
      bridge.send(message(&set, "55"));
      sleep(Duration::from_secs(1)).await;
      assert_eq!(bridge.fake.sets(), [Call::Set(SETPOINT.into(), Value::Int(55))]);
    })
    .await
  }

  #[tokio::test(start_paused = true)]
  async fn failed_write_is_not_read_back() {
    local(async {
      let bridge = Harness::start(false);
      bridge.send(connack());
      sleep(Duration::from_secs(1)).await;
      bridge.fake.state().calls.clear();

      bridge.fake.state().failing.insert(SETPOINT.into());
      bridge.send(message(&format!("vcontrol/{SETPOINT}/set"), "55"));
      sleep(Duration::from_secs(1)).await;
      assert_eq!(bridge.fake.state().calls, [Call::Set(SETPOINT.into(), Value::Int(55))]);
      assert!(!bridge.task.is_finished());
    })
    .await
  }

  #[tokio::test(start_paused = true)]
  async fn hanging_write_ends_the_bridge() {
    local(async {
      let bridge = Harness::start(false);
      bridge.send(connack());
      sleep(Duration::from_secs(1)).await;

      bridge.fake.state().hanging.insert(SETPOINT.into());
      bridge.send(message(&format!("vcontrol/{SETPOINT}/set"), "55"));
      assert_eq!(bridge.task.await.unwrap(), Exit::LinkLost);
    })
    .await
  }

  #[tokio::test(start_paused = true)]
  async fn full_publish_channel_does_not_block() {
    local(async {
      // Room for a single message that nobody ever takes out.
      let bridge = Harness::start_with(checked("", &[TEMPERATURE, SETPOINT]), false, 1, Fake::default());
      bridge.send(connack());

      sleep(Duration::from_secs(601)).await;
      assert_eq!(bridge.fake.gets(TEMPERATURE), 11);
      assert_eq!(bridge.requests.len(), 1);
      assert!(!bridge.task.is_finished());
    })
    .await
  }

  #[tokio::test(start_paused = true)]
  async fn unreachable_broker_backs_off_while_polling_continues() {
    local(async {
      let bridge = Harness::start(false);
      bridge.send(connack());
      sleep(Duration::from_secs(1)).await;
      bridge.published();
      let polls = bridge.polls.get();

      bridge.send(Err(ConnectionError::RequestsDone));
      sleep(Duration::from_secs(1)).await;
      // In backoff: the event loop is not polled again yet.
      assert_eq!(bridge.polls.get(), polls);
      sleep(Duration::from_secs(5)).await;
      assert_eq!(bridge.polls.get(), polls + 1);

      // Optolink is still polled, but nothing is queued for the absent broker.
      sleep(Duration::from_secs(120)).await;
      assert_eq!(bridge.fake.gets(TEMPERATURE), 3);
      assert_eq!(bridge.published(), []);

      // Back again: announce and publish all values right away, then at their interval.
      bridge.send(connack());
      sleep(Duration::from_millis(1)).await;
      assert_eq!(bridge.published().len(), 7);
      sleep(Duration::from_secs(59)).await;
      assert_eq!(bridge.states().len(), 0);
      sleep(Duration::from_secs(1)).await;
      assert_eq!(bridge.states().len(), 3);
    })
    .await
  }

  #[tokio::test(start_paused = true)]
  async fn values_read_before_the_first_connection_are_read_again() {
    local(async {
      let bridge = Harness::start(false);
      sleep(Duration::from_secs(30)).await;
      assert_eq!(bridge.fake.gets(TEMPERATURE), 1);
      assert_eq!(bridge.published(), []);

      bridge.send(connack());
      sleep(Duration::from_secs(1)).await;
      assert_eq!(bridge.fake.gets(TEMPERATURE), 2);
      assert_eq!(bridge.states().len(), 3);
    })
    .await
  }

  #[tokio::test(start_paused = true)]
  async fn shutdown_publishes_offline_and_disconnects() {
    local(async {
      let mut bridge = Harness::start(false);
      bridge.send(connack());
      sleep(Duration::from_secs(1)).await;
      bridge.requests.try_iter().count();

      bridge.shutdown.take().unwrap().send(()).unwrap();
      assert_eq!(bridge.task.await.unwrap(), Exit::Shutdown);

      let requests: Vec<Request> = bridge.requests.try_iter().collect();
      assert!(matches!(&requests[0], Request::Publish(publish) if publish.topic == "vcontrol/status" && publish.payload == "offline" && publish.retain));
      assert!(matches!(&requests[1], Request::Disconnect(_)));
      assert_eq!(requests.len(), 2);
    })
    .await
  }

  #[tokio::test(start_paused = true)]
  async fn shutdown_during_a_hanging_read_is_still_a_shutdown() {
    local(async {
      let mut bridge = Harness::start(false);
      bridge.send(connack());
      sleep(Duration::from_secs(1)).await;
      bridge.requests.try_iter().count();

      // The next read hangs; the shutdown arrives one second into it.
      bridge.fake.state().hanging.insert(TEMPERATURE.into());
      sleep(Duration::from_secs(60)).await;
      assert!(!bridge.task.is_finished());
      bridge.shutdown.take().unwrap().send(()).unwrap();

      assert_eq!(bridge.task.await.unwrap(), Exit::Shutdown);
      let requests: Vec<Request> = bridge.requests.try_iter().collect();
      assert!(matches!(&requests[0], Request::Publish(publish) if publish.payload == "offline"));
      assert!(matches!(&requests[1], Request::Disconnect(_)));
    })
    .await
  }

  #[test]
  fn options_and_capacity() {
    let config = checked("", &[TEMPERATURE, SETPOINT]);
    assert_eq!(capacity(&config), 14);

    let options = mqtt_options(&config, None);
    assert!(options.clean_session());
    assert_eq!(options.keep_alive(), Duration::from_secs(60));
    assert_eq!(options.credentials(), None);
    assert!(matches!(options.transport(), Transport::Tcp));
    let will = options.last_will().unwrap();
    assert_eq!((will.topic.as_str(), &will.message[..], will.retain), ("vcontrol/status", &b"offline"[..], true));

    assert!(matches!(mqtt_options(&config, Some(vec![])).transport(), Transport::Tls(_)));
  }
}
