//! Native backend and GTK frontend for Video Harness.

pub mod api;
pub mod config;
pub mod credentials;
pub mod domain;
pub mod gui;
pub mod gui_state;
pub mod history;
pub mod migration;
pub mod providers;
pub mod workflow;

pub use config::AppPaths;
pub use workflow::{ServiceCommand, ServiceConfig, ServiceEvent, ServiceHandle, spawn_service};
