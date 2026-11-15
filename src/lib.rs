//! Linux support for the EVGA Z790 CLASSIFIED's independent LED and fan controllers.
//!
//! Hardware access is Linux-specific. Protocol and validation tests run without devices.
#![cfg(target_os = "linux")]

pub mod fan;
pub mod led;
