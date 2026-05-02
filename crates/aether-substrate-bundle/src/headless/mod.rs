//! Headless chassis: std-timer driven, no GPU, no window. Replies
//! `Err` to capture / window-mode / platform_info kinds — desktop-
//! only operations the headless deployment doesn't support.

pub mod chassis;
pub mod driver;

pub use chassis::{HeadlessChassis, HeadlessEnv};
