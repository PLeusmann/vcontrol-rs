//! Builds topics and discovery ids. No I/O.

pub fn state(prefix: &str, command: &str) -> String {
  format!("{prefix}/{command}")
}

pub fn set(prefix: &str, command: &str) -> String {
  format!("{prefix}/{command}/set")
}

/// Availability of the Bridge.
pub fn status(prefix: &str) -> String {
  format!("{prefix}/status")
}

pub fn discovery(discovery_prefix: &str, component: &str, client_id: &str, object_id: &str) -> String {
  format!("{discovery_prefix}/{component}/{client_id}/{object_id}/config")
}

/// Whether `id` is a valid Home Assistant node or object id.
pub fn is_valid_id(id: &str) -> bool {
  !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The Command name reduced to the characters Home Assistant allows in object ids.
pub fn object_id(command: &str) -> String {
  let mut id = String::with_capacity(command.len());
  for c in command.chars() {
    match c {
      'ä' => id.push_str("ae"),
      'ö' => id.push_str("oe"),
      'ü' => id.push_str("ue"),
      'Ä' => id.push_str("Ae"),
      'Ö' => id.push_str("Oe"),
      'Ü' => id.push_str("Ue"),
      'ß' => id.push_str("ss"),
      c if c.is_ascii_alphanumeric() || c == '_' || c == '-' => id.push(c),
      _ => id.push('_'),
    }
  }
  id
}

/// The Command name a `/set` message is for. Retained messages are ignored: they would be re-applied on every reconnect.
pub fn set_target<'a>(prefix: &str, topic: &'a str, retained: bool) -> Option<&'a str> {
  if retained {
    return None;
  }

  let command = topic.strip_prefix(prefix)?.strip_prefix('/')?.strip_suffix("/set")?;
  (!command.is_empty() && !command.contains('/')).then_some(command)
}

#[cfg(test)]
mod tests {
  use std::collections::{BTreeMap, HashMap};

  use super::*;

  #[test]
  fn topics() {
    assert_eq!(state("vcontrol", "Ecotronic_DA_Förderschnecke"), "vcontrol/Ecotronic_DA_Förderschnecke");
    assert_eq!(set("vcontrol", "A~B"), "vcontrol/A~B/set");
    assert_eq!(status("vcontrol"), "vcontrol/status");
    assert_eq!(discovery("homeassistant", "sensor", "vcontrol", "A_B"), "homeassistant/sensor/vcontrol/A_B/config");
  }

  #[test]
  fn object_ids() {
    assert_eq!(object_id("Ecotronic_DA_Förderschnecke"), "Ecotronic_DA_Foerderschnecke");
    assert_eq!(object_id("ecnsysEventType~Error"), "ecnsysEventType_Error");
    assert_eq!(object_id("a.b-c"), "a_b-c");
  }

  #[test]
  fn set_targets() {
    assert_eq!(set_target("vcontrol", "vcontrol/Foo/set", false), Some("Foo"));
    assert_eq!(set_target("a/b", "a/b/Foo/set", false), Some("Foo"));
    assert_eq!(set_target("vcontrol", "vcontrol/Foo/set", true), None);
    assert_eq!(set_target("vcontrol", "vcontrol/Foo", false), None);
    assert_eq!(set_target("vcontrol", "vcontrol//set", false), None);
    assert_eq!(set_target("vcontrol", "vcontrol/Foo/bar/set", false), None);
    assert_eq!(set_target("vcontrol", "vcontrolx/Foo/set", false), None);
    assert_eq!(set_target("vcontrol", "other/Foo/set", false), None);
    assert_eq!(set_target("vcontrol", "vcontrol/status", false), None);
  }

  /// Every Command name of every Device, read from the data the library is generated from.
  fn devices() -> BTreeMap<String, Vec<String>> {
    let load = |file: &str| -> serde_json::Value {
      let path = format!("{}/build/{file}", env!("CARGO_MANIFEST_DIR"));
      serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    };
    let commands = load("event_types.used.json");

    load("devices.used.json")
      .as_object()
      .unwrap()
      .iter()
      .map(|(device, data)| {
        let names = data["commands"]
          .as_array()
          .unwrap()
          .iter()
          .map(|id| commands[id.to_string()]["name"].as_str().unwrap().to_owned())
          .collect();
        (device.clone(), names)
      })
      .collect()
  }

  #[test]
  fn all_command_names_give_valid_unique_ids_and_safe_topics() {
    let devices = devices();
    assert!(devices.len() > 100);

    for (device, names) in devices {
      let mut ids = HashMap::new();
      for name in &names {
        assert!(!name.contains(['+', '#', '/']), "{device}: {name}");

        let id = object_id(name);
        assert!(is_valid_id(&id), "{device}: {name} -> {id}");
        if let Some(other) = ids.insert(id.clone(), name) {
          assert_eq!(other, name, "{device}: both sanitise to {id}");
        }
      }
    }
  }
}
