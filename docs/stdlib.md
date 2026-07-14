# YatsuScript Standard Library Reference

## I/O Core

### `print(...)`

Prints all arguments to stdout, separated by spaces, with a trailing newline.

```yatscript
print("Hello, world!")    // Hello, world!
print("Value:", 42)       // Value: 42
```

### `str(x)`

Converts any value to its string representation.

```yatscript
str(42)                   // "42"
str(true)                 // "true"
str([1, 2, 3])            // "[1, 2, 3]"
```

## Collections

### `len(x)`

Returns the length of a list, string, or object.

```yatscript
len("hello")              // 5
len([1, 2, 3])            // 3
len({a: 1, b: 2})         // 2
len(0..10)                // 10 (range length)
```

## Time

### `time()`

Returns the number of seconds since the Unix epoch as a float.

```yatscript
start = time()
// ... do work ...
elapsed = time() - start
```

### `timestamp()`

Returns an opaque `Timestamp` object created with `Instant::now()`.

```yatscript
t = timestamp()
```

Timestamps can be stored, compared, and passed around. They display as `Timestamp(...)`.

### `sleep(ms)`

Blocks the current thread for `ms` milliseconds.

```yatscript
sleep(1000)               // pause 1 second
```

## Networking

### `fetch(url)`

Performs an HTTP GET request. Returns a **Promise** that resolves to the response body as a string.

```yatscript
result = await fetch("https://example.com")
// result contains the response body
```

If the request fails, it prints an error and resolves to nil.

### `serve(port, handler)`

Starts a minimal HTTP server on the given port. Returns a **Promise** that resolves when the server starts. The `handler` is a function name (string) that receives raw request data as its first argument and should return a response string.

```yatscript
fun handle(req) {
  return "Hello from YatsuScript!"
}

await serve(8080, handle)
```

If the response starts with `HTTP/`, it's sent as-is. Otherwise it's wrapped in a minimal 200 OK response.

Handlers run in a separate thread per connection. Errors result in a 500 response.

## List Methods

All list methods can be called with dot syntax on any list value. Closures accept pipe-delimited parameters.

### `map(f)`

Transform each element:

```yatscript
[1, 2, 3].map(|x| x * 2)   // [2, 4, 6]
```

### `filter(f)`

Keep elements where the closure returns truthy:

```yatscript
[1, 2, 3, 4].filter(|x| x % 2 == 0)  // [2, 4]
```

### `reduce(init, f)`

Fold/accumulate over elements:

```yatscript
[1, 2, 3].reduce(0, |acc, v| acc + v)  // 6
[1, 2, 3].reduce(1, |acc, v| acc * v)  // 6
```

### `each(f)`

Call closure for side effects on each element. Returns the original list.

```yatscript
[1, 2, 3].each(|x| print(x))
```

### `find(f)`

Returns the first element matching the predicate, or nil:

```yatscript
[1, 2, 3].find(|x| x > 1)    // 2
```

### `some(f)`

Returns `true` if any element matches:

```yatscript
[1, 2, 3].some(|x| x > 2)    // true
[1, 2, 3].some(|x| x > 10)   // false
```

### `every(f)`

Returns `true` if all elements match:

```yatscript
[1, 2, 3].every(|x| x > 0)   // true
```

### `includes(v)`

Returns `true` if the value exists in the list. Uses parallel search on large lists (>10k elements).

```yatscript
[1, 2, 3].includes(2)         // true
```

### `index_of(v)`

Returns the first index of the value, or -1:

```yatscript
[10, 20, 30].index_of(20)    // 1
[10, 20, 30].index_of(99)    // -1
```

### `sorted()`

Returns a numerically sorted copy. Uses parallel sort on large lists (>10k elements).

```yatscript
[3, 1, 4, 1, 5].sorted()     // [1, 1, 3, 4, 5]
```

### `reversed()`

Returns a reversed copy:

```yatscript
[1, 2, 3].reversed()         // [3, 2, 1]
```

### `slice(start, end)`

Returns a sub-list from `start` (inclusive) to `end` (exclusive):

```yatscript
[1, 2, 3, 4, 5].slice(1, 3) // [2, 3]
```

### `concat(other)`

Returns a new list with elements of `other` appended:

```yatscript
[1, 2].concat([3, 4])        // [1, 2, 3, 4]
```

### `flatten()`

Flattens one level of nesting:

```yatscript
[[1, 2], [3, 4]].flatten()   // [1, 2, 3, 4]
```

### `flat_map(f)`

Map each element and flatten the result:

```yatscript
[1, 2, 3].flat_map(|x| [x, x * 10])
// [1, 10, 2, 20, 3, 30]
```

### `take(n)`

Returns the first `n` elements:

```yatscript
[1, 2, 3, 4, 5].take(3)     // [1, 2, 3]
```

### `drop(n)`

Returns all but the first `n` elements:

```yatscript
[1, 2, 3, 4, 5].drop(2)     // [3, 4, 5]
```

### `unique()`

Returns a deduplicated copy:

```yatscript
[1, 2, 1, 3, 2].unique()     // [1, 2, 3]
```

## Ranges

### `range.step(n)`

Returns a new range with a custom step size:

```yatscript
(0..10).step(2)              // Range: start=0, end=10, step=2
for i in (0..10).step(2) { print(i) }
```

## Chaining

List methods can be chained because each returns a new list (or transformed value):

```yatscript
numbers = [1, 2, 3, 4, 5, 6, 7, 8]

result = numbers
  .map(|x| x * 2)
  .filter(|x| x > 10)
  .reduce(0, |a, v| a + v)
```

## Error Handling

Native functions use descriptive error messages. Runtime errors include the source location (line:column).

```yatscript
fetch(123)     // Runtime error at 1:1: fetch(url) requires a string URL as the first argument
[1].sorted()   // OK (numbers only — non-numbers are treated as equal)
```
