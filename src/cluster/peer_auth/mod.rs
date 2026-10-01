//! Node-local identities and exact-byte authenticated peer transport.
//! Membership supplies trust from a committed Store snapshot and rechecks it at
//! mutation commit; this module deliberately has no independent authority cache.
mod identity;
mod transport;
mod wire;

pub use identity::*;
pub use transport::*;
pub use wire::*;

use std::io;

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn denied() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "节点签名或权限无效")
}

#[cfg(test)]
mod tests;
