//! Session event fan-out.

pub mod bus;

pub use bus::{now_millis, EventSink, RuntimeBus};
