#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    match (args.next().as_deref(), args.next()) {
        (Some("--foreground"), None) => {
            if let Err(error) = millrace_sessions_host::run_foreground().await {
                eprintln!("{}: {error}", millrace_sessions_host::binary_name());
                std::process::exit(1);
            }
        }
        _ => {
            eprintln!(
                "usage: {} --foreground",
                millrace_sessions_host::binary_name()
            );
            std::process::exit(2);
        }
    }
}
