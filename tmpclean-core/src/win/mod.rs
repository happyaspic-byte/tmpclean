//! Windows filesystem operations anchored to owned, no-follow handles.

mod fs;

pub use fs::{WinFs, WinHandle};
