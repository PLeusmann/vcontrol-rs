use arrayref::array_ref;

use crate::{
  AccessMode, DataType, Error, Optolink, Parameter, Value,
  conversion::Conversion,
  protocol::Protocol,
  types::{self, CircuitTimes, Date, DateTime, DeviceId, DeviceIdF0},
};

/// A Zeitprogramm (`CircuitTimes`, 56 bytes) is transferred as one telegram of this many bytes per day,
/// Monday first, at `addr + 8 × day`: the controller answers at most 55 bytes per telegram.
const DAY_LEN: usize = 8;

/// A command which can be executed on an Optolink connection.
#[derive(Debug, PartialEq)]
pub struct Command {
  pub(crate) addr: u16,
  pub(crate) mode: AccessMode,
  pub(crate) data_type: DataType,
  pub(crate) parameter: Parameter,
  pub(crate) block_count: Option<usize>,
  pub(crate) block_len: usize,
  pub(crate) byte_len: usize,
  pub(crate) byte_pos: usize,
  pub(crate) bit_pos: usize,
  pub(crate) bit_len: Option<usize>,
  pub(crate) conversion: Option<Conversion>,
  pub(crate) lower_bound: Option<f64>,
  pub(crate) upper_bound: Option<f64>,
  pub(crate) unit: Option<&'static str>,
  pub(crate) mapping: Option<phf::map::Map<i32, &'static str>>,
}

impl Command {
  /// Returns the command address.
  pub fn addr(&self) -> u16 {
    self.addr
  }

  /// Get the command's access mode.
  pub fn access_mode(&self) -> AccessMode {
    self.mode
  }

  /// Get the command's data type.
  pub fn data_type(&self) -> DataType {
    self.data_type
  }

  /// Returns the command block count.
  pub fn block_count(&self) -> Option<usize> {
    self.block_count
  }

  /// Returns the command block length.
  pub fn block_len(&self) -> usize {
    self.block_len
  }

