mod attach;
mod client;
mod cockpit;
mod commands;
mod console;
mod output;

use std::{
    io::{self, Write},
    time::Duration,
};

use clap::Parser;
use commands::{Cli, CliCommand};
use millrace_sessions_core::paths::state_paths;
use millrace_sessions_core::protocol::{EventStreamFrame, LogStreamFrame};
use thiserror::Error;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("millmux: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), MillmuxCliError> {
    let cli = Cli::parse();

    if let Some(command) = cli.command.unsupported_name() {
        return Err(MillmuxCliError::Unsupported(command));
    }

    match cli.command {
        CliCommand::Doctor(args) => {
            let request = args.request()?;
            let result = match client::SessionControlClient::new() {
                Ok(client) => {
                    match tokio::time::timeout(Duration::from_millis(200), client.doctor(&request))
                        .await
                    {
                        Ok(Ok(result)) => result,
                        _ => {
                            let paths = state_paths()?;
                            millrace_sessions_host::doctor::run_doctor(&paths, None, &request)?
                        }
                    }
                }
                Err(_) => {
                    let paths = state_paths()?;
                    millrace_sessions_host::doctor::run_doctor(&paths, None, &request)?
                }
            };
            print!(
                "{}",
                if args.json {
                    output::render_json(&result)?
                } else {
                    output::render_doctor(&result)
                }
            );
            return Ok(());
        }
        CliCommand::Start(args) => {
            let client = ready_client().await?;
            let request = args.request()?;
            let result = client.start(&request).await?;
            print!(
                "{}",
                if args.json {
                    output::render_json(&result)?
                } else {
                    output::render_start(&result)
                }
            );
        }
        CliCommand::List(args) => {
            let client = ready_client().await?;
            let result = client.list(&args.request()).await?;
            print!(
                "{}",
                if args.json {
                    output::render_json(&result)?
                } else {
                    output::render_list(&result)
                }
            );
        }
        CliCommand::Status(args) => {
            let client = ready_client().await?;
            if let Some(selector) = args.selector.optional()? {
                let result = client
                    .inspect(&millrace_sessions_core::protocol::SessionInspectRequest { selector })
                    .await?;
                print!(
                    "{}",
                    if args.json {
                        output::render_json(&result)?
                    } else {
                        output::render_session_status(&result)
                    }
                );
            } else {
                let result = client.host_status().await?;
                print!(
                    "{}",
                    if args.json {
                        output::render_json(&result)?
                    } else {
                        output::render_host_status(&result)
                    }
                );
            }
        }
        CliCommand::Inspect(args) => {
            let client = ready_client().await?;
            let result = client.inspect(&args.request()?).await?;
            print!(
                "{}",
                if args.json {
                    output::render_json(&result)?
                } else {
                    output::render_inspect(&result)
                }
            );
        }
        CliCommand::Logs(args) => {
            let client = ready_client().await?;
            let request = args.request()?;
            if args.follow {
                let (result, mut reader) = client.logs_follow(&request).await?.split();
                write_stdout(if args.json {
                    output::render_json(&result)?
                } else {
                    output::render_logs(&result)
                })?;
                while let Some(frame) = reader.next_frame().await? {
                    let closed = matches!(frame, LogStreamFrame::Closed);
                    write_stdout(if args.json {
                        output::render_json(&frame)?
                    } else {
                        output::render_log_stream_frame(&frame)
                    })?;
                    if closed {
                        break;
                    }
                }
            } else {
                let result = client.logs(&request).await?;
                print!(
                    "{}",
                    if args.json {
                        output::render_json(&result)?
                    } else {
                        output::render_logs(&result)
                    }
                );
            }
        }
        CliCommand::Events(args) => {
            let client = ready_client().await?;
            let request = args.request()?;
            if args.follow {
                let (result, mut reader) = client.events_follow(&request).await?.split();
                write_stdout(if args.json {
                    output::render_json(&result)?
                } else {
                    output::render_events(&result)
                })?;
                while let Some(frame) = reader.next_frame().await? {
                    let closed = matches!(frame, EventStreamFrame::Closed);
                    write_stdout(if args.json {
                        output::render_json(&frame)?
                    } else {
                        output::render_event_stream_frame(&frame)
                    })?;
                    if closed {
                        break;
                    }
                }
            } else {
                let result = client.events(&request).await?;
                print!(
                    "{}",
                    if args.json {
                        output::render_json(&result)?
                    } else {
                        output::render_events(&result)
                    }
                );
            }
        }
        CliCommand::Send(args) => {
            let client = ready_client().await?;
            let result = client.send(&args.request()?).await?;
            print!(
                "{}",
                if args.json {
                    output::render_json(&result)?
                } else {
                    output::render_send(&result)
                }
            );
        }
        CliCommand::Resize(args) => {
            let client = ready_client().await?;
            let result = client.resize(&args.request()?).await?;
            print!(
                "{}",
                if args.json {
                    output::render_json(&result)?
                } else {
                    output::render_resize(&result)
                }
            );
        }
        CliCommand::Stop(args) => {
            let client = ready_client().await?;
            let result = client.stop(&args.request()?).await?;
            print!(
                "{}",
                if args.json {
                    output::render_json(&result)?
                } else {
                    output::render_stop(&result)
                }
            );
        }
        CliCommand::Kill(args) => {
            let client = ready_client().await?;
            let result = client.kill(&args.request()?).await?;
            print!(
                "{}",
                if args.json {
                    output::render_json(&result)?
                } else {
                    output::render_kill(&result)
                }
            );
        }
        CliCommand::Delete(args) => {
            let client = ready_client().await?;
            let result = client.delete(&args.request()?).await?;
            print!(
                "{}",
                if args.json {
                    output::render_json(&result)?
                } else {
                    output::render_delete(&result)
                }
            );
        }
        CliCommand::Context(args) => {
            let client = ready_client().await?;
            if args.list {
                let result = client.ui_context_list(&args.list_request()).await?;
                print!(
                    "{}",
                    if args.json {
                        output::render_json(&result)?
                    } else {
                        output::render_context_list(&result)
                    }
                );
            } else {
                let result = client.ui_context_get(&args.get_request()?).await?;
                print!(
                    "{}",
                    if args.json {
                        output::render_json(&result)?
                    } else {
                        output::render_context(&result)
                    }
                );
            }
        }
        CliCommand::Console(args) => {
            console::run_console(args).await?;
        }
        CliCommand::Cockpit(args) => {
            cockpit::run_cockpit(args).await?;
        }
        CliCommand::Attach(args) => {
            let client = ready_client().await?;
            attach::run_attach(&client, &args.request()?).await?;
        }
    }

    Ok(())
}

async fn ready_client() -> Result<client::SessionControlClient, MillmuxCliError> {
    let client = client::SessionControlClient::new()?;
    client.ensure_host_ready().await?;
    Ok(client)
}

fn write_stdout(output: String) -> Result<(), io::Error> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(output.as_bytes())?;
    stdout.flush()
}

#[derive(Debug, Error)]
enum MillmuxCliError {
    #[error("command `{0}` is unsupported in this CLI slice")]
    Unsupported(&'static str),
    #[error(transparent)]
    Commands(#[from] commands::CommandError),
    #[error(transparent)]
    Core(#[from] millrace_sessions_core::error::MillmuxError),
    #[error(transparent)]
    Doctor(#[from] millrace_sessions_host::doctor::DoctorError),
    #[error(transparent)]
    Client(#[from] client::ClientError),
    #[error(transparent)]
    Output(#[from] output::OutputError),
    #[error(transparent)]
    Attach(#[from] attach::AttachError),
    #[error(transparent)]
    Console(#[from] console::ConsoleError),
    #[error(transparent)]
    Cockpit(#[from] cockpit::CockpitError),
    #[error(transparent)]
    Io(#[from] io::Error),
}
