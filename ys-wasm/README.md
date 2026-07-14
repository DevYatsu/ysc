# YatsuScript WASM

YatsuScript language interpreter compiled to WebAssembly. Run `.ys` scripts in the browser with full `print()` capture, AST inspection, and bytecode disassembly.

## Quick Start

```bash
npm install yatsuscript
```

```js
import init, { _eval, _parseAst, _disassemble } from 'yatsuscript';

await init();

// ── Eval ──────────────────────────────────────────────
const result = _eval(`print("hello " + 42)`);
console.log(result.success);   // true
console.log(result.lines);     // [{ value: "hello 42", line: 1 }]

// ── AST ───────────────────────────────────────────────
const ast = _parseAst(`fun f(x) { ret x + 1 }`);
console.log(ast.data);
// [{ type: "function", name: "f", params: ["x"], body: [...] }]

// ── Bytecode ──────────────────────────────────────────
const bc = _disassemble(`print(1 + 2)`);
console.log(bc.functions);
console.log(bc.main);
```

## API

### `_eval(source: string): EvalResult`

Compile and run YatsuScript code. Captures all `print()` output.

```ts
interface EvalResult {
  success: boolean;
  lines: PrintLine[];
  error?: string;
}

interface PrintLine {
  value: string;         // the printed text
  line?: number;         // source line number (when available)
}
```

### `_parseAst(source: string): AstResult`

Parse source into a structured AST tree.

```ts
interface AstResult {
  success: boolean;
  data: AstNode[];
  error?: string;
}
```

Each `AstNode` has a `type` field (`"number"`, `"binary"`, `"call"`, `"function"`, etc.) and type-specific fields like `value`, `left`, `right`, `name`, `args`, etc.

### `_disassemble(source: string): BytecodeResult`

Compile source and return the bytecode instructions.

```ts
interface BytecodeResult {
  success: boolean;
  functions: FunctionBytecode[];
  main: string[];
  error?: string;
}

interface FunctionBytecode {
  index: number;
  name: string;
  params: number;
  locals: number;
  instructions: string[];
}
```

## Building from source

```bash
wasm-pack build ys-wasm --target web --no-default-features
```

Or use the build script (which patches package metadata):

```bash
cd ys-wasm && bash build.sh
```

## Examples

```js
import init, { _eval } from 'yatsuscript';

await init();

// Run code and show output
const r = _eval(`
fun fib(n) {
  if n < 2 { ret n }
  else { ret fib(n - 1) + fib(n - 2) }
}
print("fib(10) = " + fib(10))
`);

if (r.success) {
  r.lines.forEach(l => {
    const loc = l.line ? ` [line ${l.line}]` : '';
    console.log(`${l.value}${loc}`);
  });
}
```
