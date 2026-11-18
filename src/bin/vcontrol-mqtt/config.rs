//! Parses the TOML and checks it against the detected Device. No I/O.

use std::{collections::HashSet, path::PathBuf, time::Duration};

use serde::Deserialize;
use vcontrol::{Command, Device};

use crate::{
  payload::{self, Kind},
  topics,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
  /// Seconds.
  pub poll_interval: u64,
  #[serde(default)]
  pub writable: Vec<String>,
  pub optolink: OptolinkConfig,
  pub mqtt: MqttConfig,
  #[serde(default)]
  pub homeassistant: HomeAssistantConfig,
  #[serde(default, rename = "command")]
  pub commands: Vec<CommandConfig>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(untagged, deny_unknown_fields)]
pub enum OptolinkConfig {
  Device {
    device: String,
  },
  Tcp {
    #[serde(default = "localhost")]
    host: String,
    port: u16,
  },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MqttConfig {
  pub host: String,
  #[serde(default = "default_port")]
  pub port: u16,
  #[serde(default = "default_name")]
  pub client_id: String,
  pub username: Option<String>,
  pub password: Option<String>,
  #[serde(default)]
  pub tls: bool,
  pub ca_file: Option<PathBuf>,
  #[serde(default = "default_name")]
  pub topic_prefix: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HomeAssistantConfig {
  pub discovery_prefix: String,
}

impl Default for HomeAssistantConfig {
  fn default() -> Self {
    Self { discovery_prefix: "homeassistant".into() }
  }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandConfig {
  pub name: String,
  /// The entity name in Home Assistant. Defaults to `name`.
  pub display_name: Option<String>,
  /// Seconds.
  pub interval: Option<u64>,
}

fn localhost() -> String {
  "localhost".into()
}

fn default_port() -> u16 {
  1883
}

fn default_name() -> String {
  "vcontrol".into()
}

/// Seconds. The upper end keeps "now + interval" from overflowing.
const INTERVALS: std::ops::RangeInclusive<u64> = 1..=365 * 24 * 3600;

/// A Polled Command.
#[derive(Debug)]
pub struct Polled {
  pub name: String,
  pub display_name: String,
  pub command: &'static Command,
  pub interval: Duration,
  pub writable: bool,
}

/// A config that passed every startup check. Can only be created by [`Config::check`].
#[derive(Debug)]
pub struct Checked {
  mqtt: MqttConfig,
  discovery_prefix: String,
  commands: Vec<Polled>,
}

impl Checked {
  pub fn mqtt(&self) -> &MqttConfig {
    &self.mqtt
  }

  pub fn discovery_prefix(&self) -> &str {
    &self.discovery_prefix
  }

  pub fn commands(&self) -> &[Polled] {
    &self.commands
  }
}

/// Why a Command cannot be writable, if it cannot.
fn writable_problem(command: &Command) -> Option<String> {
  if !command.access_mode().is_write() {
    return Some("the controller does not allow writing it".into());
  }

  let samples: Vec<f64> = match payload::kind(command) {
    Kind::Number => match (command.lower_bound(), command.upper_bound()) {
      (Some(lower), Some(upper)) => vec![lower, upper],
      // Without bounds, out-of-range values would wrap around silently in `Command::set`.
      _ => return Some("it has no lower and upper bound".into()),
    },
    Kind::Mapped => command.mapping().expect("mapped").keys().map(|key| *key as f64).collect(),
    Kind::PartlyMapped => return Some("its Mapping does not cover all values".into()),
    kind => return Some(format!("{kind:?} Commands cannot be written")),
  };

  // What is written must cover the whole Command and read back as the same value.
  for sample in samples {
    let value = payload::number_value(command, sample);
    match command.serialize(value.clone()) {
      Ok(bytes) if bytes.len() != command.block_len() => {
        return Some("it is only a part of a larger block".into());
      },
      Ok(bytes) if command.deserialize(&bytes).ok().as_ref() != Some(&value) => {
        return Some(format!("{sample} does not survive being written and read back"));
      },
      Ok(bytes) if !payload::owns_all_bits(command, &bytes) => {
        return Some("it is a bit field that shares its byte with other Commands".into());
      },
      Ok(_) => (),
      Err(err) => return Some(format!("its value conversion cannot be reversed ({err})")),
    }
  }

  if payload::kind(command) == Kind::Number && payload::step(command).is_none() {
    return Some("the step between its values cannot be determined".into());
  }

  None
}

impl Config {
  /// `password_override` is the `VCONTROL_MQTT_PASSWORD` environment variable.
  pub fn parse(toml: &str, password_override: Option<String>) -> Result<Self, String> {
    let mut config: Self = toml::from_str(toml).map_err(|err| err.to_string())?;
    if password_override.is_some() {
      config.mqtt.password = password_override;
    }
    Ok(config)
  }

  /// Checks the config against the detected Device. Returns every problem found.
  pub fn check(self, device: &'static Device) -> Result<Checked, Vec<String>> {
    let mut problems = vec![];

    if !INTERVALS.contains(&self.poll_interval) {
      problems.push("poll_interval must be between 1 second and 1 year".to_owned());
    }
    if self.mqtt.tls && self.mqtt.ca_file.is_none() {
      problems.push("mqtt.tls = true requires mqtt.ca_file".to_owned());
    }
    if self.mqtt.password.is_some() && self.mqtt.username.is_none() {
      problems.push("mqtt.password requires mqtt.username".to_owned());
    }
    if !topics::is_valid_id(&self.mqtt.client_id) {
      problems.push(format!("mqtt.client_id {:?} must match [a-zA-Z0-9_-]+", self.mqtt.client_id));
    }
    for (key, prefix) in [
      ("mqtt.topic_prefix", &self.mqtt.topic_prefix),
      ("homeassistant.discovery_prefix", &self.homeassistant.discovery_prefix),
    ] {
      if prefix.is_empty() || prefix.contains(['+', '#']) || prefix.starts_with('/') || prefix.ends_with('/') {
        problems.push(format!("{key} {prefix:?} is not a valid topic prefix"));
      }
    }
    if self.mqtt.topic_prefix == self.homeassistant.discovery_prefix {
      // Both would use `<prefix>/status`: the Bridge's Availability and Home Assistant's birth message.
      problems.push("mqtt.topic_prefix and homeassistant.discovery_prefix must differ".to_owned());
    }
    if self.commands.is_empty() {
      problems.push("no [[command]] is configured".to_owned());
    }

    let mut names = HashSet::new();
    let mut object_ids = HashSet::new();
    let mut commands = vec![];
    for config in &self.commands {
      let name = &config.name;

      if !names.insert(name.as_str()) {
        problems.push(format!("command {name:?} is configured twice"));
        continue;
      }
      // `VControl` resolves system Commands before device Commands, so it would read and write a different one.
      if vcontrol::commands::system_command(name).is_some() {
        problems.push(format!("command {name:?} is a system command"));
        continue;
      }
      let Some(command) = device.command(name) else {
        problems.push(format!("command {name:?} is unknown on device {}", device.name()));
        continue;
      };
      if name == "status" || name.contains(['+', '#', '/']) || !object_ids.insert(topics::object_id(name)) {
        problems.push(format!("command {name:?} cannot be used as a topic or discovery id"));
      }
      if payload::kind(command) == Kind::Complex {
        problems.push(format!("command {name:?} has a complex type and cannot be polled"));
      }
      if !command.access_mode().is_read() {
        problems.push(format!("command {name:?} cannot be read"));
      }
      if config.display_name.as_ref().is_some_and(|display_name| display_name.trim().is_empty()) {
        problems.push(format!("command {name:?}: display_name must not be empty"));
      }
      if config.interval.is_some_and(|interval| !INTERVALS.contains(&interval)) {
        problems.push(format!("command {name:?}: interval must be between 1 second and 1 year"));
      }

      let writable = self.writable.contains(name);
      if writable && let Some(problem) = writable_problem(command) {
        problems.push(format!("command {name:?} cannot be writable: {problem}"));
      }

      commands.push(Polled {
        name: name.clone(),
        display_name: config.display_name.clone().unwrap_or_else(|| name.clone()),
        command,
        interval: Duration::from_secs(config.interval.unwrap_or(self.poll_interval)),
        writable,
      });
    }

    for name in &self.writable {
      if !names.contains(name.as_str()) {
        problems.push(format!("writable command {name:?} is not also a [[command]]"));
      }
    }

    if problems.is_empty() {
      Ok(Checked { mqtt: self.mqtt, discovery_prefix: self.homeassistant.discovery_prefix, commands })
    } else {
      Err(problems)
    }
  }
}

#[cfg(test)]
pub mod tests {
  use vcontrol::types::DeviceId;

  use super::*;

  /// The real V200KO2B.
  pub fn device() -> &'static Device {
    let device = Device::detect(DeviceId::from_bytes(&[0x20, 0x9F, 0, 0, 0, 0, 0, 0]), None).unwrap();
    assert_eq!(device.name(), "V200KO2B");
    device
  }

  pub fn command(name: &str) -> &'static Command {
    device().command(name).unwrap_or_else(|| panic!("{name}"))
  }

  /// A checked config with the given top-level lines and Commands.
  pub fn checked(top: &str, commands: &[&str]) -> Checked {
    Config::parse(&toml(top, commands), None).unwrap().check(device()).unwrap()
  }

  fn toml(top: &str, commands: &[&str]) -> String {
    let commands: String = commands.iter().map(|name| format!("[[command]]\nname = \"{name}\"\n")).collect();
    format!(
      "poll_interval = 60\n{top}\n[optolink]\ndevice = \"/dev/ttyUSB0\"\n[mqtt]\nhost = \"localhost\"\n{commands}"
    )
  }

  fn problems(toml: &str) -> Vec<String> {
    Config::parse(toml, None).unwrap().check(device()).unwrap_err()
  }

  /// The single problem of an otherwise valid config.
  fn problem(top: &str, commands: &[&str]) -> String {
    let problems = problems(&toml(top, commands));
    assert_eq!(problems.len(), 1, "{problems:?}");
    problems.into_iter().next().unwrap()
  }

  const FULL: &str = r#"
    poll_interval = 60
    writable = ["Bedien_WW_Solltemperatur"]

    [optolink]
    device = "/dev/ttyUSB0"

    [mqtt]
    host = "broker.local"
    port = 8883
    client_id = "heating"
    username = "vcontrol"
    password = "secret"
    tls = true
    ca_file = "/etc/vcontrol/ca.pem"
    topic_prefix = "heating"

    [homeassistant]
    discovery_prefix = "ha"

    [[command]]
    name = "Bedien_WW_Solltemperatur"

    [[command]]
    name = "ADC_Isttemperaturwert_2"
    display_name = "Vorlauftemperatur"
    interval = 3600
  "#;

  #[test]
  fn full_config() {
    let config = Config::parse(FULL, None).unwrap();
    assert_eq!(config.optolink, OptolinkConfig::Device { device: "/dev/ttyUSB0".into() });

    let checked = config.check(device()).unwrap();
    assert_eq!(checked.discovery_prefix(), "ha");
    let mqtt = checked.mqtt();
    assert_eq!((mqtt.host.as_str(), mqtt.port, mqtt.client_id.as_str()), ("broker.local", 8883, "heating"));
    assert_eq!((mqtt.username.as_deref(), mqtt.password.as_deref()), (Some("vcontrol"), Some("secret")));
    assert_eq!((mqtt.tls, mqtt.topic_prefix.as_str()), (true, "heating"));
    assert_eq!(mqtt.ca_file.as_deref(), Some(std::path::Path::new("/etc/vcontrol/ca.pem")));

    let [setpoint, temperature] = checked.commands() else { panic!() };
    assert_eq!(
      (setpoint.name.as_str(), setpoint.interval, setpoint.writable),
      ("Bedien_WW_Solltemperatur", Duration::from_secs(60), true)
    );
    assert!(std::ptr::eq(setpoint.command, command("Bedien_WW_Solltemperatur")));
    assert_eq!((temperature.interval, temperature.writable), (Duration::from_secs(3600), false));
    assert_eq!(
      (setpoint.display_name.as_str(), temperature.display_name.as_str()),
      ("Bedien_WW_Solltemperatur", "Vorlauftemperatur")
    );
  }

  #[test]
  fn example_config_is_valid() {
    let example = include_str!("../../../dist/config/vcontrol-mqtt.example.toml");
    let checked = Config::parse(example, None).unwrap().check(device()).unwrap();
    assert_eq!(checked.commands().iter().filter(|polled| polled.writable).count(), 1);
  }

  /// The config the .deb installs as /etc/vcontrol/vcontrol-mqtt.toml.
  #[test]
  fn default_config_is_valid() {
    let default = include_str!("../../../dist/config/vitotronic-200-ko2b.toml");
    let checked = Config::parse(default, None).unwrap().check(device()).unwrap();
    assert!(checked.commands().iter().all(|polled| !polled.writable));
  }

  #[test]
  fn defaults_and_tcp_optolink() {
    let config = Config::parse(
      "poll_interval = 5\n[optolink]\nport = 3002\n[mqtt]\nhost = \"h\"\n[[command]]\nname = \"ADC_Isttemperaturwert_2\"",
      None,
    )
    .unwrap();
    assert_eq!(config.optolink, OptolinkConfig::Tcp { host: "localhost".into(), port: 3002 });

    let checked = config.check(device()).unwrap();
    assert_eq!(checked.discovery_prefix(), "homeassistant");
    let mqtt = checked.mqtt();
    assert_eq!(
      (mqtt.port, mqtt.client_id.as_str(), mqtt.topic_prefix.as_str(), mqtt.tls),
      (1883, "vcontrol", "vcontrol", false)
    );
    assert_eq!((&mqtt.username, &mqtt.password), (&None, &None));
  }

  #[test]
  fn environment_password_wins() {
    let config = Config::parse(FULL, Some("from-env".into())).unwrap();
    assert_eq!(config.mqtt.password.as_deref(), Some("from-env"));
    assert_eq!(Config::parse(FULL, None).unwrap().mqtt.password.as_deref(), Some("secret"));
  }

  #[test]
  fn parse_errors() {
    assert!(Config::parse("poll_interval = 60", None).is_err());
    assert!(Config::parse(&toml("", &[]).replace("poll_interval = 60", "poll_interval = 1.5"), None).is_err());
    assert!(Config::parse(&toml("typo = 1", &[]), None).is_err());
    assert!(Config::parse(&toml("", &[]).replace("device = \"/dev/ttyUSB0\"", ""), None).is_err());
    assert!(Config::parse(&toml("", &[]).replace("[mqtt]", "port = 1\n[mqtt]"), None).is_err());
    assert!(Config::parse(&format!("{}interval = -1", toml("", &["ADC_Isttemperaturwert_2"])), None).is_err());
  }

  #[test]
  fn refuses_empty_display_name() {
    let config = toml("", &["ADC_Isttemperaturwert_2"]) + "display_name = \" \"";
    assert!(problems(&config)[0].contains("display_name must not be empty"));
  }

  #[test]
  fn refuses_unknown_command() {
    assert!(problem("", &["Nope"]).contains("unknown on device V200KO2B"));
  }

  #[test]
  fn refuses_system_command_names() {
    for name in ["device_id", "device_id_f0"] {
      assert!(problem(&format!("writable = [\"{name}\"]"), &[name]).contains("is a system command"), "{name}");
    }
  }

  #[test]
  fn refuses_complex_types() {
    for name in ["Uhrzeit", "Schaltzeiten_A1M1_HK", "ecnsysEventType~Error"] {
      assert!(problem("", &[name]).contains("complex type"), "{name}");
    }
  }

  #[test]
  fn refuses_write_only_command() {
    assert!(
      problems(&toml("", &["BetriebsstundenBrenner1BedienungFunktionReset"]))
        .iter()
        .any(|p| p.contains("cannot be read"))
    );
  }

  #[test]
  fn refuses_writable_that_is_not_polled() {
    assert!(
      problem("writable = [\"Bedien_WW_Solltemperatur\"]", &["ADC_Isttemperaturwert_2"])
        .contains("not also a [[command]]")
    );
  }

  #[test]
  fn refuses_writable_without_write_access() {
    assert!(
      problem("writable = [\"ADC_Isttemperaturwert_2\"]", &["ADC_Isttemperaturwert_2"])
        .contains("does not allow writing")
    );
  }

  #[test]
  fn refuses_writable_number_without_bounds() {
    // K03 on V200KO2B is read-write, but unmapped and unbounded.
    assert!(
      problem("writable = [\"K03_KonfiOelGasBetrieb\"]", &["K03_KonfiOelGasBetrieb"])
        .contains("no lower and upper bound")
    );
    checked("", &["K03_KonfiOelGasBetrieb"]);
  }

  #[test]
  fn refuses_writable_with_irreversible_conversion() {
    // Seconds to hours cannot be converted back.
    let name = "BetriebsstundenBrenner2Bedienung";
    assert!(problem(&format!("writable = [\"{name}\"]"), &[name]).contains("cannot be reversed"));
    checked("", &[name]);
  }

  #[test]
  fn refuses_writable_text() {
    assert!(writable_problem(command("Bedienteil_SW_Index")).is_some());
  }

  #[test]
  fn refuses_writable_bit_field() {
    // Writing this flag would overwrite the other flags in its byte.
    let flag = vcontrol::device::ECOTRONIC.command("AutomaticFlag_WWM2").unwrap();
    assert!(writable_problem(flag).unwrap().contains("bit field"));
  }

  #[test]
  fn refuses_equal_prefixes() {
    let toml = toml("", &["ADC_Isttemperaturwert_2"]).replace("[mqtt]", "[mqtt]\ntopic_prefix = \"homeassistant\"");
    assert!(problems(&toml)[0].contains("must differ"));
  }

  #[test]
  fn accepts_writable_numbers_and_mapped() {
    let names = [
      "Bedien_WW_Solltemperatur",
      "K0E_KonfiTSAReglerverstaerkung",
      "KA2_KonfiSpeichervorrangA1M1",
      "AGTemperaturwertMAXBedien",
    ];
    let writable = format!("writable = {names:?}");
    assert!(checked(&writable, &names).commands().iter().all(|polled| polled.writable));
  }

  #[test]
  fn refuses_tls_without_ca_file() {
    let toml = toml("", &["ADC_Isttemperaturwert_2"]).replace("[mqtt]", "[mqtt]\ntls = true");
    assert_eq!(problems(&toml), ["mqtt.tls = true requires mqtt.ca_file"]);
  }

  #[test]
  fn refuses_bad_ids_intervals_and_duplicates() {
    let ok = toml("", &["ADC_Isttemperaturwert_2"]);
    assert!(problems(&ok.replace("[mqtt]", "[mqtt]\nclient_id = \"my heating\""))[0].contains("client_id"));
    assert!(problems(&ok.replace("[mqtt]", "[mqtt]\ntopic_prefix = \"a/#\""))[0].contains("topic_prefix"));
    assert!(problems(&ok.replace("[mqtt]", "[mqtt]\npassword = \"x\""))[0].contains("username"));
    assert!(problems(&ok.replace("poll_interval = 60", "poll_interval = 0"))[0].contains("poll_interval"));
    assert!(problems(&format!("{ok}interval = 0"))[0].contains("interval"));
    assert!(problems(&format!("{ok}interval = {}", u64::MAX / 2))[0].contains("interval"));
    assert!(problem("", &["ADC_Isttemperaturwert_2", "ADC_Isttemperaturwert_2"]).contains("twice"));
    assert!(problem("", &[]).contains("no [[command]]"));
  }

  #[test]
  fn reports_every_problem() {
    assert_eq!(problems(&toml("writable = [\"Other\"]", &["Nope", "Uhrzeit"])).len(), 3);
  }

  /// No writable candidate of any Device may panic the check.
  #[test]
  fn writable_check_never_panics() {
    let mut writable = 0;
    for command in device().commands().values() {
      writable += writable_problem(command).is_none() as usize;
    }
    assert!(writable > 100, "{writable}");
  }
}
