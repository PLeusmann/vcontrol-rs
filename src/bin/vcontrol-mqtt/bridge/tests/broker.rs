//! The Bridge end to end against a real mosquitto, with the fake controller. Runs in the devcontainer.

use std::{
  fs,
  path::PathBuf,
  process::{Child, Command, Stdio},
  sync::atomic::{AtomicU16, Ordering},
};

use tokio::net::TcpStream;

use super::*;
use crate::payload::{self, Kind};

struct Broker {
  child: Option<Child>,
  port: u16,
  dir: PathBuf,
}

impl Broker {
  /// Starts mosquitto on a free port. `settings` are extra lines for its config; `{dir}` is replaced by a scratch directory.
  async fn start(settings: &str) -> Self {
    let mut broker = Self::prepare(settings);
    broker.restart().await;
    broker
  }

  /// Like `start`, but leaves starting to a later `restart`, after the files the config names are in place.
  fn prepare(settings: &str) -> Self {
    // Not a port the OS hands out: a just released one can go to another test's client socket before mosquitto binds it.
    static NEXT: AtomicU16 = AtomicU16::new(0);
    let port = 20000 + (std::process::id() % 1000) as u16 * 10 + NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("vcontrol-mqtt-test-{port}"));
    fs::create_dir_all(&dir).unwrap();
    let settings = settings.replace("{dir}", dir.to_str().unwrap());
    fs::write(
      dir.join("mosquitto.conf"),
      format!("listener {port} 127.0.0.1\nuser root\nlog_dest file {}/log\n{settings}\n", dir.display()),
    )
    .unwrap();

