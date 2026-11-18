//! The real `vcontrol-mqtt` binary end to end: a fake controller speaking VS1 or VS2 over TCP on one side,
//! mosquitto on the other. Runs in the devcontainer.

use std::{
  fs,
  path::Path,
  process::{Child, Command, Stdio},
  sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
  },
  time::Duration,
};

use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use tokio::{
  io::{AsyncReadExt, AsyncWriteExt},
  net::{TcpListener, TcpStream},
  sync::mpsc,
  time::{sleep, timeout},
};

const SETPOINT: &str = "Bedien_WW_Solltemperatur";
const SETPOINT_ADDR: u16 = 0x6300;
const TEMPERATURE: &str = "ADC_Isttemperaturwert_KTS";

#[derive(Default)]
struct FakeController {
  writes: Mutex<Vec<(u16, Vec<u8>)>>,
  /// Stops answering, like an unplugged Optolink adapter.
  hang: AtomicBool,
  /// Speaks VS2 (300) instead of only VS1 (KW).
  vs2: bool,
  /// A VS2 telegram was handled.
  used_vs2: AtomicBool,
}

/// A VS2 response telegram.
fn telegram(function: u8, addr: [u8; 2], payload_len: u8, payload: &[u8]) -> Vec<u8> {
  let mut telegram = vec![0x41, 5 + payload.len() as u8, 0x01, function, addr[0], addr[1], payload_len];
  telegram.extend_from_slice(payload);
  telegram.push(telegram[1..].iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)));
  telegram
}

impl FakeController {
  fn read(&self, addr: u16, len: usize) -> Vec<u8> {
    let stored = self.writes.lock().unwrap().iter().rev().find(|(a, _)| *a == addr).map(|(_, data)| data.clone());
    let mut data = stored.unwrap_or_else(|| match addr {
      0x00F8 => vec![0x20, 0x9F, 0, 0, 0, 0, 0, 0], // V200KO2B
      0x0802 => 215u16.to_le_bytes().to_vec(),       // 21.5 °C
      SETPOINT_ADDR => vec![50],
      _ => vec![],
    });
    data.resize(len, 0);
    data
  }

  async fn serve(&self, mut stream: TcpStream) {
    let mut buf = vec![];
    let mut chunk = [0; 64];
    // After a VS2 start, until the next reset.
    let mut in_vs2 = false;
    loop {
      match timeout(Duration::from_millis(100), stream.read(&mut chunk)).await {
        // Idle: the controller sends its sync byte.
        Err(_) => {
          let silent = !buf.is_empty() || in_vs2 || self.hang.load(Ordering::Relaxed);
          if !silent && stream.write_all(&[0x05]).await.is_err() {
            return;
          }
          continue;
        },
        Ok(Ok(0)) | Ok(Err(_)) => return,
        Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
      }
      if self.hang.load(Ordering::Relaxed) {
        buf.clear();
        continue;
      }

      loop {
        let reply = match buf[..] {
          [] => break,
          // VS2 start. If it is not supported, the protocol detection falls back to VS1.
          [0x16, _, _, ..] => {
            buf.drain(..3);
            in_vs2 = self.vs2;
            vec![if self.vs2 { 0x06 } else { 0xff }]
          },
          [0x16, ..] => break,
          // VS2 telegram: lead-in, length, type, function, address, payload length, payload, checksum.
          [0x41, len, ..] if in_vs2 && buf.len() >= len as usize + 3 => {
            let frame: Vec<u8> = buf.drain(..len as usize + 3).collect();
            self.used_vs2.store(true, Ordering::Relaxed);
            let (function, addr, payload_len) = (frame[3], [frame[4], frame[5]], frame[6]);
            let mut reply = vec![0x06];
            if function == 1 {
              let data = self.read(u16::from_be_bytes(addr), payload_len as usize);
              reply.extend(telegram(function, addr, payload_len, &data));
            } else {
              let data = frame[7..7 + payload_len as usize].to_vec();
              self.writes.lock().unwrap().push((u16::from_be_bytes(addr), data));
              reply.extend(telegram(function, addr, payload_len, &[]));
            }
            reply
          },
          [0x41, ..] if in_vs2 => break,
          [0x04, ..] => {
            buf.remove(0);
            in_vs2 = false;
            vec![]
          },
          [0x01, 0xF7, hi, lo, len, ..] => {
            buf.drain(..5);
            self.read(u16::from_be_bytes([hi, lo]), len as usize)
          },
          [0x01, 0xF4, hi, lo, len, ..] if buf.len() >= 5 + len as usize => {
            let data = buf[5..5 + len as usize].to_vec();
            buf.drain(..5 + len as usize);
            self.writes.lock().unwrap().push((u16::from_be_bytes([hi, lo]), data));
            vec![0x00]
          },
          [0x01, ..] => break,
          // Acknowledgements and anything else.
          _ => {
            buf.remove(0);
            vec![]
          },
        };
        if stream.write_all(&reply).await.is_err() {
          return;
        }
      }
    }
  }
}

struct Killed(Child);

impl Drop for Killed {
  fn drop(&mut self) {
    let _ = self.0.kill();
    let _ = self.0.wait();
  }
}

fn bridge(config: &Path, read_only: bool) -> Killed {
  let mut command = Command::new(env!("CARGO_BIN_EXE_vcontrol-mqtt"));
  command.arg("--config").arg(config).stdout(Stdio::null()).stderr(Stdio::null());
  if read_only {
    command.arg("--read-only");
  }
  Killed(command.spawn().unwrap())
}

