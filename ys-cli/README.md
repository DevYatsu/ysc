# ys-cli (ysc)

> The user-facing command-line tool for YatsuScript: Execution, REPL, and Code Formatting.

`ys-cli` is the entry point for running YatsuScript in any environment. It ties together the core parser and the runtime to provide a seamless developer experience from the terminal.

## Why This Exists

This crate provides a production-ready interface for script execution, interactive debugging via a REPL, and code style consistency via a built-in formatter.

## Quick Start

### Installation

```bash
cargo install --path ys-cli
```

### Running Scripts

```bash
ysc script.ys
# or
cargo run -p ys-cli -- script.ys
```

### Interactive REPL

Start a persistent session with history support:

```bash
ysc
```

### Code Formatting

Auto-format all `.ys` files in the current directory:

```bash
ysc fmt .
```

## Commands

| Command | Action | Description |
|---------|--------|-------------|
| `<path>` | **Run** | Compiles and executes the specified YatsuScript file. |
| `-c "<code>"` | **Eval** | Runs a raw string of code directly. |
| `fmt <path>` | **Format** | Standardizes indentation and syntax across `.ys` files. |
| (none) | **REPL** | Starts an interactive shell for the language. |

## Highlights

- **[REPL](src/run.rs)**: Persistent history through `rustyline`.
- **[Error Display](src/error_display.rs)**: Syntax-highlighted error messages with precise source line indicators and column pointers using the `colored` crate.
- **[Formatter](src/fmt.rs)**: Consistent 2-space indentation and brace-styling logic.

## Usage in Rust

```toml
[dependencies]
ys-cli = { path = "../ys-cli" }
```

```rust
use ys_cli::run::run_source;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    run_source("print('Hello from CLI!')").await?;
    Ok(())
}
```

## License

MIT © Yanis
