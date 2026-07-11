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

fn main() {
    let cli = Cli::parse();

    let result = match (cli.command, cli.eval, cli.file) {
        (None, None, None) => run::run_repl(),
        (Some(Commands::Run { file }), _, _) => match run::run_file(&file) {
            Ok(_) => Ok(()),
            Err(e) => {
                if let Some(je) = e.downcast_ref::<ys_core::error::JitError>() {
                    let source = std::fs::read_to_string(&file).unwrap_or_default();
                    error_display::display_error(je, &source);
                } else {
                    eprintln!("Error: {}", e);
                }
                Err(e)
            }
        },
        (Some(Commands::Fmt { path }), _, _) => fmt::format_all(&path),
        (Some(Commands::Check { path }), _, _) => run::check_file(&path),
        (None, Some(code), _) => match run::run_source(&code) {
            Ok(_) => Ok(()),
            Err(e) => {
                if let Some(je) = e.downcast_ref::<ys_core::error::JitError>() {
                    error_display::display_error(je, &code);
                } else {
                    eprintln!("Error: {}", e);
                }
                Err(e)
            }
        },
        (None, None, Some(file)) => match run::run_file(&file) {
            Ok(_) => Ok(()),
            Err(e) => {
                if let Some(je) = e.downcast_ref::<ys_core::error::JitError>() {
                    let source = std::fs::read_to_string(&file).unwrap_or_default();
                    error_display::display_error(je, &source);
                } else {
                    eprintln!("Error: {}", e);
                }
                Err(e)
            }
        },
    };

    if result.is_err() {
        std::process::exit(1);
    }
}
