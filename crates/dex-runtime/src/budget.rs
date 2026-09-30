//! Execution budgets.
//!
//! These are the authoritative resource boundary for a program run. They live
//! at the crate root rather than under `capability` because both the capability
//! layer (which spends them) and the script runtime (which enforces the
//! wall-clock half) need them, and because a budget is a property of a run
//! rather than of any one subsystem.
//!
//! Rune exposes no instruction counter, so there is no "operations" field here.
//! The three that do the work are the wall-clock deadline, the capability
//! invocation count, and the byte budgets; a program that loops forever without
//! awaiting is stopped by the deadline.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dex_protocol::CapabilityErrorKind;

/// The limits one program execution runs under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecutionBudget {
    /// Wall-clock ceiling for the whole program.
    pub wall_clock: Duration,
    /// Scripting-language instructions the program may execute.
    ///
    /// Enforced by the script runtime's own instruction budget, so a program
    /// that computes forever is stopped mid-flight rather than being left to
    /// run until the wall clock. Native capability work is bounded separately by
    /// the call and byte budgets below, because a native function is not
    /// charged instructions.
    pub instructions: u64,
    /// Capability invocations permitted.
    pub capability_calls: u32,
    /// Filesystem bytes readable.
    pub read_bytes: u64,
    /// Filesystem bytes writable.
    pub write_bytes: u64,
    /// Output bytes retained from any single capability.
    pub output_bytes: u64,
    /// Wall clock for a single spawned process.
    pub command: Duration,
}

impl Default for ExecutionBudget {
    fn default() -> Self {
        Self {
            wall_clock: Duration::from_secs(30),
            instructions: 20_000_000,
            capability_calls: 100,
            read_bytes: 32 * 1024 * 1024,
            write_bytes: 8 * 1024 * 1024,
            output_bytes: 256 * 1024,
            command: Duration::from_secs(120),
        }
    }
}

impl ExecutionBudget {
    /// Human-readable form for the CLI banner and for `dex.capabilities()`.
    pub fn describe(&self) -> String {
        format!(
            "wall_clock={}ms instructions={} capability_calls={} read={}B write={}B output={}B command={}ms",
            self.wall_clock.as_millis(),
            self.instructions,
            self.capability_calls,
            self.read_bytes,
            self.write_bytes,
            self.output_bytes,
            self.command.as_millis(),
        )
    }
}

/// Mutable spend counters for one program run.
///
/// Cloneable and cheap: every capability call receives a handle, and the
/// counters are shared atomics rather than anything that would pin the budget
/// to a thread.
#[derive(Clone, Debug)]
pub struct BudgetMeter {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    limits: ExecutionBudget,
    started: Instant,
    capability_calls: AtomicU64,
    read_bytes: AtomicU64,
    write_bytes: AtomicU64,
}

impl BudgetMeter {
    pub fn new(limits: ExecutionBudget) -> Self {
        Self {
            inner: Arc::new(Inner {
                limits,
                started: Instant::now(),
                capability_calls: AtomicU64::new(0),
                read_bytes: AtomicU64::new(0),
                write_bytes: AtomicU64::new(0),
            }),
        }
    }

    pub fn limits(&self) -> &ExecutionBudget {
        &self.inner.limits
    }

    pub fn elapsed(&self) -> Duration {
        self.inner.started.elapsed()
    }

    /// Whether the wall-clock deadline has passed. Checked on every capability
    /// entry so a program that awaits between operations stops promptly.
    pub fn deadline_exceeded(&self) -> bool {
        self.elapsed() >= self.inner.limits.wall_clock
    }

    /// Record one capability invocation. Called before any work happens, so a
    /// denied or over-budget call still costs a slot: otherwise a program could
    /// spin on a failing capability for free.
    pub fn charge_capability(&self) -> Result<(), BudgetError> {
        let spent = self
            .inner
            .capability_calls
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        if spent > u64::from(self.inner.limits.capability_calls) {
            return Err(BudgetError::CapabilityCalls {
                limit: self.inner.limits.capability_calls,
            });
        }
        if self.deadline_exceeded() {
            return Err(BudgetError::WallClock {
                limit: self.inner.limits.wall_clock,
            });
        }
        Ok(())
    }

    pub fn charge_read(&self, bytes: u64) -> Result<(), BudgetError> {
        let spent = self.inner.read_bytes.fetch_add(bytes, Ordering::Relaxed) + bytes;
        if spent > self.inner.limits.read_bytes {
            return Err(BudgetError::ReadBytes {
                limit: self.inner.limits.read_bytes,
            });
        }
        Ok(())
    }

