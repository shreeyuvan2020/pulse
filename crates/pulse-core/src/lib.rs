//! Pure logic shared by the Pulse client and server: the `pulse.event.v1`
//! event model and the rules that turn events into credited seconds.
//! Nothing in this crate touches the file system, the clock or the network.

mod attribution;
mod event;

pub use attribution::{Params, ProjectTotals, Report, attribute, engaged_projects};
pub use event::{Event, EventKind, PROTOCOL_VERSION};
