//! Builds the Home Assistant Discovery message for each Command. No I/O.

use serde_json::json;
use crate::{
  config::{Checked, Polled},
  payload::{self, Kind},
  topics,
};

/// The Home Assistant entity type for a Command.
pub fn component(polled: &Polled) -> &'static str {
  match payload::kind(polled.command) {
    Kind::Number if polled.writable => "number",
    Kind::Mapped if polled.writable => "select",
    _ => "sensor",
  }
}

/// The Discovery topic and JSON payload for a Command. `device_name` is the name of the detected Device.
pub fn message(config: &Checked, device_name: &str, polled: &Polled) -> (String, String) {
  let mqtt = config.mqtt();
  let (prefix, client_id) = (&mqtt.topic_prefix, &mqtt.client_id);
  let component = component(polled);
  let object_id = topics::object_id(&polled.name);
  let command = polled.command;

  let mut entity = json!({
    "name": polled.display_name,
    "unique_id": format!("{client_id}_{object_id}"),
    "state_topic": topics::state(prefix, &polled.name),
    "availability_topic": topics::status(prefix),
    "device": {
      "identifiers": [client_id],
      "name": device_name,
      "model": device_name,
      "manufacturer": "Viessmann",
    },
  });

  // Mapped Commands publish Labels, which have no unit.
  if payload::kind(command) == Kind::Number
    && let Some(unit) = command.unit()
  {
    entity["unit_of_measurement"] = json!(unit);
  }

  if component != "sensor" {
    entity["command_topic"] = json!(topics::set(prefix, &polled.name));
  }
  match component {
    "number" => {
      entity["min"] = json!(command.lower_bound());
      entity["max"] = json!(command.upper_bound());
      // The startup check made sure a writable number has a step.
      entity["step"] = json!(payload::step(command).unwrap_or(1.0));
    },
    "select" => entity["options"] = json!(payload::labels(command)),
    _ => (),
  }

  (topics::discovery(config.discovery_prefix(), component, client_id, &object_id), entity.to_string())
}

#[cfg(test)]
mod tests {
  use serde_json::Value;

  use super::*;
  use crate::config::tests::checked;

  fn discover(writable: bool, name: &str) -> (String, Value) {
    let top = if writable { format!("writable = [\"{name}\"]") } else { String::new() };
    let config = checked(&top, &[name]);
    let (topic, payload) = message(&config, "V200KO2B", &config.commands()[0]);
    (topic, serde_json::from_str(&payload).unwrap())
  }

  #[test]
  fn read_only_number_is_a_sensor_with_unit() {
    let (topic, entity) = discover(false, "ADC_Isttemperaturwert_2");
    assert_eq!(topic, "homeassistant/sensor/vcontrol/ADC_Isttemperaturwert_2/config");
    assert_eq!(
      entity,
      json!({
        "name": "ADC_Isttemperaturwert_2",
        "unique_id": "vcontrol_ADC_Isttemperaturwert_2",
        "state_topic": "vcontrol/ADC_Isttemperaturwert_2",
        "availability_topic": "vcontrol/status",
        "unit_of_measurement": "°C",
        "device": { "identifiers": ["vcontrol"], "name": "V200KO2B", "model": "V200KO2B", "manufacturer": "Viessmann" },
      })
    );
  }

  #[test]
  fn writable_command_that_is_not_allowlisted_is_a_sensor() {
    let (topic, entity) = discover(false, "Bedien_WW_Solltemperatur");
    assert!(topic.starts_with("homeassistant/sensor/"));
    assert_eq!(entity.get("command_topic"), None);
  }

  #[test]
  fn read_only_mapped_is_a_sensor_without_unit() {
    let (topic, entity) = discover(false, "KA2_KonfiSpeichervorrangA1M1");
    assert!(topic.starts_with("homeassistant/sensor/"));
    assert_eq!(entity.get("unit_of_measurement"), None);
    assert_eq!(entity.get("options"), None);
  }

  #[test]
  fn text_is_a_sensor() {
    let (topic, entity) = discover(false, "Bedienteil_SW_Index");
    assert!(topic.starts_with("homeassistant/sensor/"));
    assert_eq!(entity.get("unit_of_measurement"), None);
  }

  #[test]
  fn unmapped_k03_is_a_sensor() {
    assert!(discover(false, "K03_KonfiOelGasBetrieb").0.starts_with("homeassistant/sensor/"));
  }

  #[test]
  fn writable_numbers() {
    let (topic, entity) = discover(true, "Bedien_WW_Solltemperatur");
    assert_eq!(topic, "homeassistant/number/vcontrol/Bedien_WW_Solltemperatur/config");
    assert_eq!(entity["command_topic"], "vcontrol/Bedien_WW_Solltemperatur/set");
    assert_eq!(entity["state_topic"], "vcontrol/Bedien_WW_Solltemperatur");
    assert_eq!((&entity["min"], &entity["max"], &entity["step"]), (&json!(10.0), &json!(95.0), &json!(1.0)));
    assert_eq!(entity["unit_of_measurement"], "°C");

    let (_, entity) = discover(true, "K0E_KonfiTSAReglerverstaerkung");
    assert_eq!((&entity["min"], &entity["max"], &entity["step"]), (&json!(0.1), &json!(9.9), &json!(0.1)));
  }

  #[test]
  fn writable_mapped_is_a_select() {
    let (topic, entity) = discover(true, "KA2_KonfiSpeichervorrangA1M1");
    assert_eq!(topic, "homeassistant/select/vcontrol/KA2_KonfiSpeichervorrangA1M1/config");
    assert_eq!(entity["command_topic"], "vcontrol/KA2_KonfiSpeichervorrangA1M1/set");
    assert_eq!(entity.get("unit_of_measurement"), None);

    let options = entity["options"].as_array().unwrap();
    assert_eq!(options[0], "No cylinder priority");
    assert!(options.contains(&json!("Modulating priority (4)")));
  }

  #[test]
  fn display_name_only_changes_the_entity_name() {
    let toml = "poll_interval = 60\n[optolink]\ndevice = \"d\"\n[mqtt]\nhost = \"h\"\n[[command]]\nname = \"ADC_Isttemperaturwert_2\"\ndisplay_name = \"Vorlauf\"";
    let config = crate::config::Config::parse(toml, None).unwrap().check(crate::config::tests::device()).unwrap();
    let (topic, payload) = message(&config, "V200KO2B", &config.commands()[0]);
    let entity: Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(entity["name"], "Vorlauf");
    assert_eq!(entity["unique_id"], "vcontrol_ADC_Isttemperaturwert_2");
    assert_eq!(topic, "homeassistant/sensor/vcontrol/ADC_Isttemperaturwert_2/config");
  }

  #[test]
  fn ids_are_sanitised_and_prefixed_with_the_client_id() {
    let (topic, entity) = discover(false, "SM1_K0C_Delta_T_Überwachung");
    assert_eq!(topic, "homeassistant/sensor/vcontrol/SM1_K0C_Delta_T_Ueberwachung/config");
    assert_eq!(entity["unique_id"], "vcontrol_SM1_K0C_Delta_T_Ueberwachung");
    // The raw name stays in the state topic.
    assert_eq!(entity["state_topic"], "vcontrol/SM1_K0C_Delta_T_Überwachung");
  }
}
