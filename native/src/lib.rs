//! Native backend and terminal UI for OpenRouter Video Studio.

pub mod api;
pub mod app;
pub mod config;
pub mod credentials;
pub mod domain;
pub mod history;
pub mod providers;
pub mod ui;
pub mod ui_input;
pub mod workflow;

pub use config::AppPaths;
pub use workflow::{ServiceCommand, ServiceConfig, ServiceEvent, ServiceHandle, spawn_service};
