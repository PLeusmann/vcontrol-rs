//! Builds the Home Assistant Discovery message for each Command. No I/O.

use crate::{
  config::{Checked, Polled, Source},
  payload::{self, Kind},
  topics,
};
use serde_json::json;

/// Today's Zeitphasen from the week, e.g. `05:30–22:30, 23:00–24:00`. `now().weekday()` is 0 for Monday.
const TODAYS_ZEITPHASEN: &str = "{% set day = value_json[['mon','tue','wed','thu','fri','sat','sun'][now().weekday()]] %}\
{% for phase in day if phase %}{{ phase['from'] }}–{{ phase['to'] }}{% if not loop.last %}, {% endif %}{% endfor %}";

/// The values a Ferienprogramm accepts on its Set topic.
const FERIENPROGRAMM_PATTERN: &str = r"^(\d{4}-\d{2}-\d{2}/\d{4}-\d{2}-\d{2}|None)$";

/// The Home Assistant entity type for a Command or Ferienprogramm.
pub fn component(polled: &Polled) -> &'static str {
  if !polled.writable {
    return "sensor";
  }
  match &polled.source {
    Source::Ferienprogramm { .. } => "text",
    Source::Command(command) => match payload::kind(command) {
      Kind::Number => "number",
      Kind::Mapped => "select",
      // A Zeitprogramm has no Home Assistant control; it is written on its Set topic.
      _ => "sensor",
    },
  }
}

/// The Discovery topic and JSON payload for a Command. `device_name` is the name of the detected Device.
pub fn message(config: &Checked, device_name: &str, polled: &Polled) -> (String, String) {
  let mqtt = config.mqtt();
  let (prefix, client_id) = (&mqtt.topic_prefix, &mqtt.client_id);
  let component = component(polled);
  let object_id = topics::object_id(&polled.name);
  let state_topic = topics::state(prefix, &polled.name);

  let mut entity = json!({
    "name": polled.display_name,
    "unique_id": format!("{client_id}_{object_id}"),
    "state_topic": state_topic,
    "availability_topic": topics::status(prefix),
    "device": {
      "identifiers": [client_id],
      "name": device_name,
      "model": device_name,
      "manufacturer": "Viessmann",
    },
  });

  if component != "sensor" {
    entity["command_topic"] = json!(topics::set(prefix, &polled.name));
  }

  match polled.source {
    Source::Ferienprogramm { .. } => {
      if polled.writable {
        entity["pattern"] = json!(FERIENPROGRAMM_PATTERN);
      }
    },
    Source::Command(command) => {
      match payload::kind(command) {
        // Mapped Commands publish Labels, which have no unit.
        Kind::Number => {
          if let Some(unit) = command.unit() {
            entity["unit_of_measurement"] = json!(unit);
          }
        },
        // The state shows today, the attributes hold the week.
        Kind::Zeitprogramm => {
          entity["value_template"] = json!(TODAYS_ZEITPHASEN);
          entity["json_attributes_topic"] = json!(state_topic);
        },
        Kind::Stoerungsmeldungen => {
          entity["value_template"] = json!("{{ value_json | count }}");
          entity["json_attributes_topic"] = json!(state_topic);
          entity["json_attributes_template"] = json!("{{ {'stoerungsmeldungen': value_json} | tojson }}");
        },
        _ => (),
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
    },
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
  fn zeitprogramm_is_a_sensor_showing_today_with_the_week_in_the_attributes() {
    for writable in [false, true] {
      let (topic, entity) = discover(writable, "Schaltzeiten_A1M1_HK");
      assert_eq!(topic, "homeassistant/sensor/vcontrol/Schaltzeiten_A1M1_HK/config");
      assert_eq!(entity["json_attributes_topic"], "vcontrol/Schaltzeiten_A1M1_HK");
      assert_eq!(entity["value_template"], TODAYS_ZEITPHASEN);
      assert_eq!(entity.get("command_topic"), None);
      assert_eq!(entity.get("unit_of_measurement"), None);
    }
  }

  #[test]
  fn uhrzeit_is_a_plain_sensor() {
    let (topic, entity) = discover(false, "Uhrzeit");
    assert!(topic.starts_with("homeassistant/sensor/"));
    // `timestamp` would need a time zone.
    assert_eq!(entity.get("device_class"), None);
    assert_eq!(entity.get("value_template"), None);
  }

  #[test]
  fn stoerungsmeldungen_count_with_the_list_in_the_attributes() {
    let (topic, entity) = discover(false, "ecnsysEventType~ErrorIndex");
    assert_eq!(topic, "homeassistant/sensor/vcontrol/ecnsysEventType_ErrorIndex/config");
    assert_eq!(entity["value_template"], "{{ value_json | count }}");
    assert_eq!(entity["json_attributes_topic"], "vcontrol/ecnsysEventType~ErrorIndex");
    assert_eq!(entity["json_attributes_template"], "{{ {'stoerungsmeldungen': value_json} | tojson }}");
  }

  #[test]
  fn ferienprogramm_is_a_text_when_writable() {
    let ferien = "[[ferienprogramm]]\nid = \"ferienprogramm_hk1\"\ndisplay_name = \"Ferienprogramm\"\nstart = \"FerienBeginnA1M1\"\nend = \"FerienEndeA1M1\"\n";
    let discover = |writable: &str| {
      let toml = format!("poll_interval = 60\n{writable}\n[optolink]\nport = 1\n[mqtt]\nhost = \"h\"\n{ferien}");
      let config = crate::config::Config::parse(&toml, None).unwrap().check(crate::config::tests::device()).unwrap();
      let (topic, payload) = message(&config, "V200KO2B", &config.commands()[0]);
      (topic, serde_json::from_str::<Value>(&payload).unwrap())
    };

    let (topic, entity) = discover("writable = [\"ferienprogramm_hk1\"]");
    assert_eq!(topic, "homeassistant/text/vcontrol/ferienprogramm_hk1/config");
    assert_eq!(entity["name"], "Ferienprogramm");
    assert_eq!(entity["unique_id"], "vcontrol_ferienprogramm_hk1");
    assert_eq!(entity["state_topic"], "vcontrol/ferienprogramm_hk1");
    assert_eq!(entity["command_topic"], "vcontrol/ferienprogramm_hk1/set");
    assert_eq!(entity["pattern"], r"^(\d{4}-\d{2}-\d{2}/\d{4}-\d{2}-\d{2}|None)$");

    let (topic, entity) = discover("");
    assert_eq!(topic, "homeassistant/sensor/vcontrol/ferienprogramm_hk1/config");
    assert_eq!((entity.get("command_topic"), entity.get("pattern")), (None, None));
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