    Self { child: None, port, dir }
  }

  fn stop(&mut self) {
    if let Some(mut child) = self.child.take() {
      child.kill().unwrap();
      child.wait().unwrap();
    }
  }

  /// (Re)starts the broker on the same port. It forgets all sessions and retained messages.
  async fn restart(&mut self) {
    self.stop();
    self.child = Some(
      Command::new("mosquitto")
        .arg("-c")
        .arg(self.dir.join("mosquitto.conf"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("mosquitto is not installed; run the tests in the devcontainer"),
    );

    for _ in 0..100 {
      if TcpStream::connect(("127.0.0.1", self.port)).await.is_ok() {
        return;
      }
      sleep(Duration::from_millis(50)).await;
    }
    panic!("mosquitto did not start");
  }

  /// Runs a tool in the scratch directory.
  fn tool(&self, program: &str, args: &[&str]) {
    let status =
      Command::new(program).args(args).current_dir(&self.dir).stdout(Stdio::null()).stderr(Stdio::null()).status();
    assert!(status.unwrap().success(), "{program} {args:?}");
  }

  fn config(&self, top: &str, mqtt: &str, commands: &[&str]) -> Checked {
    let commands: String = commands.iter().map(|name| format!("[[command]]\nname = \"{name}\"\n")).collect();
    let toml = format!(
      "poll_interval = 1\n{top}\n[optolink]\nport = 1\n[mqtt]\nhost = \"127.0.0.1\"\nport = {}\n{mqtt}\n{commands}",
      self.port
    );
    Config::parse(&toml, None).unwrap().check(device()).unwrap()
  }

  /// The config used by most tests: `TEMPERATURE` and a writable `SETPOINT`.
  fn default_config(&self) -> Checked {
    self.config(&format!("writable = [\"{SETPOINT}\"]"), "", &[TEMPERATURE, SETPOINT])
  }

  fn observer_options(&self) -> MqttOptions {
    MqttOptions::new(format!("observer-{}", rand_id()), "127.0.0.1", self.port)
  }
}

impl Drop for Broker {
  fn drop(&mut self) {
    self.stop();
    let _ = fs::remove_dir_all(&self.dir);
  }
}

fn rand_id() -> u128 {
  std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
}

/// A second MQTT client that watches what the Bridge does.
struct Observer {
  client: AsyncClient,
  messages: mpsc::UnboundedReceiver<Publish>,
}

impl Observer {
  async fn connect(options: MqttOptions, filters: &[&str]) -> Self {
    let (client, mut event_loop) = AsyncClient::new(options, 1000);
    for filter in filters {
      client.subscribe(*filter, QoS::AtLeastOnce).await.unwrap();
    }

    let (sender, messages) = mpsc::unbounded_channel();
    spawn_local(async move {
      loop {
        match event_loop.poll().await {
          Ok(Event::Incoming(Packet::Publish(message))) => {
            if sender.send(message).is_err() {
              break;
            }
          },
          Ok(_) => (),
          Err(_) => sleep(Duration::from_millis(100)).await,
        }
      }
    });

    Self { client, messages }
  }

  /// Waits for a message on `topic` that matches.
  async fn expect(&mut self, topic: &str, matches: impl Fn(&Publish) -> bool) -> Publish {
    let wait = async {
      loop {
        let message = self.messages.recv().await.expect("observer ended");
        if message.topic == topic && matches(&message) {
          return message;
        }
      }
    };
    timeout(Duration::from_secs(10), wait).await.unwrap_or_else(|_| panic!("no matching message on {topic}"))
  }

  async fn expect_payload(&mut self, topic: &str, payload: &str) -> Publish {
    self.expect(topic, |message| message.payload == payload).await
  }

  async fn publish(&self, topic: &str, payload: &str) {
    self.client.publish(topic, QoS::AtLeastOnce, false, payload).await.unwrap();
  }
}

struct Running {
  fake: Fake,
  shutdown: oneshot::Sender<()>,
  task: JoinHandle<Exit>,
}

fn start_bridge(config: Checked, ca: Option<Vec<u8>>, fake: Fake) -> Running {
  let (client, event_loop) = AsyncClient::new(mqtt_options(&config, ca), capacity(&config));
  let (shutdown, shutdown_rx) = oneshot::channel::<()>();

  let mut bridge = Bridge::new(fake.clone(), config, client, false);
  bridge.backoff = Duration::from_millis(200);
  let task = spawn_local(bridge.run(event_loop, async { shutdown_rx.await.unwrap_or(()) }));

  Running { fake, shutdown, task }
}

fn fake_with_values() -> Fake {
  let fake = Fake::default();
  fake.state().values.insert(TEMPERATURE.into(), Value::Double(21.5));
  fake.state().values.insert(SETPOINT.into(), Value::Int(50));
  fake
}

async fn wait_for(what: &str, condition: impl Fn() -> bool) {
  for _ in 0..600 {
    if condition() {
      return;
    }
    sleep(Duration::from_millis(50)).await;
  }
  panic!("timed out waiting for {what}");
}

const STATUS: &str = "vcontrol/status";

fn state_topic(command: &str) -> String {
  format!("vcontrol/{command}")
}

fn set_topic(command: &str) -> String {
  format!("vcontrol/{command}/set")
}

#[tokio::test]
async fn state_status_and_discovery_are_retained() {
  local(async {
    let broker = Broker::start("allow_anonymous true").await;
    let _bridge = start_bridge(broker.default_config(), None, fake_with_values());

    // Wait until the Bridge has published, then look with a client that connects afterwards.
    Observer::connect(broker.observer_options(), &["#"]).await.expect_payload(&state_topic(SETPOINT), "50").await;

    let mut late = Observer::connect(broker.observer_options(), &["#"]).await;
    let mut seen = HashMap::new();
    while seen.len() < 5 {
      let message = timeout(Duration::from_secs(10), late.messages.recv()).await.expect("retained messages").unwrap();
      if message.retain {
        seen.insert(message.topic.clone(), String::from_utf8(message.payload.to_vec()).unwrap());
      }
    }
    assert_eq!(seen[STATUS], "online");
    assert_eq!(seen[&state_topic(TEMPERATURE)], "21.5");
    assert_eq!(seen[&state_topic(SETPOINT)], "50");
    assert!(seen[&format!("homeassistant/sensor/vcontrol/{TEMPERATURE}/config")].contains("\"state_topic\""));
    assert!(seen[&format!("homeassistant/number/vcontrol/{SETPOINT}/config")].contains("\"command_topic\""));
  })
  .await
}

#[tokio::test]
async fn set_round_trip() {
  local(async {
    let broker = Broker::start("allow_anonymous true").await;
    let bridge = start_bridge(broker.default_config(), None, fake_with_values());
    let mut observer = Observer::connect(broker.observer_options(), &["vcontrol/#"]).await;
    observer.expect_payload(&state_topic(SETPOINT), "50").await;

    observer.publish(&set_topic(SETPOINT), "55").await;
    observer.expect_payload(&state_topic(SETPOINT), "55").await;
    assert_eq!(bridge.fake.sets(), [Call::Set(SETPOINT.into(), Value::Int(55))]);

    // Invalid and not allowlisted writes are dropped; the Bridge keeps running.
    observer.publish(&set_topic(SETPOINT), "96").await;
    observer.publish(&set_topic(SETPOINT), "NaN").await;
    observer.publish(&set_topic(TEMPERATURE), "20").await;
    observer.publish(&set_topic(SETPOINT), "56").await;
    observer.expect_payload(&state_topic(SETPOINT), "56").await;
    assert_eq!(bridge.fake.sets().len(), 2);
  })
  .await
}

#[tokio::test]
async fn retained_set_is_ignored_on_connect() {
  local(async {
    let broker = Broker::start("allow_anonymous true").await;
    let observer = Observer::connect(broker.observer_options(), &[]).await;
    observer.client.publish(set_topic(SETPOINT), QoS::AtLeastOnce, true, "60").await.unwrap();
    sleep(Duration::from_millis(300)).await;

    let bridge = start_bridge(broker.default_config(), None, fake_with_values());
    let mut observer = Observer::connect(broker.observer_options(), &["vcontrol/#"]).await;
    // Once it is online, the Bridge has subscribed and received the retained message.
    observer.expect_payload(STATUS, "online").await;
    observer.publish(&set_topic(SETPOINT), "55").await;
    observer.expect_payload(&state_topic(SETPOINT), "55").await;
    assert_eq!(bridge.fake.sets(), [Call::Set(SETPOINT.into(), Value::Int(55))]);
  })
  .await
}

#[tokio::test]
async fn reconnects_resubscribes_and_announces_again() {
  local(async {
    let mut broker = Broker::start("allow_anonymous true").await;
    let bridge = start_bridge(broker.default_config(), None, fake_with_values());
    Observer::connect(broker.observer_options(), &["#"]).await.expect_payload(&state_topic(SETPOINT), "50").await;

    broker.restart().await;

    // The new broker knows nothing: status, Discovery and subscriptions must come from the Bridge again.
    let mut observer = Observer::connect(broker.observer_options(), &["#"]).await;
    observer.expect_payload(STATUS, "online").await;
    observer.expect(&format!("homeassistant/number/vcontrol/{SETPOINT}/config"), |_| true).await;
    observer.expect_payload(&state_topic(TEMPERATURE), "21.5").await;

    // Depending on timing the Bridge may still be subscribing; a QoS 1 write sent after `online` must arrive.
    wait_for("the write after the reconnect", || {
      let _ = observer.client.try_publish(set_topic(SETPOINT), QoS::AtLeastOnce, false, "55");
      !bridge.fake.sets().is_empty()
    })
    .await;
    assert!(!bridge.task.is_finished());
  })
  .await
}

#[tokio::test]
async fn last_will_sets_offline_when_the_bridge_dies() {
  local(async {
    let broker = Broker::start("allow_anonymous true").await;
    let bridge = start_bridge(broker.default_config(), None, fake_with_values());
    let mut observer = Observer::connect(broker.observer_options(), &[STATUS]).await;
    observer.expect_payload(STATUS, "online").await;

    // Dies without a goodbye, like a lost Optolink link ending the process.
    bridge.task.abort();
    observer.expect_payload(STATUS, "offline").await;
    let log = || fs::read_to_string(broker.dir.join("log")).unwrap();
    wait_for("the broker to log the disconnect", || log().contains("Client vcontrol")).await;
    assert!(log().contains("Client vcontrol closed its connection."), "{}", log());

    let mut late = Observer::connect(broker.observer_options(), &[STATUS]).await;
    assert!(late.expect_payload(STATUS, "offline").await.retain);
  })
  .await
}

#[tokio::test]
async fn link_loss_ends_the_bridge_and_the_last_will_sets_offline() {
  local(async {
    let broker = Broker::start("allow_anonymous true").await;
    let bridge = start_bridge(broker.default_config(), None, fake_with_values());
    let mut observer = Observer::connect(broker.observer_options(), &[STATUS]).await;
    observer.expect_payload(STATUS, "online").await;

    bridge.fake.state().failing.extend([TEMPERATURE.to_owned(), SETPOINT.to_owned()]);
    assert_eq!(bridge.task.await.unwrap(), Exit::LinkLost);
    observer.expect_payload(STATUS, "offline").await;
  })
  .await
}

#[tokio::test]
async fn shutdown_publishes_offline() {
  local(async {
    let broker = Broker::start("allow_anonymous true").await;
    let bridge = start_bridge(broker.default_config(), None, fake_with_values());
    let mut observer = Observer::connect(broker.observer_options(), &[STATUS]).await;
    observer.expect_payload(STATUS, "online").await;

    let start = Instant::now();
    bridge.shutdown.send(()).unwrap();
    assert_eq!(bridge.task.await.unwrap(), Exit::Shutdown);
    // Sent, not given up on after the timeout.
    assert!(start.elapsed() < SHUTDOWN_TIMEOUT / 2, "{:?}", start.elapsed());
    observer.expect_payload(STATUS, "offline").await;

    // The broker saw a clean disconnect, so it dropped the Last Will: `offline` was published by the Bridge itself.
    let log = || fs::read_to_string(broker.dir.join("log")).unwrap();
    wait_for("the broker to log the disconnect", || log().contains("Client vcontrol")).await;
    assert!(log().contains("Client vcontrol disconnected.\n"), "{}", log());
  })
  .await
}

#[tokio::test]
async fn home_assistant_restart_gets_discovery_again() {
  local(async {
    let broker = Broker::start("allow_anonymous true").await;
    let _bridge = start_bridge(broker.default_config(), None, fake_with_values());
    let config_topic = format!("homeassistant/number/vcontrol/{SETPOINT}/config");
    let mut observer = Observer::connect(broker.observer_options(), &["homeassistant/#", STATUS]).await;
    observer.expect_payload(STATUS, "online").await;
    observer.expect(&config_topic, |_| true).await;
    sleep(Duration::from_millis(300)).await;
    while observer.messages.try_recv().is_ok() {}

    observer.publish("homeassistant/status", "online").await;
    // Not the retained copy: a fresh publish.
    observer.expect(&config_topic, |message| !message.retain).await;
  })
  .await
}

/// Reads are always overdue here, so every pass of the loop has a read to do. No write may get lost or reordered,
/// and the MQTT connection must survive.
#[tokio::test]
async fn nothing_is_lost_while_reads_are_always_due() {
  local(async {
    let broker = Broker::start("allow_anonymous true").await;

    let mut names: Vec<&str> = device()
      .commands()
      .entries()
      .filter(|(name, command)| {
        payload::kind(command) != Kind::Complex && command.access_mode().is_read() && topics::is_valid_id(name)
      })
      .map(|(name, _)| *name)
      .filter(|name| *name != SETPOINT)
      .collect();
    names.sort_unstable();
    names.truncate(39);
    names.push(SETPOINT);
    let config = broker.config(&format!("writable = [\"{SETPOINT}\"]"), "", &names);

    // 40 Commands × 30 ms > the 1 s interval.
    let fake = fake_with_values();
    fake.state().read_time = Duration::from_millis(30);
    let bridge = start_bridge(config, None, fake);

    let mut observer = Observer::connect(broker.observer_options(), &[STATUS, &state_topic(SETPOINT)]).await;
    observer.expect_payload(STATUS, "online").await;

    let values: Vec<i64> = (0..50).map(|i| 10 + i).collect();
    for value in &values {
      observer.publish(&set_topic(SETPOINT), &value.to_string()).await;
    }

    wait_for("all writes", || bridge.fake.sets().len() >= values.len()).await;
    let expected: Vec<Call> = values.iter().map(|value| Call::Set(SETPOINT.into(), Value::Int(*value))).collect();
    assert_eq!(bridge.fake.sets(), expected);

    // Still connected and publishing: no `offline` in between, and the last value arrives.
    observer.expect_payload(&state_topic(SETPOINT), "59").await;
    assert!(!bridge.task.is_finished());
    while let Ok(message) = observer.messages.try_recv() {
      assert!(message.payload != "offline");
    }
  })
  .await
}

#[tokio::test]
async fn tls_with_username_and_password() {
  local(async {
    let mut broker = Broker::prepare(
      "allow_anonymous false\npassword_file {dir}/passwords\ncertfile {dir}/server.pem\nkeyfile {dir}/server.key",
    );
    broker.tool("openssl", &["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", "ca.key", "-out", "ca.pem", "-subj", "/CN=test-ca", "-days", "1"]);
    broker.tool("openssl", &["req", "-newkey", "rsa:2048", "-nodes", "-keyout", "server.key", "-out", "server.csr", "-subj", "/CN=broker"]);
    fs::write(broker.dir.join("ext"), "subjectAltName=IP:127.0.0.1\n").unwrap();
    broker.tool("openssl", &["x509", "-req", "-in", "server.csr", "-CA", "ca.pem", "-CAkey", "ca.key", "-CAcreateserial", "-out", "server.pem", "-days", "1", "-extfile", "ext"]);
    broker.tool("mosquitto_passwd", &["-b", "-c", "passwords", "heating", "secret"]);
    broker.restart().await;

    let ca = fs::read(broker.dir.join("ca.pem")).unwrap();
    let mqtt = |client_id: &str, password: &str| {
      format!(
        "client_id = \"{client_id}\"\ntopic_prefix = \"{client_id}\"\nusername = \"heating\"\npassword = \"{password}\"\ntls = true\nca_file = \"{}\"",
        broker.dir.join("ca.pem").display()
      )
    };

    let _bridge = start_bridge(broker.config("", &mqtt("vcontrol", "secret"), &[TEMPERATURE]), Some(ca.clone()), fake_with_values());
    let observer_options = mqtt_options(&broker.config("", &mqtt("observer", "secret"), &[TEMPERATURE]), Some(ca.clone()));
    let mut observer = Observer::connect(observer_options, &["vcontrol/#"]).await;
    observer.expect_payload(STATUS, "online").await;
    observer.expect_payload(&state_topic(TEMPERATURE), "21.5").await;

    // A wrong password, an untrusted CA and a plain connection are all refused.
    let (_, mut wrong_password) =
      AsyncClient::new(mqtt_options(&broker.config("", &mqtt("intruder", "wrong"), &[TEMPERATURE]), Some(ca)), 10);
    assert!(wrong_password.poll().await.is_err());

    let other_ca = fs::read(broker.dir.join("server.pem")).unwrap();
    let (_, mut wrong_ca) =
      AsyncClient::new(mqtt_options(&broker.config("", &mqtt("intruder", "secret"), &[TEMPERATURE]), Some(other_ca)), 10);
    assert!(wrong_ca.poll().await.is_err());

    let mut plain_options = MqttOptions::new("plain", "127.0.0.1", broker.port);
    plain_options.set_credentials("heating", "secret");
    let (_, mut plain) = AsyncClient::new(plain_options, 10);
    assert!(timeout(Duration::from_secs(10), plain.poll()).await.map_or(true, |result| result.is_err()));
  })
  .await
}
