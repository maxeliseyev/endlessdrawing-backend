//! Backend foundation for the shared infinite canvas.

pub mod admin;
pub mod app;
pub mod config;
pub mod db;
pub mod domain;
pub mod realtime;
pub mod session;

mod protocol;
mod realtime_settings;
mod snapshot;
mod spawn;
mod storage;
mod ws;
