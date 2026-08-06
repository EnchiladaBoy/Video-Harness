//! Native GTK/libadwaita frontend for Video Harness.

pub mod cloud_cinema;
pub mod composer_state;
mod window;

use std::fmt;
use std::sync::Arc;

use adw::prelude::*;

pub const APPLICATION_ID: &str = "io.github.EnchiladaBoy.VideoHarness";

#[derive(Debug)]
pub enum GuiError {
    Runtime(std::io::Error),
}

impl fmt::Display for GuiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Runtime(error) => write!(formatter, "could not start the async runtime: {error}"),
        }
    }
}

impl std::error::Error for GuiError {}

/// Run the native application until its final window closes.
pub fn run() -> Result<(), GuiError> {
    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("video-harness-worker")
            .build()
            .map_err(GuiError::Runtime)?,
    );
    let application = adw::Application::builder()
        .application_id(APPLICATION_ID)
        .build();

    application.connect_startup(|_| window::install_style());
    application.connect_activate(move |application| {
        if let Some(window) = application.active_window() {
            window.present();
            return;
        }
        if let Err(error) = window::present(application, Arc::clone(&runtime)) {
            window::present_startup_error(application, &error.to_string());
        }
    });
    application.run();
    Ok(())
}