/// Waits up to 60 s: a controller that stops answering takes three failed calls (5 + 20 + 20 s) to end the bridge.
async fn exit_code(child: &mut Killed) -> i32 {
  for _ in 0..1200 {
    if let Some(status) = child.0.try_wait().unwrap() {
      return status.code().expect("ended by its own exit");
    }
    sleep(Duration::from_millis(50)).await;
  }
  panic!("vcontrol-mqtt did not exit");
}

async fn expect(messages: &mut mpsc::UnboundedReceiver<(String, String)>, topic: &str, payload: &str) {
  let wait = async {
    loop {
      let (t, p) = messages.recv().await.unwrap();
      if t == topic && p == payload {
        return;
      }
    }
  };
  timeout(Duration::from_secs(30), wait).await.unwrap_or_else(|_| panic!("no {payload:?} on {topic}"));
}

#[tokio::test]
async fn real_binary_end_to_end_vs1() {
  end_to_end(false).await
}

#[tokio::test]
async fn real_binary_end_to_end_vs2() {
  end_to_end(true).await
}

async fn end_to_end(vs2: bool) {
  let port = 18000 + vs2 as u16 * 1000 + (std::process::id() % 1000) as u16;
  let dir = std::env::temp_dir().join(format!("vcontrol-mqtt-e2e-{port}"));
  fs::create_dir_all(&dir).unwrap();

  fs::write(dir.join("mosquitto.conf"), format!("listener {port} 127.0.0.1\nallow_anonymous true\nuser root\n")).unwrap();
  let _broker = Killed(
    Command::new("mosquitto")
      .arg("-c")
      .arg(dir.join("mosquitto.conf"))
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .spawn()
      .expect("mosquitto is not installed; run the tests in the devcontainer"),
  );

  let controller = Arc::new(FakeController { vs2, ..Default::default() });
  let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
  let optolink_port = listener.local_addr().unwrap().port();
  tokio::spawn({
    let controller = controller.clone();
    async move {
      loop {
        let (stream, _) = listener.accept().await.unwrap();
        controller.serve(stream).await;
      }
    }
  });

  let config = dir.join("vcontrol-mqtt.toml");
  fs::write(
    &config,
    format!(
      "poll_interval = 1\nwritable = [\"{SETPOINT}\"]\n[optolink]\nhost = \"127.0.0.1\"\nport = {optolink_port}\n\
       [mqtt]\nhost = \"127.0.0.1\"\nport = {port}\n[[command]]\nname = \"{TEMPERATURE}\"\n[[command]]\nname = \"{SETPOINT}\"\n"
    ),
  )
  .unwrap();

  let (client, mut event_loop) = AsyncClient::new(MqttOptions::new("observer", "127.0.0.1", port), 100);
  client.subscribe("#", QoS::AtLeastOnce).await.unwrap();
  let (sender, mut messages) = mpsc::unbounded_channel();
  tokio::spawn(async move {
    loop {
      match event_loop.poll().await {
        Ok(Event::Incoming(Packet::Publish(message))) => {
          let _ = sender.send((message.topic.clone(), String::from_utf8_lossy(&message.payload).into_owned()));
        },
        Ok(_) => (),
        Err(_) => sleep(Duration::from_millis(100)).await,
      }
    }
  });
  let set_topic = format!("vcontrol/{SETPOINT}/set");

  // A wrong configuration: exit code 2, before anything is connected.
  let bad = dir.join("bad.toml");
  fs::write(&bad, fs::read_to_string(&config).unwrap().replace(TEMPERATURE, "Nope")).unwrap();
  assert_eq!(exit_code(&mut bridge(&bad, false)).await, 2);
  assert!(controller.writes.lock().unwrap().is_empty());

  // Live: values, a write with read-back, then SIGTERM.
  let mut live = bridge(&config, false);
  expect(&mut messages, "vcontrol/status", "online").await;
  expect(&mut messages, &format!("vcontrol/{TEMPERATURE}"), "21.5").await;
  expect(&mut messages, &format!("vcontrol/{SETPOINT}"), "50").await;
  client.publish(&set_topic, QoS::AtLeastOnce, false, "96").await.unwrap();
  client.publish(&set_topic, QoS::AtLeastOnce, false, "55").await.unwrap();
  expect(&mut messages, &format!("vcontrol/{SETPOINT}"), "55").await;
  assert_eq!(*controller.writes.lock().unwrap(), [(SETPOINT_ADDR, vec![55])]);

  assert!(Command::new("kill").arg("-TERM").arg(live.0.id().to_string()).status().unwrap().success());
  assert_eq!(exit_code(&mut live).await, 0);
  expect(&mut messages, "vcontrol/status", "offline").await;

  // Read-only: a valid write is not sent. Then the controller stops answering: exit code 1 and the Last Will.
  let mut read_only = bridge(&config, true);
  expect(&mut messages, "vcontrol/status", "online").await;
  client.publish(&set_topic, QoS::AtLeastOnce, false, "60").await.unwrap();
  sleep(Duration::from_secs(2)).await;
  assert_eq!(controller.writes.lock().unwrap().len(), 1);

  controller.hang.store(true, Ordering::Relaxed);
  assert_eq!(exit_code(&mut read_only).await, 1);
  expect(&mut messages, "vcontrol/status", "offline").await;

  // The protocol under test was really the one in use.
  assert_eq!(controller.used_vs2.load(Ordering::Relaxed), vs2);

  let _ = fs::remove_dir_all(&dir);
}
