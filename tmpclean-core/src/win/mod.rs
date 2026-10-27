//! Windows filesystem operations anchored to owned, no-follow handles.

mod fs;
pub mod harden;
pub mod resolve;

pub use fs::{WinFs, WinHandle};
pub use harden::harden;
