use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Office file to open. Omit to open the file picker.
    path: Option<PathBuf>,

    /// One-based page/slide number to show first.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..))]
    page: u32,

    /// Print the shared configuration as JSON (also creates the default file).
    #[arg(long)]
    print_config: bool,

    /// Enable viewer control with named viewer endpoints.
    #[arg(long)]
    session: Option<String>,

    /// Save the running viewer's visible document viewport as a PNG.
    #[arg(long, requires = "session", conflicts_with = "path")]
    screenshot: Option<PathBuf>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let mut config = match officeterm::config::Config::load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("oterm: {error}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(error) = config.select_session(cli.session.as_deref()) {
        eprintln!("oterm: {error}");
        return ExitCode::FAILURE;
    }
    if cli.print_config {
        match serde_json::to_string(&config) {
            Ok(json) => {
                println!("{json}");
                return ExitCode::SUCCESS;
            }
            Err(error) => {
                eprintln!("oterm: {error}");
                return ExitCode::FAILURE;
            }
        }
    }
    if let Some(path) = cli.screenshot.as_deref() {
        let result = (|| -> Result<(), Box<dyn std::error::Error>> {
            let socket = config
                .forward_socket()
                .ok_or("forward_socket is disabled")?;
            officeterm::ipc::screenshot(socket, path)?;
            Ok(())
        })();
        return match result {
            Ok(()) => {
                println!("{}", path.display());
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("oterm: {error}");
                ExitCode::FAILURE
            }
        };
    }
    match officeterm::app::run(cli.path, cli.page - 1, &config) {
        Ok(()) | Err(officeterm::app::AppError::Quit) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("oterm: {error}");
            ExitCode::FAILURE
        }
    }
}
