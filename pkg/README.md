# ysc WASM

ysc language interpreter compiled to WebAssembly. Run `.ys` scripts in the browser with `print()` capture, AST inspection, bytecode disassembly, syntax highlighting, and LSP grammar.

```bash
npm install ysc-wasm
```

## Quick Start — Interpreter

```js
import init, { _eval, _parseAst, _disassemble } from 'ysc-wasm';

await init();

const r = _eval(`print("hello " + 42)`);
console.log(r.lines[0].value); // "hello 42"
```

## Quick Start — Syntax Highlighting (Monaco)

```js
import { monarchLanguage, registerMonaco } from 'ysc-wasm/syntax.js';

// Option 1: get the tokenizer definition
monaco.languages.register({ id: 'ysc' });
monaco.languages.setMonarchTokensProvider('ysc', monarchLanguage);

// Option 2: one-liner
registerMonaco();
```

## Package Contents

| Path | Description |
|------|-------------|
| `ysc-wasm` (default) | WASM interpreter: `_eval`, `_parseAst`, `_disassemble` |
| `ysc-wasm/syntax.js` | Monaco Monarch tokenizer + `registerMonaco()` helper |
| `ysc-wasm/ysc.tmLanguage.json` | TextMate grammar (VS Code, Sublime, etc.) |

## Full API

### Interpreter

#### `_eval(source): EvalResult`
Run code, capture `print()` output.

```ts
{ success: boolean, lines: [{ value: string, line?: number }], error?: string }
```

#### `_parseAst(source): AstResult`
Parse source into structured AST.

```ts
{ success: boolean, data: AstNode[], error?: string }
```

#### `_disassemble(source): BytecodeResult`
Compile and return bytecode.

```ts
{ success: boolean, functions: [{ index, name, params, locals, instructions }], main: string[], error?: string }
```

### Syntax

#### `monarchLanguage`
Monarch tokenizer definition object — pass directly to `monaco.languages.setMonarchTokensProvider()`.

#### `registerMonaco()`
Register the `ysc` language in Monaco Editor (idempotent — safe to call multiple times).

#### `ysc.tmLanguage.json`
TextMate grammar for VS Code, Sublime Text, etc.

```bash
# VS Code: copy to .vscode/syntaxes/
cp node_modules/ysc-wasm/ysc.tmLanguage.json .vscode/syntaxes/
```

## Building from source

```bash
cd ys-wasm && bash build.sh
```

Publishes to `pkg/` with all assets.
