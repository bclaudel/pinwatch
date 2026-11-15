//! GPU connector telemetry. Hardware writes are confined to the active policy executor.
#![forbid(unsafe_code)]
pub mod backend;
pub mod config;
pub mod daemon;
pub mod ipc;
pub mod model;
pub mod notify;
pub mod policy;
pub mod sensor;
