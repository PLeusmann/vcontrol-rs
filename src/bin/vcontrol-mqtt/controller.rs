//! The Optolink side of the Bridge. Every call has a timeout.

use std::{future::Future, time::Duration};

use tokio::time::timeout;
use vcontrol::{Device, Optolink, VControl, Value};

use crate::config::OptolinkConfig;

/// A `get` or `set` that takes longer than this has failed.
pub const TIMEOUT: Duration = Duration::from_secs(5);

/// Connecting detects the protocol, which waits several times for the controller's sync byte (sent about every 2 s),
/// so it needs longer than a single call.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug)]
pub enum Failure {
  Timeout,
  Error(String),
}

pub trait Controller {
  fn device(&self) -> &'static Device;
  async fn get(&mut self, command: &str) -> Result<Value, Failure>;
  async fn set(&mut self, command: &str, value: Value) -> Result<(), Failure>;
}

pub async fn with_timeout<T, E: ToString>(
  limit: Duration,
  call: impl Future<Output = Result<T, E>>,
) -> Result<T, Failure> {
  match timeout(limit, call).await {
    Ok(result) => result.map_err(|err| Failure::Error(err.to_string())),
    Err(_) => Err(Failure::Timeout),
  }
}

/// The time a call may take. `VControl` renegotiates the protocol in the call after a failed one,
/// which takes as long as connecting; a single failure must not turn into a timeout.
pub fn limit(previous_call_failed: bool) -> Duration {
  if previous_call_failed { CONNECT_TIMEOUT } else { TIMEOUT }
}

/// The real controller.
pub struct Link {
  vcontrol: VControl,
  failed: bool,
}

impl Controller for Link {
  fn device(&self) -> &'static Device {
    self.vcontrol.device()
  }

  async fn get(&mut self, command: &str) -> Result<Value, Failure> {
    let result = with_timeout(limit(self.failed), self.vcontrol.get(command)).await;
    self.failed = result.is_err();
    result.map(|output| output.value)
  }

  async fn set(&mut self, command: &str, value: Value) -> Result<(), Failure> {
    let result = with_timeout(limit(self.failed), self.vcontrol.set(command, value)).await;
    self.failed = result.is_err();
    result
  }
}

/// Opens the Optolink link and detects the Device.
pub async fn connect(config: &OptolinkConfig) -> Result<Link, String> {
  let connect = async {
    let optolink = match config {
      OptolinkConfig::Device { device } => Optolink::open(device).await,
      OptolinkConfig::Tcp { host, port } => Optolink::connect((host.as_str(), *port)).await,
    }
    .map_err(|err| err.to_string())?;

    let vcontrol = VControl::connect(optolink).await.map_err(|err| err.to_string())?;
    Ok(Link { vcontrol, failed: false })
  };

  timeout(CONNECT_TIMEOUT, connect).await.unwrap_or_else(|_| Err("timed out".into()))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn call_after_a_failure_gets_time_to_renegotiate() {
    assert_eq!(limit(false), TIMEOUT);
    assert_eq!(limit(true), CONNECT_TIMEOUT);
    assert!(CONNECT_TIMEOUT > TIMEOUT);
  }

  #[tokio::test(start_paused = true)]
  async fn slow_call_is_a_timeout_and_an_error_is_not() {
    let hang = std::future::pending::<Result<(), String>>();
    assert!(matches!(with_timeout(TIMEOUT, hang).await, Err(Failure::Timeout)));
    assert!(matches!(with_timeout(TIMEOUT, async { Err::<(), _>("broken") }).await, Err(Failure::Error(_))));
    assert!(matches!(with_timeout(TIMEOUT, async { Ok::<_, String>(1) }).await, Ok(1)));
  }
}
