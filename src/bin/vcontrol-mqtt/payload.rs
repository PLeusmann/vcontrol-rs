//! Converts values to and from MQTT payloads. No I/O.

use serde_json::json;
use vcontrol::{
  Command, DataType, Device, Value,
  types::{CircuitTimes, Date},
};

/// Published instead of an empty payload, which would delete the retained message. Home Assistant shows it as unknown.
pub const NONE: &str = "None";

/// How the Bridge treats a Command.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
  Number,
  /// Every possible value has a Label.
  Mapped,
  /// Has a Mapping, but unmapped values can occur. Read-only.
  PartlyMapped,
  Text,
  /// A week of Zeitphasen.
  Zeitprogramm,
  /// The controller's date and time. Read-only.
  Uhrzeit,
  /// The Störungen currently pending. Read-only.
  Stoerungsmeldungen,
  /// A start or end of a Ferienprogramm. Polled only through a `[[ferienprogramm]]`.
  Date,
  /// Errors with time, device ids, byte arrays, arrays. Neither polled nor written.
  Complex,
}

pub fn kind(command: &Command) -> Kind {
  if command.block_count().is_some() {
    return Kind::Complex;
  }

  match command.data_type() {
    DataType::String => Kind::Text,
    DataType::CircuitTimes => Kind::Zeitprogramm,
    DataType::DateTime => Kind::Uhrzeit,
    DataType::ErrorIndex => Kind::Stoerungsmeldungen,
    DataType::Date => Kind::Date,
    DataType::Int | DataType::Double | DataType::Byte => match command.mapping() {
      None => Kind::Number,
      Some(mapping) if covers(mapping, command.lower_bound(), command.upper_bound()) => Kind::Mapped,
      Some(_) => Kind::PartlyMapped,
    },
    _ => Kind::Complex,
  }
}

/// Whether the Mapping has a Label for every value between the bounds.
fn covers(mapping: &phf::Map<i32, &'static str>, lower: Option<f64>, upper: Option<f64>) -> bool {
  match (lower, upper) {
    // Without bounds the Mapping is all we know about the value range.
    (None, None) => true,
    (Some(lower), Some(upper)) => {
      upper - lower < mapping.len() as f64
        && (lower.ceil() as i32..=upper.floor() as i32).all(|n| mapping.contains_key(&n))
    },
    _ => false,
  }
}

/// The Label for `n`. Labels shared by several numbers of a Mapping get the number appended to stay unique.
pub fn label(command: &Command, n: f64) -> Option<String> {
  let mapping = command.mapping()?;
  let key = n as i32;
  if key as f64 != n {
    return None;
  }
  let label = *mapping.get(&key)?;

  Some(if mapping.values().filter(|other| **other == label).count() > 1 {
    format!("{label} ({key})")
  } else {
    label.to_owned()
  })
}

/// All Labels of a Mapping, ordered by number.
pub fn labels(command: &Command) -> Vec<String> {
  let mut keys: Vec<i32> = command.mapping().into_iter().flat_map(|mapping| mapping.keys().copied()).collect();
  keys.sort_unstable();
  keys.into_iter().filter_map(|key| label(command, key as f64)).collect()
}

/// The state payload for a value: the plain value without unit, the Label, or JSON for a Zeitprogramm and
/// Störungsmeldungen. `None` for values that cannot be published.
pub fn state(command: &Command, value: &Value, device: &Device) -> Option<String> {
  let number = |n: f64, plain: String| label(command, n).unwrap_or(plain);

  Some(match (kind(command), value) {
    // A week without Zeitphasen is all 0xFF, which the library reads as empty.
    (Kind::Zeitprogramm, Value::Empty) => serde_json::to_string(&CircuitTimes::from_bytes(&[0xff; 56])).ok()?,
    (_, Value::Empty) => NONE.to_owned(),
    (_, Value::Int(n)) => number(*n as f64, n.to_string()),
    (_, Value::Double(n)) => number(*n, n.to_string()),
    (_, Value::String(s)) if s.is_empty() => NONE.to_owned(),
    (_, Value::String(s)) => s.clone(),
    (Kind::Zeitprogramm, Value::CircuitTimes(week)) => serde_json::to_string(week).ok()?,
    // ISO local date-time: the controller has no time zone.
    (Kind::Uhrzeit, Value::DateTime(date_time)) => date_time.to_string(),
    (Kind::Stoerungsmeldungen, Value::ByteArray(codes)) => {
      let list: Vec<_> = codes
        .iter()
        .map(|code| match device.errors().get(&i32::from(*code)) {
          Some(text) => json!({ "code": format!("{code:02X}"), "text": text }),
          None => json!({ "code": format!("{code:02X}") }),
        })
        .collect();
      json!(list).to_string()
    },
    _ => return None,
  })
}

