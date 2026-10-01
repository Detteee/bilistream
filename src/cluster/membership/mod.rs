//! Blocking, per-operation membership transactions. Store commits precede every
//! acknowledgement; unknown decisions never release an execution hold.
mod driver;
mod engine;
mod types;

pub use driver::*;
pub use engine::*;
pub use types::*;

use serde::{de::DeserializeOwned, Serialize};
use std::io;

pub(crate) fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
pub(crate) fn conflict(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::WouldBlock, message)
}
pub(crate) fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "节点不具备此成员操作的权限",
    )
}
pub(crate) fn encode<T: Serialize>(value: &T) -> io::Result<Vec<u8>> {
    // Protocol v1 uses fixed-order structs and sorted collections only. No
    // untyped JSON maps or floating point values enter signed records.
    serde_json::to_vec(value).map_err(|_| invalid("成员操作编码失败"))
}
pub(crate) fn decode<T: DeserializeOwned>(value: serde_json::Value) -> io::Result<T> {
    serde_json::from_value(value).map_err(|_| invalid("成员操作记录损坏或版本不兼容"))
}

#[cfg(test)]
mod tests;
