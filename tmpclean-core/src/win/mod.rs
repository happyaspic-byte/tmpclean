//! Windows filesystem operations anchored to owned, no-follow handles.

mod fs;
pub mod resolve;

pub use fs::{WinFs, WinHandle};
