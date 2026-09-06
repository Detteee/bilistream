//! Multi-node cluster: election, heartbeat, fencing, and config sync.
//!
//! Submodules are layered so heartbeat can call sync, but sync never calls heartbeat.

mod election;
mod fencing;
mod heartbeat;
mod self_check;
mod state;
mod status;
mod sync;
mod types;
mod version;

pub(crate) use election::*;
pub use fencing::*;
pub use heartbeat::*;
pub(crate) use self_check::{self_check_reply, SELF_CHECK_API_PATH};
pub use self_check::{
    SelfCheckFailure, SelfCheckReply, SelfCheckRequest, SelfCheckState, SelfCheckStatus,
};
pub(crate) use state::*;
pub use status::*;
pub use sync::*;
pub use types::*;
pub use version::*;

#[cfg(test)]
mod election_tests;
#[cfg(test)]
pub(crate) mod tests;
