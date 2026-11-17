//! Enrollment is independent of an installer or UI. Secrets cross the API as
//! owned descriptors; diagnostics and return values contain slot identities only.
#[cfg(target_os = "linux")]
pub mod linux;
pub mod pcr;
#[cfg(target_os = "linux")]
pub use linux::*;