/// What the controller reports for both dates when no Ferienprogramm is set, and what ends one.
pub fn no_holiday() -> Date {
  Date::new(1970, 1, 1).expect("valid date")
}

/// The state payload of a Ferienprogramm: `start/end`, or `None` when none is set.
pub fn ferienprogramm_state(start: &Value, end: &Value) -> Option<String> {
  match (start, end) {
    (Value::Date(start), Value::Date(end)) if *start == no_holiday() && *end == no_holiday() => Some(NONE.to_owned()),
    (Value::Date(start), Value::Date(end)) => Some(format!("{start}/{end}")),
    _ => None,
  }
}

/// Validates a Ferienprogramm `/set` payload: `start/end`, or `None` to end it. `today` is the Bridge's local date.
pub fn parse_ferienprogramm(payload: &[u8], today: Date) -> Result<Option<(Date, Date)>, String> {
  let text = str::from_utf8(payload).map_err(|_| "payload is not UTF-8".to_owned())?.trim();
  if text == NONE {
    return Ok(None);
  }

  // Only the exact form YYYY-MM-DD: the controller stores the year as four BCD digits.
  let date = |part: &str| part.parse::<Date>().ok().filter(|date| date.to_string() == part);
  let Some((Some(start), Some(end))) = text.split_once('/').map(|(start, end)| (date(start), date(end))) else {
    return Err(format!("{text:?} is not start/end (YYYY-MM-DD/YYYY-MM-DD) or None"));
  };

  if start > end {
    return Err(format!("start {start} is after end {end}"));
  }
  if end < today {
    return Err(format!("end {end} is before today"));
  }
  Ok(Some((start, end)))
}

/// Validates a Zeitprogramm payload. Rejects, and never rounds, what the controller cannot store as given.
fn parse_zeitprogramm(text: &str) -> Result<Value, String> {
  let json: serde_json::Value = serde_json::from_str(text).map_err(|err| format!("not JSON: {err}"))?;
  let week: CircuitTimes = serde_json::from_value(json.clone()).map_err(|err| format!("not a Zeitprogramm: {err}"))?;

  // The library truncates times to the 10-minute grid when writing and ignores unknown fields: what it would
  // write must read back as exactly what was sent.
  let stored = CircuitTimes::from_bytes(&week.to_bytes());
  if serde_json::to_value(&stored).ok().as_ref() != Some(&json) {
    return Err("not a Zeitprogramm with times on the 10-minute grid (HH:MM)".to_owned());
  }

  for (day, phases) in json.as_object().into_iter().flatten() {
    // Times are zero-padded HH:MM, so they compare as strings.
    let mut previous_end = "00:00";
    for phase in phases.as_array().into_iter().flatten().filter(|phase| !phase.is_null()) {
      let (from, to) = (phase["from"].as_str().unwrap_or_default(), phase["to"].as_str().unwrap_or_default());
      if from > "24:00" || to > "24:00" {
        return Err(format!("{day}: {from}–{to} is after 24:00"));
      }
      if from >= to {
        return Err(format!("{day}: {from}–{to} does not end after it starts"));
      }
      if from < previous_end {
        return Err(format!("{day}: {from}–{to} overlaps the Zeitphase before it or is out of order"));
      }
      previous_end = to;
    }
  }

  Ok(Value::CircuitTimes(Box::new(week)))
}

/// The value to write for the number `n`.
pub fn number_value(command: &Command, n: f64) -> Value {
  if command.data_type() == DataType::Double { Value::Double(n) } else { Value::Int(n as i64) }
}