  /// Returns the unit for the command value.
  pub fn unit(&self) -> Option<&'static str> {
    self.unit
  }

  /// Returns the lower bound for the command value.
  pub fn lower_bound(&self) -> Option<f64> {
    self.lower_bound
  }

  /// Returns the upper bound for the command value.
  pub fn upper_bound(&self) -> Option<f64> {
    self.upper_bound
  }

  /// Returns the mapping for the command value.
  pub fn mapping(&self) -> Option<&phf::map::Map<i32, &'static str>> {
    self.mapping.as_ref()
  }

  /// Deserializes the command value from bytes.
  pub fn deserialize(&self, bytes: &[u8]) -> Result<Value, Error> {
    if let Some(block_count) = self.block_count {
      let block_len = self.block_len / block_count;

      let mut values = vec![];
      for i in 0..block_count {
        let start = i * block_len;
        let value = self.parse_value(&bytes[(start)..(start + block_len)])?;
        values.push(value);
      }

      Ok(Value::Array(values))
    } else {
      self.parse_value(bytes)
    }
  }

  pub(crate) fn parse_value(&self, bytes: &[u8]) -> Result<Value, Error> {
    if !matches!(
      self.parameter,
      Parameter::SInt | Parameter::SIntHighByteFirst | Parameter::SInt4 | Parameter::SInt4HighByteFirst
    ) && self.lower_bound().is_none()
      && self.upper_bound().is_none()
      && bytes.iter().all(|&b| b == 0xff)
    {
      return Ok(Value::Empty);
    }

    let mut value = match &self.data_type {
      DataType::DeviceId => {
        if bytes.len() != 8 {
          return Err(Error::InvalidFormat("array length is not 8".to_string()));
        }

        Value::DeviceId(DeviceId::from_bytes(array_ref![bytes, 0, 8]))
      },
      DataType::DeviceIdF0 => {
        if bytes.len() != 2 {
          return Err(Error::InvalidFormat("array length is not 2".to_string()));
        }

        Value::DeviceIdF0(DeviceIdF0::from_bytes(array_ref![bytes, 0, 2]))
      },
      DataType::Date => {
        if bytes.len() != 8 {
          return Err(Error::InvalidFormat("array length is not 8".to_string()));
        }

        Value::Date(Date::from_bytes(array_ref![bytes, 0, 8])?)
      },
      DataType::DateTime => {
        if bytes.len() != 8 {
          return Err(Error::InvalidFormat("array length is not 8".to_string()));
        }

        Value::DateTime(DateTime::from_bytes(array_ref![bytes, 0, 8])?)
      },
      DataType::CircuitTimes => {
        if bytes.len() != 56 {
          return Err(Error::InvalidFormat("array length is not 56".to_string()));
        }

        Value::CircuitTimes(Box::new(CircuitTimes::from_bytes(array_ref![bytes, 0, 56])))
      },
      DataType::ErrorIndex => {
        if bytes.len() != 10 {
          return Err(Error::InvalidFormat("array length is not 10".to_string()));
        }

        let errors = bytes.iter().copied().take_while(|&b| b != 0).collect();
        Value::ByteArray(errors)
      },
      DataType::Error => {
        if bytes.len() != 9 {
          return Err(Error::InvalidFormat("array length is not 9".to_string()));
        }

        Value::Error(types::Error::from_bytes(array_ref![bytes, 0, 9])?)
      },
      DataType::String => {
        let end = bytes.iter().position(|&c| c == b'\0').unwrap_or(bytes.len());

        match String::from_utf8(bytes[..end].to_vec()) {
          Ok(s) => Value::String(s),
          Err(err) => return Err(Error::Utf8(err)),
        }
      },
      DataType::ByteArray => Value::ByteArray(bytes.to_vec()),
      t => {
        let mut n: i32 = 0;

        if let Some(bit_len) = self.bit_len {
          for i in 0..bit_len {
            let bit_pos = self.bit_pos + i - self.byte_pos * 8;

            let byte = bit_pos / 8;
            let bit = bit_pos % 8;
            let bit_mask = 0b10000000 >> bit;

            n <<= 1;

            if (bytes[byte] & bit_mask) != 0 {
              n |= 0b1;
            }
          }
        } else {
          #[allow(arithmetic_overflow)]
          match self.parameter {
            Parameter::IntHighByteFirst
            | Parameter::Int4HighByteFirst
            | Parameter::SIntHighByteFirst
            | Parameter::SInt4HighByteFirst => {
              for &b in bytes.iter().take(4) {
                n = (n << 8) | (b as i32);
              }
            },
            _ => {
              for &b in bytes.iter().rev().take(4) {
                n = (n << 8) | (b as i32);
              }
            },
          }
        }

        match t {
          data_type @ DataType::Byte => match &self.parameter {
            Parameter::SByte => Value::Int(n as u8 as i64),
            Parameter::Byte => Value::Int(n as u8 as i64),
            parameter => unreachable!("Data type {:?} with parameter {:?}.", data_type, parameter),
          },
          data_type @ DataType::Int => match &self.parameter {
            Parameter::Byte => Value::Int(n as i64),
            Parameter::Int | Parameter::IntHighByteFirst => Value::Int(n as i64),
            Parameter::Int4 | Parameter::Int4HighByteFirst => Value::Int(n as i64),
            Parameter::SByte => Value::Int(n as i64),
            Parameter::SInt | Parameter::SIntHighByteFirst => Value::Int(n as i64),
            Parameter::SInt4 | Parameter::SInt4HighByteFirst => Value::Int(n as i64),
            parameter => unreachable!("Data type {:?} with parameter {:?} ({:?}).", data_type, parameter, bytes),
          },
          data_type @ DataType::Double => match &self.parameter {
            Parameter::Byte => Value::Double(n as f64),
            Parameter::Int | Parameter::IntHighByteFirst => Value::Double(n as f64),
            Parameter::Int4 | Parameter::Int4HighByteFirst => Value::Double(n as f64),
            Parameter::SByte => Value::Double(n as i8 as f64),
            Parameter::SInt | Parameter::SIntHighByteFirst => Value::Double(n as i16 as f64),
            Parameter::SInt4 | Parameter::SInt4HighByteFirst => Value::Double(n as f64),
            parameter => unreachable!("Data type {:?} with parameter {:?}.", data_type, parameter),
          },
          _ => unreachable!(),
        }
      },
    };

    // Conversion must happen before bounds checking.
    if let Some(conversion) = &self.conversion {
      value = match value.convert(conversion) {
        Ok(value) => value,
        Err(err) => {
          log::warn!("Failed to convert 0x{:04X}: {err}", self.addr);
          err.value
        },
      };
    }

    if let Some(lower_bound) = self.lower_bound {
      value = match value {
        Value::Double(n) => {
          if n < lower_bound {
            Value::Empty
          } else {
            Value::Double(n)
          }
        },
        Value::Int(n) => {
          if (n as f64) < lower_bound {
            Value::Empty
          } else {
            Value::Int(n)
          }
        },
        value => value,
      };
    }

    if let Some(upper_bound) = self.upper_bound {
      value = match value {
        Value::Double(n) => {
          if n > upper_bound {
            Value::Empty
          } else {
            Value::Double(n)
          }
        },
        Value::Int(n) => {
          if (n as f64) > upper_bound {
            Value::Empty
          } else {
            Value::Int(n)
          }
        },
        value => value,
      };
    }

    Ok(value)
  }

  pub async fn get(&self, o: &mut Optolink, protocol: Protocol) -> Result<Value, Error> {
    log::trace!("Command::get(…)");

    if !self.mode.is_read() {
      return Err(Error::UnsupportedMode(format!("Address 0x{:04X} does not support reading.", self.addr)));
    }

    let mut buf = vec![0; self.block_len];
    if self.data_type == DataType::CircuitTimes {
      for (day, telegram) in buf.chunks_mut(DAY_LEN).enumerate() {
        protocol.get(o, self.addr + (day * DAY_LEN) as u16, telegram).await?;
      }
    } else {
      protocol.get(o, self.addr, &mut buf).await?;
    }

    let bytes = &buf[self.byte_pos..(self.byte_pos + self.byte_len)];

    self.deserialize(bytes)
  }

  pub async fn set(&self, o: &mut Optolink, protocol: Protocol, input: Value) -> Result<(), Error> {
    log::trace!("Command::set(…)");

    if !self.mode.is_write() {
      return Err(Error::UnsupportedMode(format!("Address 0x{:04X} does not support writing.", self.addr)));
    }

    let bytes = self.serialize(input)?;

    // Not atomic: if a day fails, the days before it are already written.
    if self.data_type == DataType::CircuitTimes {
      for (day, telegram) in bytes.chunks(DAY_LEN).enumerate() {
        protocol.set(o, self.addr + (day * DAY_LEN) as u16, telegram).await?;
      }
      return Ok(());
    }

    protocol.set(o, self.addr, &bytes).await.map_err(Into::into)
  }

  /// Serializes the command value to the bytes `set` writes.
  pub fn serialize(&self, mut input: Value) -> Result<Vec<u8>, Error> {
    match &input {
      Value::Int(n) => {
        if let Some(lower_bound) = self.lower_bound
          && (*n as f64) < lower_bound
        {
          return Err(Error::InvalidArgument(format!("{} is less than minimum {}.", n, lower_bound)));
        }

        if let Some(upper_bound) = self.upper_bound
          && (*n as f64) > upper_bound
        {
          return Err(Error::InvalidArgument(format!("{} is greater than maximum {}.", n, upper_bound)));
        }
      },
      Value::Double(n) => {
        if let Some(lower_bound) = self.lower_bound
          && *n < lower_bound
        {
          return Err(Error::InvalidArgument(format!("{} is less than minimum {}", n, lower_bound)));
        }

        if let Some(upper_bound) = self.upper_bound
          && *n > upper_bound
        {
          return Err(Error::InvalidArgument(format!("{} is greater than maximum {}", n, upper_bound)));
        }
      },
      _ => (),
    }

    if let Some(conversion) = &self.conversion {
      input = input.convert_back(conversion).map_err(|err| Error::InvalidArgument(err.to_string()))?;
    }

    if self.data_type == DataType::Int
      && let Value::Double(n) = input
      && n.fract() == 0.0
    {
      input = Value::Int(n as i64);
    }

    if self.data_type == DataType::DateTime
      && let Value::String(ref s) = input
      && let Ok(date_time) = s.parse::<DateTime>()
    {
      input = Value::DateTime(date_time);
    }

    if self.data_type == DataType::Date
      && let Value::String(ref s) = input
      && let Ok(date) = s.parse::<Date>()
    {
      input = Value::Date(date);
    }

    Ok(match (self.data_type, input) {
      (DataType::Date, Value::Date(date)) => date.to_bytes().to_vec(),
      (DataType::DateTime, Value::DateTime(date_time)) => date_time.to_bytes().to_vec(),
      (DataType::CircuitTimes, Value::CircuitTimes(cycletimes)) => cycletimes.to_bytes().to_vec(),
      (DataType::ByteArray, Value::ByteArray(bytes)) => bytes.to_vec(),
      (DataType::String, Value::String(s)) => s.as_bytes().to_vec(),
      (DataType::Error, Value::Error(error)) => error.to_bytes().to_vec(),
      (DataType::Int | DataType::Byte, Value::Int(n)) => match &self.parameter {
        Parameter::Byte => (n as u8).to_le_bytes().to_vec(),
        Parameter::Int => (n as u16).to_le_bytes().to_vec(),
        Parameter::IntHighByteFirst => (n as u16).to_be_bytes().to_vec(),
        Parameter::Int4 => (n as u32).to_le_bytes().to_vec(),
        Parameter::Int4HighByteFirst => (n as u32).to_be_bytes().to_vec(),
        Parameter::SByte => (n as i8).to_le_bytes().to_vec(),
        Parameter::SInt => (n as i16).to_le_bytes().to_vec(),
        Parameter::SIntHighByteFirst => (n as i16).to_be_bytes().to_vec(),
        Parameter::SInt4 => (n as i32).to_le_bytes().to_vec(),
        Parameter::SInt4HighByteFirst => (n as i32).to_be_bytes().to_vec(),
        parameter => {
          return Err(Error::InvalidArgument(format!("cannot write {:?} as {:?}", self.data_type, parameter)));
        },
      },
      (DataType::Double, Value::Double(n)) => match &self.parameter {
        Parameter::Byte => (n as u8).to_le_bytes().to_vec(),
        Parameter::Int => (n as u16).to_le_bytes().to_vec(),
        Parameter::IntHighByteFirst => (n as u16).to_be_bytes().to_vec(),
        Parameter::Int4 => (n as u32).to_le_bytes().to_vec(),
        Parameter::Int4HighByteFirst => (n as u32).to_be_bytes().to_vec(),
        Parameter::SByte => (n as i8).to_le_bytes().to_vec(),
        Parameter::SInt => (n as i16).to_le_bytes().to_vec(),
        Parameter::SIntHighByteFirst => (n as i16).to_be_bytes().to_vec(),
        Parameter::SInt4 => (n as i32).to_le_bytes().to_vec(),
        Parameter::SInt4HighByteFirst => (n as i32).to_be_bytes().to_vec(),
        parameter => {
          return Err(Error::InvalidArgument(format!("cannot write {:?} as {:?}", self.data_type, parameter)));
        },
      },
      (data_type, input) => return Err(Error::InvalidArgument(format!("expected {:?}, got {:?}", data_type, input))),
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parse_double_from_int() {
    let command = Command {
      addr: 0x0886,
      mode: AccessMode::Read,
      data_type: DataType::Double,
      parameter: Parameter::Int,
      block_count: None,
      block_len: 4,
      byte_len: 4,
      byte_pos: 0,
      bit_len: None,
      bit_pos: 0,
      conversion: Some(Conversion::SecToHour),
      lower_bound: None,
      upper_bound: None,
      unit: Some("h"),
      mapping: None,
    };
    let value = command.parse_value(&[0x00, 0x95, 0xBA, 0x0A]).unwrap();
    assert_eq!(value, Value::Double(50000.0));
  }

  #[test]
  fn serialize_irreversible_conversion_is_an_error() {
    let command = Command {
      addr: 0x0886,
      mode: AccessMode::ReadWrite,
      data_type: DataType::Double,
      parameter: Parameter::Int,
      block_count: None,
      block_len: 4,
      byte_len: 4,
      byte_pos: 0,
      bit_len: None,
      bit_pos: 0,
      conversion: Some(Conversion::SecToHour),
      lower_bound: None,
      upper_bound: None,
      unit: Some("h"),
      mapping: None,
    };

    assert!(matches!(command.serialize(Value::Double(1.0)), Err(Error::InvalidArgument(_))));
  }

  // `Ecotronic_0D00` specifies `RotateBytes` for `Int`, which doesn't seem to do anything.
  #[test]
  fn parse_rotate_bytes_int() {
    let command = Command {
      addr: 0x0D00,
      mode: AccessMode::Read,
      data_type: DataType::Int,
      parameter: Parameter::Int,
      block_count: None,
      block_len: 2,
      byte_len: 2,
      byte_pos: 0,
      bit_len: None,
      bit_pos: 0,
      conversion: Some(Conversion::RotateBytes),
      lower_bound: None,
      upper_bound: None,
      unit: None,
      mapping: None,
    };

    let value = command.parse_value(&[0xb3, 0x04]).unwrap();
    assert_eq!(value, Value::Int(1203));
  }

  // Ensure `Ecotronic_Gemischte_AT` is cast to a negative number properly.
  #[test]
  fn parse_negative_s_int() {
    let command = Command {
      addr: 21801,
      mode: AccessMode::Read,
      data_type: DataType::Double,
      parameter: Parameter::SInt,
      block_count: None,
      block_len: 2,
      byte_len: 2,
      byte_pos: 0,
      bit_len: None,
      bit_pos: 0,
      conversion: Some(Conversion::Div10),
      lower_bound: Some(-60.0),
      upper_bound: Some(60.0),
      unit: Some("°C"),
      mapping: None,
    };

    let value = command.parse_value(&[0xfd, 0xff]).unwrap();
    assert_eq!(value, Value::Double(-0.3));
  }

  /// One Zeitphase per day, from (5 + day):00 to 22:00.
  fn week() -> [u8; 56] {
    std::array::from_fn(|i| match i % 8 {
      0 => (i as u8 / 8 + 5) << 3,
      1 => 22 << 3,
      _ => 0xff,
    })
  }

  /// A VS1 controller on TCP with `week()` at 0x2000 of memory at 0x2000. Records every telegram as (function, address, data)
  /// and drops the connection at `fail_at`.
  async fn fake_controller(fail_at: Option<u16>) -> (Optolink, tokio::task::JoinHandle<Vec<(u8, u16, Vec<u8>)>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
      let (mut stream, _) = listener.accept().await.unwrap();
      let mut memory = week().to_vec();
      let mut telegrams = vec![];
      let mut buf = vec![];
      let mut chunk = [0; 64];
      loop {
        match stream.read(&mut chunk).await {
          Ok(0) | Err(_) => return telegrams,
          Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
        loop {
          let (function, addr, len) = match buf[..] {
            [0x04, ..] => {
              buf.remove(0);
              stream.write_all(&[0x05]).await.unwrap();
              continue;
            },
            [0x01, f @ (0xF7 | 0xF4), hi, lo, len, ..] => (f, u16::from_be_bytes([hi, lo]), len as usize),
            _ => break,
          };
          let size = if function == 0xF4 { 5 + len } else { 5 };
          if buf.len() < size {
            break;
          }
          let data: Vec<u8> = buf.drain(..size).skip(5).collect();
          telegrams.push((function, addr, if function == 0xF4 { data.clone() } else { vec![len as u8] }));
          if Some(addr) == fail_at {
            return telegrams;
          }
          let at = (addr - 0x2000) as usize;
          if function == 0xF4 {
            memory[at..at + len].copy_from_slice(&data);
            stream.write_all(&[0x00]).await.unwrap();
          } else {
            stream.write_all(&memory[at..at + len]).await.unwrap();
          }
        }
      }
    });
    (Optolink::connect(("127.0.0.1", port)).await.unwrap(), server)
  }

  #[tokio::test]
  async fn zeitprogramm_is_read_one_day_per_telegram() {
    let command = crate::device::V200KO2B.command("Schaltzeiten_A1M1_HK").unwrap();
    let (mut optolink, server) = fake_controller(None).await;

    let value = command.get(&mut optolink, Protocol::Vs1).await.unwrap();
    drop(optolink);

    let telegrams = server.await.unwrap();
    let expected: Vec<_> = (0..7).map(|day| (0xF7, 0x2000 + 8 * day, vec![8])).collect();
    assert_eq!(telegrams, expected);
    let Value::CircuitTimes(week) = value else { panic!("{value:?}") };
    assert_eq!(week.to_bytes(), self::week());
  }

  #[tokio::test]
  async fn zeitprogramm_is_written_one_day_per_telegram() {
    let command = crate::device::V200KO2B.command("Schaltzeiten_A1M1_HK").unwrap();
    let mut bytes = week();
    bytes[1] = 23 << 3;
    let (mut optolink, server) = fake_controller(None).await;

    let week = Value::CircuitTimes(Box::new(CircuitTimes::from_bytes(&bytes)));
    command.set(&mut optolink, Protocol::Vs1, week).await.unwrap();
    drop(optolink);

    let expected: Vec<_> =
      (0..7).map(|day| (0xF4, 0x2000 + 8 * day as u16, bytes[day * 8..day * 8 + 8].to_vec())).collect();
    assert_eq!(server.await.unwrap(), expected);
  }

  #[tokio::test]
  async fn zeitprogramm_transfer_stops_at_the_failing_day() {
    let command = crate::device::V200KO2B.command("Schaltzeiten_A1M1_HK").unwrap();

    let (mut optolink, server) = fake_controller(Some(0x2018)).await;
    assert!(command.get(&mut optolink, Protocol::Vs1).await.is_err());
    drop(optolink);
    let addrs: Vec<u16> = server.await.unwrap().iter().map(|(_, addr, _)| *addr).collect();
    assert_eq!(addrs, [0x2000, 0x2008, 0x2010, 0x2018]);

    let (mut optolink, server) = fake_controller(Some(0x2018)).await;
    let week = Value::CircuitTimes(Box::new(CircuitTimes::from_bytes(&[0xff; 56])));
    assert!(command.set(&mut optolink, Protocol::Vs1, week).await.is_err());
    drop(optolink);
    let addrs: Vec<u16> = server.await.unwrap().iter().map(|(_, addr, _)| *addr).collect();
    assert_eq!(addrs, [0x2000, 0x2008, 0x2010, 0x2018]);
  }
}
