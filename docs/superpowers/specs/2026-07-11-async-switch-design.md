# YatsuScript: Async/Await + Switch Statement

**Date:** 2026-07-11
**Status:** Draft

## 1. Async/Await

### 1.1 Syntax

```yatscript
// Declare an async function
async fun fetch_data(url) {
    let resp = await fetch(url)
    return resp
}

// Top-level await (module level, no async wrapper needed)
let data = await fetch_data("https://example.com")
print(data)

// Await in any expression
let result = await compute() + 1

// Promise as a value (passing around)
let p = fetch_data("https://a.com")   // p is a Promise
let other = await p                    // await later
```

### 1.2 Semantics (JS-like, event-loop based)

- **`async fun`**: Declares an async function. When called, it immediately returns a `Promise`.
  The function body starts executing synchronously until the first `await`.

- **`await expr`**: Evaluates `expr` (must resolve to a `Promise`), then suspends the
  current execution context and returns control to the event loop. When the awaited
  Promise resolves, execution resumes from the `await` point.

- **Top-level await**: The module's top-level code is implicitly wrapped in an async task.
  The runtime's event loop runs until all top-level tasks and their descendants complete.

- **Promise** (`ManagedObject::Promise`):
  - `state`: `Pending` | `Resolved(Value)` | `Rejected`
  - `continuation`: captured call frame state (registers + PC + instructions)
  - `thens`: list of `.then()` callbacks for chaining

### 1.3 Event Loop

```
                                                                                
    ┌─────────────────────────────────────────────────────┐                    
    │  Event Loop                                         │                    
    │                                                     │                    
    │  while tasks_remain() {                             │                    
    │      for each pending promise {                     │                    
    │          poll(promise)  // advance until await/ret   │                    
    │      }                                              │                    
    │      process_io()        // non-blocking I/O events  │                    
    │      yield_to_thread_pool()  // blocking ops done    │                    
    │  }                                                  │                    
    └─────────────────────────────────────────────────────┘                    
                                                                                
```

The event loop is part of the runtime. It manages:
1. **Promise queue**: pending async tasks
2. **I/O events**: completed read/write operations (via epoll/kqueue or a thread pool)
3. **Timer events**: `sleep(ms)` resolves after timeout

When a promise hits `await`, it yields. The event loop polls other pending promises.
When the awaited promise resolves, the first promise is re-queued.

### 1.4 Promise chaining

```yatscript
let p = fetch_data(url)
p.then(|data| print(data))
p.catch(|err| print("error: " + str(err)))
```

`.then()` and `.catch()` return new Promises, enabling chaining (like JS).

### 1.5 Implementation sketch

- **Lexer**: Add `async` and `await` keywords
- **Parser**: Add `async` modifier for `fun` declarations; `await` as a unary prefix expression
- **AST nodes**: `AsyncFun { fun_decl }`, `Await { expr }`
- **Codegen**: 
  - `async fun` → emits the function body with a prologue that creates a `Promise`
  - `await expr` → emits code that evaluates `expr`, checks it's a Promise, then
    suspends the current frame and yields to the event loop
- **VM**:
  - New `Promise` variant in `ManagedObject`
  - New instruction `MakePromise` + `Await` instructions
  - Event loop integrated into `execute_bytecode`

## 2. Switch Statement

### 2.1 Syntax

```yatscript
// Basic switch
switch status_code {
    200 => print("OK")
    404 => print("Not found")
    _ => print("Unknown")
}

// With blocks and break
switch x {
    1 => {
        print("one")
        break          // exits the switch
    }
    2 => print("two")  // falls through to default (since no break)
    _ => print("other")
}

// Multi-value patterns
switch c {
    'a' | 'e' | 'i' | 'o' | 'u' => print("vowel")
    _ => print("consonant")
}

// As expression (returns a value)
let name = switch id {
    0 => "admin"
    1 => "user"
    _ => "guest"
}
```

### 2.2 Semantics

- **Fallthrough by default** (C-style): If a case arm doesn't end with `break`, execution
  falls through to the next arm.
- **`break`**: Exits the switch (like C's `break`).
- **`_`**: Default/wildcard arm. Must be last if present.
- **Multi-value**: `a | b | c => body` matches any of the values.
- **Expression**: If used as an expression, the switch evaluates to the value of the
  matched arm (arms must be expressions, not statements). Implicit break at end of
  each arm in expression context (no fallthrough).

### 2.3 Implementation

- **Lexer**: `switch`, `case` (or use `=>` directly)
- **Parser**: `switch expr { pattern => body, ... }`
- **Codegen**: Emit comparison + jump chain. Each arm compares against the switch value,
  jumps to the body if match, falls through to next arm otherwise. `break` emits a
  `Jump` to the end of the switch.
- **Optimization**: For small integer ranges, emit a jump table instead of comparison chain.
