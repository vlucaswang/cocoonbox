use std::process::ExitCode;
use std::time::Duration;

use cocoonbox_agent::config::{ServeConfig, ToolhostConfig};
use cocoonbox_agent::{offload, server, toolhost};

const RUNTIME_SHUTDOWN: Duration = Duration::from_millis(200);
const USAGE: &str = "usage: cocoonbox-agent <serve|toolhost|toolhost-service|offload|desk|version>";
const NO_SERVICE: &str = "toolhost-service runs under shuttle, which sets SHUTTLE_SERVICE_SOCKET";

fn main() -> ExitCode {
    let cmd = std::env::args().nth(1).unwrap_or_default();
    let result = match cmd.as_str() {
        "serve" => run(async { server::serve(ServeConfig::from_env()?).await }),
        "toolhost" => run(async { toolhost::run(ToolhostConfig::from_env()?).await }),
        "toolhost-service" if std::env::var_os("SHUTTLE_SERVICE_SOCKET").is_none() => {
            Err(NO_SERVICE.to_owned())
        }
        "toolhost-service" => run(async { toolhost::run(ToolhostConfig::from_env()?).await }),
        "offload" => {
            let args = std::env::args().skip(2).collect();
            return match run(async { Ok(offload::main(args).await) }) {
                Ok(code) => ExitCode::from(code),
                Err(e) => {
                    eprintln!("cocoonbox-agent offload: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        "desk" => {
            let args = std::env::args().skip(2).collect();
            return match run(async { Ok(toolhost::desk::cli(args).await) }) {
                Ok(code) => ExitCode::from(code),
                Err(e) => {
                    eprintln!("cocoonbox-agent desk: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        "version" | "--version" => {
            println!("cocoonbox-agent {}", env!("CB_AGENT_VERSION"));
            Ok(())
        }
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("cocoonbox-agent {cmd}: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run<T>(fut: impl Future<Output = Result<T, String>>) -> Result<T, String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| format!("build runtime: {e}"))?;
    let result = runtime.block_on(fut);
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN);
    result
}
