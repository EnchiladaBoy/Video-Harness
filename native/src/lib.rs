//! Platform-neutral Video Harness engine and optional desktop frontends.

pub mod api;
mod atomic;
pub mod composer;
pub mod config;
pub mod credentials;
pub mod domain;
#[cfg(feature = "legacy-gtk")]
pub mod gui;
pub mod gui_state;
pub mod history;
pub mod migration;
pub mod providers;
pub mod workflow;

pub use config::AppPaths;
pub use workflow::{ServiceCommand, ServiceConfig, ServiceEvent, ServiceHandle, spawn_service};
