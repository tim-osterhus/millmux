pub mod bootstrap;
pub mod doctor;
pub mod reconcile;
pub mod registry;
pub mod server;
pub mod stop;
pub mod worker_launcher;

pub fn binary_name() -> &'static str {
    "millrace-sessiond"
}

pub async fn run_foreground() -> Result<(), server::HostServerError> {
    server::run_foreground().await
}
