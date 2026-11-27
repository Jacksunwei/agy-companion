#![forbid(unsafe_code)]

mod common;

#[path = "cli_integration/cli_idle_shutdown.rs"]
mod cli_idle_shutdown;
#[path = "cli_integration/daemon_integration.rs"]
mod daemon_integration;
#[path = "cli_integration/install_lifecycle.rs"]
mod install_lifecycle;
#[path = "cli_integration/install_symlink_safety.rs"]
mod install_symlink_safety;
#[path = "cli_integration/serve_doctor.rs"]
mod serve_doctor;
