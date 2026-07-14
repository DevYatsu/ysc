# YatsuScript Language Guide

YatsuScript uses a clean, minimal syntax. Statements are terminated by newlines (no semicolons). Braces `{}` define blocks.

## 1. Variables

Variables are mutable by default. The first assignment declares a variable.

```yatscript
name = "YatsuScript"     // declaration + assignment
count = 0
count = count + 1        // reassignment
```

Variables can optionally be typed with a colon annotation. Type annotations are parsed but ignored at runtime.

```yatscript
x: 10                    // type annotation (ignored)
```

## 2. Functions

Functions are defined with `fun`. The last expression is the implicit return value.

```yatscript
fun add(a, b) {
  a + b                   // implicit return
}

fun greet(name) {
  return "Hello, " + name // explicit return
}

// Recursion
fun fib(n) {
  if n < 2 {
    n
  } else {
    fib(n - 1) + fib(n - 2)
  }
}
```

Optional return type annotation:

```yatscript
fun multiply(a, b) -> int {
  a * b
}
```

## 3. Control Flow

### If / Else

```yatscript
if score >= 90 {
  print("A")
} else if score >= 80 {
  print("B")
} else {
  print("C")
}
```

### While loops

```yatscript
n = 10
while n > 0 {
  print(n)
  n = n - 1
}
```

### For loops with ranges

```yatscript
for i in 0..5 {
  print(i)
}

// Stepped range
for i in (0..10).step(2) {
  print(i)  // 0, 2, 4, 6, 8
}
```

### Short-circuit logic operators

```yatscript
if a and b { ... }
if a or b  { ... }
```

## 4. Switch

Switch statements support pattern matching with multiple arms:

```yatscript
result = switch x {
  | 1 -> "one"
  | 2, 3 -> "two or three"
  | _ -> "other"          // default arm
}

// Switch arms can have block bodies
switch status {
  | 200 -> {
    print("OK")
  }
  | 404 -> {
    print("Not found")
  }
  | _ -> {
    print("Unknown")
  }
}
```

Use `break` to exit a switch arm early.

## 5. Ranges

Ranges use the `..` operator and are first-class objects:

```yatscript
// Range literal
r = 0..10
for i in r { ... }

// Custom step via .step() method
evens = (0..20).step(2)
```

Range objects have start, end, and step properties extractable via `RangeInfo`.

## 6. Closures

Closures are defined with `|params|` syntax:

```yatscript
// Single expression closure
double = |n| n * 2
print(double(5))          // 10

// Block body closure
evens = [1,2,3,4,5,6].filter(|x| {
  x % 2 == 0
})

// Capture by reference
base = 10
add_base = |x| x + base
print(add_base(5))        // 15

// Capture by value (move)
factory = move || {
  // takes ownership of captures
}
```

Closures can be passed to list methods, stored in variables, and called dynamically.

## 7. Collections

### Lists

```yatscript
// List literal
fruits = ["apple", "banana", "cherry"]
print(fruits[0])          // "apple"
fruits[0] = "pear"

// Repeat initialization (Rust-style)
zeros = [0; 10]           // 10 zeros

// Nested lists
matrix = [[1, 2], [3, 4]]
```

### List Methods

Lists have 18 built-in methods:

| Method | Description |
|--------|-------------|
| `map(f)` | Transform each element |
| `filter(f)` | Keep matching elements |
| `reduce(init, f)` | Fold/accumulate |
| `each(f)` | Side effects, returns original |
| `find(f)` | First match |
| `some(f)` | Any match → bool |
| `every(f)` | All match → bool |
| `includes(v)` | Element exists |
| `index_of(v)` | First index or -1 |
| `sorted()` | Numeric sort |
| `reversed()` | Reversed copy |
| `slice(start, end)` | Sub-list |
| `concat(other)` | Append list |
| `flatten()` | Flatten one level |
| `flat_map(f)` | Map then flatten |
| `take(n)` | First n elements |
| `drop(n)` | All but first n |
| `unique()` | Deduplicate |

```yatscript
numbers = [1, 2, 3, 4, 5, 6, 7, 8]

// Chaining
result = numbers
  .map(|x| x * 2)
  .filter(|x| x > 10)
  .reduce(0, |a, v| a + v)

evens = numbers.filter(|x| x % 2 == 0)
squares = numbers.map(|x| x * x)
sum = numbers.reduce(0, |acc, v| acc + v)
```

