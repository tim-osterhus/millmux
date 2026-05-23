use std::{path::PathBuf, str::FromStr};

use millrace_sessions_core::ids::SessionId;

fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(error) => {
            eprintln!("{}: {error}", millrace_sessions_worker::binary_name());
            eprintln!(
                "usage: {} --session-id ID --state-dir PATH",
                millrace_sessions_worker::binary_name()
            );
            std::process::exit(2);
        }
    };

    if let Err(error) = millrace_sessions_worker::run_worker(args.session_id, args.state_dir) {
        eprintln!("{}: {error}", millrace_sessions_worker::binary_name());
        std::process::exit(1);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkerArgs {
    session_id: SessionId,
    state_dir: PathBuf,
}

fn parse_args() -> Result<WorkerArgs, String> {
    let mut args = std::env::args().skip(1);
    let mut session_id = None;
    let mut state_dir = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--session-id" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--session-id requires a value".to_string())?;
                session_id = Some(
                    SessionId::from_str(&value)
                        .map_err(|error| format!("invalid --session-id: {error}"))?,
                );
            }
            "--state-dir" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--state-dir requires a value".to_string())?;
                state_dir = Some(PathBuf::from(value));
            }
            other => return Err(format!("unsupported argument: {other}")),
        }
    }

    Ok(WorkerArgs {
        session_id: session_id.ok_or_else(|| "--session-id is required".to_string())?,
        state_dir: state_dir.ok_or_else(|| "--state-dir is required".to_string())?,
    })
}
