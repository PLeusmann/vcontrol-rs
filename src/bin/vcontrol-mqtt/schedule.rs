//! Decides which Command is read next and when the Optolink link counts as lost. No I/O.

use std::time::Duration;

use tokio::time::Instant;

/// This many consecutive failures, not all of the same Command, mean the Optolink link is lost.
const MAX_FAILURES: usize = 3;

/// When each Command is read next.
pub struct Schedule {
  intervals: Vec<Duration>,
  due: Vec<Instant>,
}

impl Schedule {
  /// Everything is due at `now`.
  pub fn new(intervals: Vec<Duration>, now: Instant) -> Self {
    Self { due: vec![now; intervals.len()], intervals }
  }

  /// The Command to read next and when. With several due at the same time, the first one configured.
  pub fn next(&self) -> (usize, Instant) {
    self.due.iter().copied().enumerate().min_by_key(|(_, due)| *due).expect("has commands")
  }

  /// A read of the Command started at `now`. It is due again one interval later, whether the read succeeds or fails,
  /// so a failing Command never spins.
  pub fn started(&mut self, index: usize, now: Instant) {
    self.due[index] = now + self.intervals[index];
  }

  pub fn all_due(&mut self, now: Instant) {
    self.due.fill(now);
  }
}

/// The Commands of the failed controller calls since the last success.
#[derive(Default)]
pub struct Failures(Vec<String>);

impl Failures {
  pub fn success(&mut self) {
    self.0.clear();
  }

  /// Records a failed call. Returns whether the link is lost. A single Command failing on its own never is:
  /// on one error it cannot be told apart from a lost link.
  pub fn failure(&mut self, command: &str) -> bool {
    self.0.push(command.to_owned());
    self.0.len() >= MAX_FAILURES && self.0.iter().any(|failed| failed != command)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const S: Duration = Duration::from_secs(1);

  #[test]
  fn reads_the_earliest_due_command_first() {
    let start = Instant::now();
    let mut schedule = Schedule::new(vec![60 * S, 10 * S, 3600 * S], start);

    // All due at once: in configured order.
    for index in 0..3 {
      assert_eq!(schedule.next(), (index, start));
      schedule.started(index, start);
    }

    assert_eq!(schedule.next(), (1, start + 10 * S));
    schedule.started(1, start + 10 * S);
    assert_eq!(schedule.next(), (1, start + 20 * S));

    // A late read is rescheduled from when it started.
    schedule.started(1, start + 59 * S);
    assert_eq!(schedule.next(), (0, start + 60 * S));
    schedule.started(0, start + 60 * S);
    assert_eq!(schedule.next(), (1, start + 69 * S));

    schedule.all_due(start + 61 * S);
    assert_eq!(schedule.next(), (0, start + 61 * S));
  }

  #[test]
  fn link_is_lost_after_three_consecutive_failures_of_different_commands() {
    let mut failures = Failures::default();
    assert!(!failures.failure("a"));
    assert!(!failures.failure("b"));
    assert!(failures.failure("c"));

    let mut failures = Failures::default();
    assert!(!failures.failure("a"));
    assert!(!failures.failure("a"));
    assert!(failures.failure("b"));
  }

  #[test]
  fn one_command_failing_alone_is_not_a_lost_link() {
    let mut failures = Failures::default();
    for _ in 0..100 {
      assert!(!failures.failure("a"));
    }
  }

  #[test]
  fn a_success_starts_the_count_again() {
    let mut failures = Failures::default();
    for _ in 0..100 {
      assert!(!failures.failure("a"));
      assert!(!failures.failure("b"));
      failures.success();
    }
  }
}