### Objects

```yatscript
// Object literal
user = { id: 1, name: "Alice", age: 30 }
print(user.name)
print(user.age)

// Field assignment
user.age = 31

// Method invocation (on registered functions)
user.greet()              // calls greet(user) if registered
```

Objects are hash maps from string keys to values. When a field access doesn't find a property, the language looks up a registered function with that name and calls it with the object as the first argument.

## 8. Async / Await

YatsuScript supports async functions via the `async` keyword and `await` expression:

```yatscript
async fun fetch_url(url) {
  data = await fetch(url)
  return data
}

// Await a promise
result = await some_async_function()
```

The `await` keyword handles both Promise and non-Promise values:
- If the value is a resolved Promise, it extracts the value
- If the value is not a Promise, it passes through directly

## 9. Modules

YatsuScript has a Rust-style module system:

```yatscript
// Import a module
use utils.parse
use utils.format

// Import specific items
use utils.parse.parse_line

// Export items with `exp`
exp fun public_api() {
  "visible to importers"
}

fun internal_helper() {
  "private to this module"
}

exp VERSION = "1.0.0"
```

Module paths use dots (`.`) matching the directory structure.

## 10. Error Declarations

Declare custom error kinds for use with `fail` and `except`:

```yatscript
// Single error kind
error NotFound

// Grouped enum with variants
error MathError {
  | DivisionByZero
  | Overflow
}
```

Enum variants use their full dot-path internally (`"MathError.DivisionByZero"`). Flat errors use just their name (`"NotFound"`).

### Functions with error annotations

Use `!` in the return type to declare what kind of errors a function produces:

```yatscript
fun div(a, b) -> int!MathError {
  if b == 0 { fail DivisionByZero }
  a / b
}
```

Inside a `!`-annotated function, the short form (without the kind prefix) is available for `fail`:

```yatscript
// Short form — expands to MathError.DivisionByZero
fail DivisionByZero
```

Without the annotation, the full path is required:

```yatscript
fun div(a, b) {
  fail MathError.DivisionByZero
}
```

### One kind per function

A function can only use one error kind. The compiler rejects mixing:

```yatscript
fun bad() {
  fail MathError.DivisionByZero
  fail HttpError.Timeout  // ERROR: mixing MathError and HttpError
}
```

### Handling errors at call sites

Use `or` for simple fallbacks and `except` for pattern matching:

```yatscript
result = div(10, 0) or 0

result = div(10, 0) except {
  | MathError.DivisionByZero -> 0
  | MathError.Overflow -> max_value()
  | _ -> fallback
}
```

## 11. Template Strings

Backtick strings support embedded expressions:

```yatscript
name = "world"
greeting = `Hello, ${name}!`   // parsed as template
```

Note: Template expression interpolation is parsed by the lexer but evaluated as plain string concatenation at runtime.

## 12. Comments

```yatscript
// Line comment

/* Block comment (cannot be nested) */
```

## 13. Operator Precedence (highest to lowest)

| Level | Operators |
|-------|-----------|
| Postfix | `f()`, `x[i]`, `x.y` |
| Unary | `!`, `-` |
| Multiplicative | `*`, `/`, `%` |
| Additive | `+`, `-` |
| Comparison | `==`, `!=`, `<`, `<=`, `>`, `>=` |
| `and` | Logical AND (short-circuit) |
| `or` | Logical OR (short-circuit) |
| Range | `..` |

## 14. Type System

YatsuScript is dynamically typed with NaN-boxed values:

| Type | Examples | Storage |
|------|----------|---------|
| Number | `3.14`, `42` | f64 inline |
| Bool | `true`, `false` | NaN-boxed |
| String | `"hello"` | SSO (≤6 bytes inline) or heap |
| List | `[1, 2, 3]` | Heap |
| Object | `{a: 1}` | Heap |
| Range | `0..10` | Heap |
| Timestamp | `timestamp()` | Heap |
| Function | `fun f() {}` | Compiled bytecode |
| Closure | `\|x\| x * 2` | Heap |
| Promise | `async fun` result | Heap |
| Nil | uninitialized value | Zero bits |

## 15. Code Formatter

YatsuScript includes a built-in code formatter:

```bash
ysc fmt <file-or-directory>
```

The formatter operates on the token stream (no AST needed) and normalizes indentation, spacing, and line breaks.