/// Whether writing `n` stores exactly `n`. The library truncates what the controller cannot represent
/// (e.g. 9 for a Command that counts in steps of 5), so such values must never reach it.
pub fn representable(command: &Command, n: f64) -> bool {
  if command.data_type() != DataType::Double && n.fract() != 0.0 {
    return false;
  }
  let value = number_value(command, n);
  command
    .serialize(value.clone())
    .is_ok_and(|bytes| bytes.len() == command.block_len() && command.deserialize(&bytes).ok() == Some(value))
}

/// The distance between two neighbouring values the controller can store, measured at the lower bound.
pub fn step(command: &Command) -> Option<f64> {
  let lower = command.lower_bound()?;
  let bytes = command.serialize(number_value(command, lower)).ok()?;

  // The next raw value. The byte order is not public, so count up both ways; the wrong one gives a larger step.
  let mut little_endian = bytes.clone();
  let mut big_endian = bytes;
  let _ = little_endian.iter_mut().try_for_each(count_up);
  let _ = big_endian.iter_mut().rev().try_for_each(count_up);

  [little_endian, big_endian]
    .iter()
    .filter_map(|bytes| match command.deserialize(bytes) {
      Ok(Value::Int(n)) => Some(n as f64),
      Ok(Value::Double(n)) => Some(n),
      _ => None,
    })
    // Rounded, because e.g. 0.2 - 0.1 is not exactly 0.1.
    .map(|next| ((next - lower).abs() * 1000.0).round() / 1000.0)
    .filter(|step| *step > 0.0)
    .min_by(f64::total_cmp)
}

/// Adds one to a byte; `Err` (stop) unless it carries over to the next byte.
fn count_up(byte: &mut u8) -> Result<(), ()> {
  *byte = byte.wrapping_add(1);
  if *byte == 0 { Ok(()) } else { Err(()) }
}

/// Whether every bit of `bytes` belongs to the Command. `Command::set` writes whole bytes, so writing a Command
/// that is only some bits of a byte would overwrite the other Commands in it.
pub fn owns_all_bits(command: &Command, bytes: &[u8]) -> bool {
  let value = command.deserialize(bytes).ok();
  (0..bytes.len() * 8).all(|bit| {
    let mut flipped = bytes.to_vec();
    flipped[bit / 8] ^= 1 << (bit % 8);
    command.deserialize(&flipped).ok() != value
  })
}

