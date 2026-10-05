//! The `qbot` binary. Configuration is checked first thing in every command, so a bad
//! deployment fails before anything else starts, with every problem listed.

use std::process::ExitCode;
use std::sync::Arc;

use qbot_app::{Options, run};
use qbot_config::{Loaded, ProcessEnv, SecretResolver, Source, load, prepare_directories};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Frequent small allocations (chat lines, JSON, SQL rows) are what this process does all day.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const USAGE: &str = "\
usage: qbot <command>

commands:
  run              start the bot: serve the platform connection until stopped
  check-config     load, validate and report where every layer came from; exit 0 if valid
  print-config     print the effective configuration (secret names, never secret values)
  print-defaults   print the baked-in defaults

environment:
  QBOT_CONFIG_DIR   configuration directory   (default /etc/qbot)
  QBOT_DATA_DIR     persistent state directory (default /var/lib/qbot)
  QBOT_SECRETS_DIR  mounted secrets directory  (default /run/secrets)
  QBOT__SECTION__KEY=value   override any configuration key";

fn describe(source: &Source) -> String {
    match source {
        Source::Defaults => "built-in defaults".to_owned(),
        Source::File(path) => format!("file {}", path.display()),
        Source::DropIn(path) => format!("drop-in {}", path.display()),
        Source::Env(name) => format!("environment variable {name}"),
    }
}

fn check(loaded: &Loaded) -> Result<(), qbot_config::ConfigErrors> {
    let paths = loaded.config.paths(&loaded.layout);
    prepare_directories(&paths)?;
    let resolver = SecretResolver::new(Arc::new(ProcessEnv), loaded.layout.secrets_dir.clone());
    loaded.config.ready_to_run(&resolver)?;
    Ok(())
}

fn main() -> ExitCode {
    let command = std::env::args().nth(1);
    match command.as_deref() {
        Some("run") => {
            let loaded = match load() {
                Ok(loaded) => loaded,
                Err(errors) => {
                    eprint!("{errors}");
                    return ExitCode::FAILURE;
                }
            };
            // Normal output, plus the recent warnings and errors /logs shows.
            let logs = qbot_app::logs::LogBuffer::new(qbot_app::logs::KEPT_LINES);
            tracing_subscriber::registry()
                .with(
                    tracing_subscriber::fmt::layer().with_filter(
                        tracing_subscriber::EnvFilter::try_from_default_env()
                            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
                    ),
                )
                .with(logs.layer())
                .init();
            let runtime = match tokio::runtime::Runtime::new() {
                Ok(runtime) => runtime,
                Err(error) => {
                    eprintln!("cannot start the async runtime: {error}");
                    return ExitCode::FAILURE;
                }
            };
            let shutdown = CancellationToken::new();
            let options = Options {
                env: Arc::new(ProcessEnv),
                shutdown: shutdown.clone(),
                ready: None,
                logs: Some(logs),
            };
            let outcome = runtime.block_on(async {
                tokio::spawn(async move {
                    shutdown_signal().await;
                    shutdown.cancel();
                });
                run(loaded, options).await
            });
            match outcome {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("{error}");
                    ExitCode::FAILURE
                }
            }
        }
        Some("print-defaults") => {
            print!("{}", qbot_config::DEFAULTS);
            ExitCode::SUCCESS
        }
        Some(cmd @ ("check-config" | "print-config")) => {
            let loaded = match load() {
                Ok(loaded) => loaded,
                Err(errors) => {
                    eprint!("{errors}");
                    return ExitCode::FAILURE;
                }
            };
            if cmd == "print-config" {
                match toml::to_string_pretty(&loaded.config) {
                    Ok(text) => print!("{text}"),
                    Err(e) => {
                        eprintln!("cannot render configuration: {e}");
                        return ExitCode::FAILURE;
                    }
                }
                return ExitCode::SUCCESS;
            }
            println!("configuration layers, lowest precedence first:");
            for source in &loaded.sources {
                println!("  - {}", describe(source));
            }
            match check(&loaded) {
                Ok(()) => {
                    println!(
                        "ok: configuration is valid, secrets are present, state directories are writable"
                    );
                    ExitCode::SUCCESS
                }
                Err(errors) => {
                    eprint!("{errors}");
                    ExitCode::FAILURE
                }
            }
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// Ctrl-C, or SIGTERM (what `docker stop` sends).
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}
