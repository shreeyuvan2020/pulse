//! Pure logic shared by the Pulse client and server: the `pulse.event.v1`
//! event model, the rules that turn events into credited seconds, screen
//! activity scoring and the evidence hash chain. Nothing in this crate
//! touches the file system, the clock or the network.

pub mod activity;
mod attribution;
pub mod chain;
mod event;

pub use attribution::{Params, ProjectTotals, Report, attribute, engaged_projects};
pub use event::{Event, EventKind, PROTOCOL_VERSION, TERMINAL_SOURCE, source_is_agent};