/// Validates a `/set` payload and turns it into the value to write.
pub fn parse_write(command: &Command, payload: &[u8]) -> Result<Value, String> {
  let text = str::from_utf8(payload).map_err(|_| "payload is not UTF-8".to_owned())?.trim();

  let number = || match text.parse::<f64>() {
    Ok(n) if n.is_finite() => Ok(n),
    _ => Err(format!("{text:?} is not a finite number")),
  };

  let n = match kind(command) {
    Kind::Zeitprogramm => return parse_zeitprogramm(text),
    Kind::Number => number()?,
    Kind::Mapped => {
      let mapping = command.mapping().expect("mapped");
      match mapping.keys().find(|key| label(command, **key as f64).as_deref() == Some(text)) {
        Some(key) => *key as f64,
        None => match number() {
          Ok(n) if label(command, n).is_some() => n,
          Ok(n) => return Err(format!("{n} is not in the Mapping")),
          Err(_) => return Err(format!("unknown Label {text:?}")),
        },
      }
    },
    kind => return Err(format!("{kind:?} Commands cannot be written")),
  };

  if command.lower_bound().is_some_and(|lower| n < lower) || command.upper_bound().is_some_and(|upper| n > upper) {
    return Err(format!("{n} is outside {:?}..={:?}", command.lower_bound(), command.upper_bound()));
  }

  if !representable(command, n) {
    return Err(format!("the controller cannot store {n} exactly"));
  }

  Ok(number_value(command, n))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::config::tests::{command, device};

  #[test]
  fn kinds() {
    assert_eq!(kind(command("ADC_Isttemperaturwert_2")), Kind::Number);
    assert_eq!(kind(command("K03_KonfiOelGasBetrieb")), Kind::Number);
    assert_eq!(kind(command("KA2_KonfiSpeichervorrangA1M1")), Kind::Mapped);
    assert_eq!(kind(command("Bedienteil_SW_Index")), Kind::Text);
    assert_eq!(kind(command("Uhrzeit")), Kind::Uhrzeit);
    assert_eq!(kind(command("Schaltzeiten_A1M1_HK")), Kind::Zeitprogramm);
    assert_eq!(kind(command("ecnsysEventType~ErrorIndex")), Kind::Stoerungsmeldungen);
    assert_eq!(kind(command("FerienBeginnA1M1")), Kind::Date);
    assert_eq!(kind(command("ecnsysEventType~Error")), Kind::Complex);
  }

  #[test]
  fn mapping_must_cover_the_bounds() {
    // Labels for 0..=15.
    let mapping = command("KA2_KonfiSpeichervorrangA1M1").mapping().unwrap();
    assert_eq!(mapping.len(), 16);

    assert!(covers(mapping, None, None));
    assert!(covers(mapping, Some(0.0), Some(15.0)));
    assert!(covers(mapping, Some(2.0), Some(3.0)));
    // Unmapped raw values can occur.
    assert!(!covers(mapping, Some(0.0), Some(16.0)));
    assert!(!covers(mapping, Some(-1.0), Some(15.0)));
    assert!(!covers(mapping, Some(0.0), Some(255.0)));
    assert!(!covers(mapping, Some(0.0), Some(4e9)));
    assert!(!covers(mapping, Some(0.0), None));
    assert!(!covers(mapping, None, Some(15.0)));
  }

  #[test]
  fn state_is_plain_value_without_unit() {
    let temperature = command("ADC_Isttemperaturwert_2");
    assert_eq!(temperature.unit(), Some("°C"));
    assert_eq!(state(temperature, &Value::Double(21.5), device()).unwrap(), "21.5");
    assert_eq!(state(temperature, &Value::Double(21.0), device()).unwrap(), "21");
    assert_eq!(state(command("Bedien_WW_Solltemperatur"), &Value::Int(50), device()).unwrap(), "50");
    assert_eq!(state(command("Bedienteil_SW_Index"), &Value::String("12".into()), device()).unwrap(), "12");
  }

  #[test]
  fn empty_is_none_never_an_empty_payload() {
    assert_eq!(state(command("ADC_Isttemperaturwert_2"), &Value::Empty, device()).unwrap(), "None");
    assert_eq!(state(command("Bedienteil_SW_Index"), &Value::String(String::new()), device()).unwrap(), "None");
  }

  #[test]
  fn complex_values_have_no_payload() {
    assert_eq!(state(command("ADC_Isttemperaturwert_2"), &Value::ByteArray(vec![1]), device()), None);
    assert_eq!(state(command("ADC_Isttemperaturwert_2"), &Value::Array(vec![]), device()), None);
  }

  #[test]
  fn mapped_state_is_the_label() {
    let priority = command("KA2_KonfiSpeichervorrangA1M1");
    assert_eq!(state(priority, &Value::Int(0), device()).unwrap(), "No cylinder priority");
    // Labels shared by several numbers are made unique.
    assert_eq!(state(priority, &Value::Int(3), device()).unwrap(), "Modulating priority (3)");
    assert_eq!(state(priority, &Value::Int(4), device()).unwrap(), "Modulating priority (4)");
    // An unmapped raw value is published as the number.
    assert_eq!(state(priority, &Value::Int(200), device()).unwrap(), "200");

    let all = labels(priority);
    assert_eq!(all.len(), priority.mapping().unwrap().len());
    assert_eq!(all[0], "No cylinder priority");
    let mut unique = all.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), all.len());
  }

  #[test]
  fn write_numbers() {
    let int = command("Bedien_WW_Solltemperatur"); // Int, 10..=95
    assert_eq!(parse_write(int, b"50"), Ok(Value::Int(50)));
    assert_eq!(parse_write(int, b" 50.0\n"), Ok(Value::Int(50)));
    assert_eq!(parse_write(int, b"10"), Ok(Value::Int(10)));
    assert_eq!(parse_write(int, b"95"), Ok(Value::Int(95)));
    assert!(parse_write(int, b"50.5").is_err());
    assert!(parse_write(int, b"9").is_err());
    assert!(parse_write(int, b"96").is_err());
    assert!(parse_write(int, b"-1").is_err());
    assert!(parse_write(int, b"1e99").is_err());

    let double = command("K0E_KonfiTSAReglerverstaerkung"); // Double, 0.1..=9.9
    assert_eq!(parse_write(double, b"2.5"), Ok(Value::Double(2.5)));
    assert_eq!(parse_write(double, b"3"), Ok(Value::Double(3.0)));
    assert!(parse_write(double, b"0").is_err());
    assert!(parse_write(double, b"10").is_err());
  }

  #[test]
  fn write_rejects_values_between_steps() {
    // Counts in steps of 5 (5..=995): 9 would silently become 5.
    let delay = command("K80_KonfiFehlerDelay");
    assert_eq!(parse_write(delay, b"10"), Ok(Value::Double(10.0)));
    assert!(parse_write(delay, b"9").is_err());
    assert!(parse_write(delay, b"7.5").is_err());
    assert_eq!(step(delay), Some(5.0));

    // A whole number stored as a signed byte (-9..=15): -0.5 would become 0.
    let frost = command("KA3_KonfiFrostgrenzeA1M1");
    assert_eq!(parse_write(frost, b"-8"), Ok(Value::Double(-8.0)));
    assert!(parse_write(frost, b"-0.5").is_err());
    assert!(parse_write(frost, b"-8.9").is_err());
    assert_eq!(step(frost), Some(1.0));

    // Tenths (0.1..=9.9).
    let gain = command("K0E_KonfiTSAReglerverstaerkung");
    assert!(parse_write(gain, b"2.55").is_err());
    assert_eq!(step(gain), Some(0.1));
    assert_eq!(step(command("Bedien_WW_Solltemperatur")), Some(1.0));
    // Two bytes, tenths (0..=500).
    assert_eq!(step(command("AGTemperaturwertMAXBedien")), Some(0.1));
    // No bounds, no step.
    assert_eq!(step(command("K03_KonfiOelGasBetrieb")), None);

    // Counts in steps of 3, on another Device.
    let reset_time = vcontrol::device::V100CC1X.command("K89_KonfiFkt2u6NachstellzeitZielTR").unwrap();
    assert_eq!(step(reset_time), Some(3.0));
    assert!(parse_write(reset_time, b"6").is_ok());
    assert!(parse_write(reset_time, b"7").is_err());
  }

  #[test]
  fn bit_fields_do_not_own_their_byte() {
    // One bit of a byte shared with other flags.
    let flag = vcontrol::device::ECOTRONIC.command("AutomaticFlag_WWM2").unwrap();
    assert!(!owns_all_bits(flag, &[0x01]));
    assert!(!owns_all_bits(flag, &[0x00]));

    assert!(owns_all_bits(command("Bedien_WW_Solltemperatur"), &[50]));
    assert!(owns_all_bits(command("KA2_KonfiSpeichervorrangA1M1"), &[2]));
    assert!(owns_all_bits(command("AGTemperaturwertMAXBedien"), &[0xD7, 0x00]));
  }

  /// Every value Home Assistant can offer for any writable number (min + k × step) is accepted.
  #[test]
  fn every_step_of_every_writable_number_is_accepted() {
    let mut checked = 0;
    for (name, command) in crate::config::tests::device().commands().entries() {
      let (Some(lower), Some(upper)) = (command.lower_bound(), command.upper_bound()) else { continue };
      // A number with a single allowed value has no step and is refused as writable.
      if kind(command) != Kind::Number
        || lower == upper
        || !representable(command, lower)
        || !representable(command, upper)
      {
        continue;
      }
      let step = step(command).unwrap_or_else(|| panic!("{name} has no step"));
      for k in 0..=(((upper - lower) / step).round() as u32).min(2000) {
        // Rounded like a UI would print it.
        let text = format!("{:.3}", lower + k as f64 * step);
        assert!(parse_write(command, text.as_bytes()).is_ok(), "{name}: {text} (step {step})");
      }
      checked += 1;
    }
    assert!(checked > 100, "{checked}");
  }

  #[test]
  fn write_rejects_garbage() {
    for command in [command("Bedien_WW_Solltemperatur"), command("K0E_KonfiTSAReglerverstaerkung")] {
      for payload in ["NaN", "nan", "inf", "-inf", "infinity", "", " ", "abc", "50 °C", "0x10", "None"] {
        assert!(parse_write(command, payload.as_bytes()).is_err(), "{payload:?}");
      }
      assert!(parse_write(command, &[0xff, 0xfe]).is_err());
    }
  }

  #[test]
  fn write_labels() {
    let priority = command("KA2_KonfiSpeichervorrangA1M1");
    assert_eq!(parse_write(priority, b"No cylinder priority"), Ok(Value::Int(0)));
    assert_eq!(parse_write(priority, b"Modulating priority (4)"), Ok(Value::Int(4)));
    assert_eq!(parse_write(priority, b"2"), Ok(Value::Int(2)));

    // Ambiguous or unknown Labels, unmapped numbers.
    assert!(parse_write(priority, b"Modulating priority").is_err());
    assert!(parse_write(priority, b"no cylinder priority").is_err());
    assert!(parse_write(priority, b"Nonsense").is_err());
    assert!(parse_write(priority, b"200").is_err());
    assert!(parse_write(priority, b"0.5").is_err());
    assert!(parse_write(priority, b"-1").is_err());
    assert!(parse_write(priority, b"4294967296").is_err());
    assert!(parse_write(priority, b"NaN").is_err());
  }

  #[test]
  fn write_rejects_other_kinds() {
    assert!(parse_write(command("Bedienteil_SW_Index"), b"1").is_err());
    assert!(parse_write(command("Uhrzeit"), b"1").is_err());
  }

  const WEEK: &str = r#"{"mon":[{"from":"05:30","to":"22:30"},null,null,null],"tue":[{"from":"00:00","to":"06:00"},{"from":"06:00","to":"08:10"},{"from":"16:00","to":"24:00"},null],"wed":[null,null,null,null],"thu":[null,null,null,null],"fri":[null,null,null,null],"sat":[null,null,null,null],"sun":[{"from":"07:00","to":"23:00"},null,null,null]}"#;

  fn zeitprogramm() -> &'static Command {
    command("Schaltzeiten_A1M1_HK")
  }

  #[test]
  fn zeitprogramm_payload_is_the_week_as_json() {
    let Ok(week) = parse_write(zeitprogramm(), WEEK.as_bytes()) else { panic!() };
    assert_eq!(state(zeitprogramm(), &week, device()).unwrap(), WEEK);

    // A week without Zeitphasen is all 0xFF, which the library reads as empty.
    let empty = state(zeitprogramm(), &Value::Empty, device()).unwrap();
    assert_eq!(
      empty,
      r#"{"mon":[null,null,null,null],"tue":[null,null,null,null],"wed":[null,null,null,null],"thu":[null,null,null,null],"fri":[null,null,null,null],"sat":[null,null,null,null],"sun":[null,null,null,null]}"#
    );
    assert!(parse_write(zeitprogramm(), empty.as_bytes()).is_ok());
  }

  #[test]
  fn zeitprogramm_write_is_rejected_never_rounded() {
    let with = |from: &str, to: &str| WEEK.replacen(from, to, 1);
    for (payload, why) in [
      ("[]".to_owned(), "not a week"),
      ("{}".to_owned(), "no days"),
      (
        with(r#""sun":[{"from":"07:00","to":"23:00"},null,null,null]"#, r#""sun":[null,null,null]"#),
        "three Zeitphasen",
      ),
      (with(r#","sun":"#, r#","holiday":[null,null,null,null],"sun":"#), "unknown day"),
      (with(r#""to":"22:30"}"#, r#""to":"22:30","on":true}"#), "unknown field"),
      (with("05:30", "05:35"), "off the 10-minute grid"),
      (with("05:30", "5:30"), "not HH:MM"),
      (with("05:30", "05:30:00"), "seconds"),
      (with("22:30", "24:10"), "after 24:00"),
      (with("22:30", "25:00"), "no such hour"),
      (with("22:30", "05:30"), "from = to"),
      (with("22:30", "04:00"), "from > to"),
      (with(r#""from":"06:00","to":"08:10""#, r#""from":"05:00","to":"08:10""#), "overlap"),
      (with(r#""from":"16:00","to":"24:00""#, r#""from":"03:00","to":"04:00""#), "out of order"),
      ("None".to_owned(), "None"),
    ] {
      assert!(parse_write(zeitprogramm(), payload.as_bytes()).is_err(), "{why}: {payload}");
    }
  }

  #[test]
  fn uhrzeit_is_an_iso_local_date_time() {
    let uhrzeit = vcontrol::types::DateTime::new(2026, 11, 19, 13, 54, 39).unwrap();
    assert_eq!(state(command("Uhrzeit"), &Value::DateTime(uhrzeit), device()).unwrap(), "2026-11-19T13:54:39");
    assert_eq!(state(command("Uhrzeit"), &Value::Empty, device()).unwrap(), "None");
  }

  #[test]
  fn stoerungsmeldungen_are_a_list_with_text_for_known_codes() {
    let stoerungen = command("ecnsysEventType~ErrorIndex");
    let known = *device().errors().keys().find(|code| (1..=255).contains(*code)).unwrap() as u8;
    let text = device().errors()[&i32::from(known)];
    let unknown = (1..=255u8).find(|code| !device().errors().contains_key(&i32::from(*code))).unwrap();

    assert_eq!(state(stoerungen, &Value::ByteArray(vec![]), device()).unwrap(), "[]");
    let list: serde_json::Value =
      serde_json::from_str(&state(stoerungen, &Value::ByteArray(vec![known, unknown]), device()).unwrap()).unwrap();
    assert_eq!(list, json!([{ "code": format!("{known:02X}"), "text": text }, { "code": format!("{unknown:02X}") }]));
  }

  fn date(year: u16, month: u8, day: u8) -> Date {
    Date::new(year, month, day).unwrap()
  }

  #[test]
  fn ferienprogramm_payload() {
    let none = Value::Date(no_holiday());
    assert_eq!(ferienprogramm_state(&none, &none).unwrap(), "None");
    let (start, end) = (Value::Date(date(2026, 12, 20)), Value::Date(date(2027, 1, 6)));
    assert_eq!(ferienprogramm_state(&start, &end).unwrap(), "2026-12-20/2027-01-06");
    // Any other combination is published as read.
    assert_eq!(ferienprogramm_state(&none, &end).unwrap(), "1970-01-01/2027-01-06");
    assert_eq!(ferienprogramm_state(&Value::Empty, &end), None);
  }

  #[test]
  fn ferienprogramm_write() {
    let today = date(2026, 11, 20);
    let parse = |payload: &str| parse_ferienprogramm(payload.as_bytes(), today);

    assert_eq!(parse("2026-12-20/2027-01-06"), Ok(Some((date(2026, 12, 20), date(2027, 1, 6)))));
    assert_eq!(parse(" 2026-11-20/2026-11-20\n"), Ok(Some((date(2026, 11, 20), date(2026, 11, 20)))));
    // Started in the past, still running.
    assert_eq!(parse("2026-11-01/2026-11-20"), Ok(Some((date(2026, 11, 1), date(2026, 11, 20)))));
    assert_eq!(parse("None"), Ok(None));

    for payload in [
      "2027-01-06/2026-12-20",
      "2026-11-01/2026-11-19",
      "2026-12-20",
      "2026-12-20/",
      "/2027-01-06",
      "2026-12-20/2027-01-06/2027-01-07",
      "2026-12-20 2027-01-06",
      "2026-12-32/2027-01-06",
      "2026-2-20/2027-01-06",
      "12026-12-20/12027-01-06",
      "+2026-12-20/2027-01-06",
      "none",
      "",
    ] {
      assert!(parse(payload).is_err(), "{payload}");
    }
    assert!(parse_ferienprogramm(&[0xff, 0xfe], today).is_err());
  }
}
