pub mod check;
pub mod detect_failure;
pub mod launch;

pub use check::{AuthStatus, check_auth};
pub use detect_failure::{format_auth_failure_message, is_auth_failure, is_auth_failure_outcome};
pub use launch::{LaunchOutcome, launch_login, launch_terminal_login};
