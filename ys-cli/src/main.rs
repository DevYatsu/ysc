use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod run;
mod fmt;
mod error_display;

#[derive(Parser)]
#[command(name = "yatsuscript")]
#[command(version)]
#[command(about = "YatsuScript CLI: runner, REPL, and code formatter.")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Run a script file directly (positional).
    file: Option<PathBuf>,

    /// Run a string snippet.
    #[arg(short = 'c', long)]
    eval: Option<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run a script file.
    Run { file: PathBuf },
    /// Format YatsuScript files.
    Fmt { path: PathBuf },
    /// Syntax-check YatsuScript files.
    Check { path: PathBuf },
}

/// Print an error to stderr (with source annotation for [`JitError`]).
fn report_error(e: Box<dyn std::error::Error>, source: &str) -> Box<dyn std::error::Error> {
    if let Some(je) = e.downcast_ref::<ys_core::error::JitError>() {
        error_display::display_error(je, source);
    } else {
        eprintln!("Error: {}", e);
    }
    e
}

fn main() {
    let cli = Cli::parse();

    let result = match (cli.command, cli.eval, cli.file) {
        (None, None, None) => run::run_repl(),
        (Some(Commands::Run { file }), _, _) => {
            run::run_file(&file).map_err(|e| {
                let source = std::fs::read_to_string(&file).unwrap_or_default();
                report_error(e, &source)
            })
        }
        (Some(Commands::Fmt { path }), _, _) => fmt::format_all(&path),
        (Some(Commands::Check { path }), _, _) => run::check_file(&path),
        (None, Some(code), _) => {
            run::run_source(&code).map_err(|e| report_error(e, &code))
        }
        (None, None, Some(file)) => {
            run::run_file(&file).map_err(|e| {
                let source = std::fs::read_to_string(&file).unwrap_or_default();
                report_error(e, &source)
            })
        }
    };

    if result.is_err() {
        std::process::exit(1);
    }
}
