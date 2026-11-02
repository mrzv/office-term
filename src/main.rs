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

    /// Maximum document file size in MiB; overrides the configuration.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    max_file_size_mib: Option<u32>,

    /// Print the shared configuration as JSON (also creates the default file).
    #[arg(long)]
    print_config: bool,

    /// Enable viewer control with named viewer endpoints.
    #[arg(long)]
    session: Option<String>,

    /// Save the running viewer's visible document viewport as a PNG.
    #[arg(long, requires = "session", conflicts_with = "path")]
    screenshot: Option<PathBuf>,

    /// Stop a running named viewer session.
    #[arg(long, requires = "session", conflicts_with_all = ["path", "screenshot", "print_config", "page", "max_file_size_mib"])]
    quit: bool,
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
    config.set_max_file_size_override(cli.max_file_size_mib);
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
    if cli.quit {
        let result = config
            .forward_socket()
            .ok_or_else(|| std::io::Error::other("forward_socket is disabled"))
            .and_then(officeterm::ipc::quit);
        return match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("oterm: {error}");
                ExitCode::FAILURE
            }
        };
    }
    if let Some(path) = cli.screenshot.as_deref() {
        let result = (|| -> Result<(), Box<dyn std::error::Error>> {
            let socket = config
                .forward_socket()
                .ok_or("forward_socket is disabled")?;
            let path = std::path::absolute(path)?;
            officeterm::ipc::screenshot(socket, &path)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;

    #[test]
    fn quit_requires_a_named_session() {
        assert_eq!(
            Cli::try_parse_from(["oterm", "--quit"]).unwrap_err().kind(),
            ErrorKind::MissingRequiredArgument
        );
        let cli = Cli::try_parse_from(["oterm", "--session", "demo", "--quit"]).unwrap();
        assert!(cli.quit);
        assert_eq!(cli.session.as_deref(), Some("demo"));
        assert!(cli.path.is_none());
    }

    #[test]
    fn quit_rejects_document_and_other_actions() {
        for incompatible in [
            vec!["document.pdf"],
            vec!["--screenshot", "capture.png"],
            vec!["--print-config"],
            vec!["--page", "2"],
            vec!["--max-file-size-mib", "100"],
        ] {
            let mut args = vec!["oterm", "--session", "demo", "--quit"];
            args.extend(incompatible);
            assert_eq!(
                Cli::try_parse_from(args).unwrap_err().kind(),
                ErrorKind::ArgumentConflict
            );
        }
    }
}
