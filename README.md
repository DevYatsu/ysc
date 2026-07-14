# YatsuScript

> A register-based bytecode interpreter with a generational GC, async/await, and a custom LSP.

YatsuScript is a lightweight scripting language with a register-based VM, NaN-boxed values, closures, pattern matching, async/await, and a novel failure handling system — no try/catch, no stack unwinding.

## Quick Start

```bash
cargo install --path ys-cli
yatsuscript examples/fib.ys
yatsuscript              # REPL
```

## Language Examples

### Variables & Functions

```yatscript
name = "YatsuScript"                   // mutable by default
fun greet(name) {
  return "Hello, " + name              // explicit return
}

fun add(a, b) {
  a + b                                // implicit return (last expression)
}

fun multiply(a, b) -> int {            // optional return type annotation
  a * b
}
```

### Collections & Closures

```yatscript
// Lists and objects
numbers = [1, 2, 3, 4, 5]
matrix = [[1, 2], [3, 4]]
user = { name: "Alice", age: 30 }

// List methods
squares = numbers.map(|x| x * x)
evens = numbers.filter(|x| x % 2 == 0)
sum = numbers.reduce(0, |acc, v| acc + v)

// Closures with captures
base = 10
add_base = |x| x + base
```

### Ranges & Iteration

```yatscript
for i in 0..5 {
  print(i)
}

for i in (0..10).step(2) {             // stepped ranges
  print(i)
}
```

### Pattern Matching (Switch)

```yatscript
result = switch status {
  | 200 -> "OK"
  | 404, 410 -> "gone"
  | _ -> "unknown"
}
```

### Async / Await

```yatscript
async fun fetch_data(url) {
  body = await fetch(url)
  return body
}

data = await fetch_data("https://example.com")
```

### Error Handling (no try/catch)

Failures are tagged values that propagate automatically through expressions — no `?` operator needed.

```yatscript
// Declare error kinds
error NotFound
error MathError { | DivisionByZero | Overflow }

// Functions that can fail
fun div(a, b) -> int!MathError {
  if b == 0 { fail DivisionByZero }
  a / b
}

// Inline fallback with `or`
result = div(10, 0) or 0

// Pattern matching with `except`
result = div(10, x) except {
  | MathError.DivisionByZero -> 0
  | MathError.Overflow -> max_value()
  | _ -> fallback
}

// Propagation is automatic — failures flow through expressions
total = (div(revenue, count) + bonus) or 0
```

### Modules

```yatscript
use utils.parse
exp fun public_api() { "visible to importers" }
fun internal_helper() { "private" }
```

## Project Structure

| Crate | Role |
|-------|------|
| `ys-core` | Lexer, Parser, AST, Optimizer, Bytecode Compiler |
| `ys-runtime` | Register VM, Generational GC, Heap, Native Functions |
| `ys-cli` | CLI binary (`yatsuscript`), REPL, formatter |
| `ys-lsp` | Language Server Protocol implementation |

## Documentation

- **[Language Guide](docs/language_guide.md)** — Full syntax reference
- **[Standard Library Reference](docs/stdlib.md)** — Built-in functions and methods
- **[Examples](examples/)** — Runnable `.ys` scripts

## License

MIT © Yanis