    pub fn charge_write(&self, bytes: u64) -> Result<(), BudgetError> {
        let spent = self.inner.write_bytes.fetch_add(bytes, Ordering::Relaxed) + bytes;
        if spent > self.inner.limits.write_bytes {
            return Err(BudgetError::WriteBytes {
                limit: self.inner.limits.write_bytes,
            });
        }
        Ok(())
    }

    /// Clamp a captured output buffer to the per-capability limit. The second
    /// element of the result reports whether anything was dropped, so callers
    /// can mark the result truncated rather than silently losing the tail.
    pub fn clamp_output(&self, bytes: &mut Vec<u8>) -> bool {
        let limit = self.inner.limits.output_bytes as usize;
        if bytes.len() <= limit {
            return false;
        }
        bytes.truncate(limit);
        true
    }

    pub fn usage(&self) -> BudgetUsage {
        BudgetUsage {
            capability_calls: self.inner.capability_calls.load(Ordering::Relaxed),
            read_bytes: self.inner.read_bytes.load(Ordering::Relaxed),
            write_bytes: self.inner.write_bytes.load(Ordering::Relaxed),
            elapsed: self.elapsed(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BudgetUsage {
    pub capability_calls: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub elapsed: Duration,
}

/// A budget refusal. Every variant surfaces to the program as
/// `BudgetExceeded` with a message naming the limit, so the model can adapt
/// rather than merely retry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BudgetError {
    WallClock { limit: Duration },
    CapabilityCalls { limit: u32 },
    ReadBytes { limit: u64 },
    WriteBytes { limit: u64 },
}

impl std::fmt::Display for BudgetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BudgetError::WallClock { limit } => {
                write!(f, "wall-clock budget of {}ms exhausted", limit.as_millis())
            }
            BudgetError::CapabilityCalls { limit } => {
                write!(f, "capability invocation budget of {limit} exhausted")
            }
            BudgetError::ReadBytes { limit } => {
                write!(f, "filesystem read budget of {limit} bytes exhausted")
            }
            BudgetError::WriteBytes { limit } => {
                write!(f, "filesystem write budget of {limit} bytes exhausted")
            }
        }
    }
}

impl From<BudgetError> for CapabilityErrorKind {
    fn from(_: BudgetError) -> Self {
        CapabilityErrorKind::BudgetExceeded
    }
}

impl std::error::Error for BudgetError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_calls_are_capped() {
        let meter = BudgetMeter::new(ExecutionBudget {
            capability_calls: 2,
            ..ExecutionBudget::default()
        });
        assert!(meter.charge_capability().is_ok());
        assert!(meter.charge_capability().is_ok());
        assert_eq!(
            meter.charge_capability(),
            Err(BudgetError::CapabilityCalls { limit: 2 })
        );
    }

    #[test]
    fn a_denied_call_still_costs_a_slot() {
        // Charges happen before authorization, so a program cannot spin on a
        // failing capability for free.
        let meter = BudgetMeter::new(ExecutionBudget {
            capability_calls: 1,
            ..ExecutionBudget::default()
        });
        assert!(meter.charge_capability().is_ok());
        assert!(meter.charge_capability().is_err());
    }

    #[test]
    fn read_and_write_are_budgeted_separately() {
        let meter = BudgetMeter::new(ExecutionBudget {
            read_bytes: 10,
            write_bytes: 5,
            ..ExecutionBudget::default()
        });
        assert!(meter.charge_read(10).is_ok());
        assert_eq!(meter.charge_read(1), Err(BudgetError::ReadBytes { limit: 10 }));
        // A read overrun must not consume the write allowance.
        assert!(meter.charge_write(5).is_ok());
        assert_eq!(meter.charge_write(1), Err(BudgetError::WriteBytes { limit: 5 }));
    }

    #[test]
    fn output_is_clamped_and_reports_truncation() {
        let meter = BudgetMeter::new(ExecutionBudget {
            output_bytes: 4,
            ..ExecutionBudget::default()
        });
        let mut within = b"abc".to_vec();
        assert!(!meter.clamp_output(&mut within));
        assert_eq!(within, b"abc");

        let mut over = b"abcdefgh".to_vec();
        assert!(meter.clamp_output(&mut over));
        assert_eq!(over, b"abcd");
    }

    #[test]
    fn a_passed_deadline_is_reported() {
        let meter = BudgetMeter::new(ExecutionBudget {
            wall_clock: Duration::from_millis(0),
            ..ExecutionBudget::default()
        });
        assert!(meter.deadline_exceeded());
    }

    #[test]
    fn the_meter_is_shared_across_clones() {
        let meter = BudgetMeter::new(ExecutionBudget {
            capability_calls: 10,
            ..ExecutionBudget::default()
        });
        let clone = meter.clone();
        meter.charge_capability().unwrap();
        clone.charge_capability().unwrap();
        assert_eq!(meter.usage().capability_calls, 2);
    }
}
