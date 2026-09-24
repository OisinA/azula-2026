## Azula

[![chat](https://img.shields.io/discord/606118150655705088)](https://discord.gg/Hkx8XnB) [![issues](https://img.shields.io/github/issues/azula-lang/azula)](https://github.com/azula-lang/azula/issues)

Azula is a strongly-typed compiled language, using an LLVM backend, with the following goals:
- Static typing
- Easy-to-read syntax
- Efficient execution

Azula is self-hosting: the compiler in [`compiler/`](compiler) is written in Azula
and compiles itself.

[Discord](https://discord.gg/Hkx8XnB)

## Compiling Your Code
```
azula build FILENAME [-o OUTPUT] [--emit-llvm] [--release]
```

or to run directly:
```
azula run FILENAME
```

## Building the compiler

There are two compilers:

- **The Rust compiler** (the Cargo workspace in this repository) is the bootstrap
  ("stage 0") compiler. It needs Rust and LLVM 18, and links with `zig cc` if
  available or `cc` otherwise.
- **The self-hosted compiler** (`compiler/*.azl`) emits LLVM IR as text and uses
  `clang` to turn it into an executable (set `AZULA_CC` to use a different one).

`./bootstrap.sh` builds the Rust compiler, uses it to build the self-hosted compiler
(stage 1), then has that compile itself (stage 2), and that compile itself again
(stage 3). Stages 2 and 3 must generate identical LLVM IR. The result is
`build/azula`.

```
./bootstrap.sh          # bootstrap and check the fixed point
./bootstrap.sh --test   # also run the test suite with every stage
./run_tests.sh          # cargo unit tests + bootstrap + all tests
```

The self-hosted compiler finds the standard library via `--stdlib DIR`, the
`AZULA_STDLIB` environment variable, or the `stdlib/` directory next to the
directory holding the compiler.

## Progress

- [x] Lexing
- [x] Parsing
- [x] Typechecking
- [x] Azula IR codegen
- [x] LLVM backend
- [x] Hooking into C standard library functions
- [x] Arrays
- [x] Loops
- [x] Structures
- [x] Methods
- [x] Multi-file projects (`import "file.azl"`)
- [x] Enums with payloads and pattern matching
- [x] Generics
- [x] Beginnings of a standard library (`Option<T>`, `Vec<T>`, `Map<V>`, `StringBuilder`)
- [x] Self-hosting compiler

## A tour of the language

More complete programs are in [`examples/`](examples).


```
// Structs, with methods in their body (`self` is implicit)
struct Point {
    x: int;
    y: int;

    func length_squared(): int {
        return self.x * self.x + self.y * self.y;
    }

    // `self` is a reference, so methods can modify the value they're called on
    func shift(dx: int) {
        self.x = self.x + dx;
    }
}

// Enums can carry payloads; values of these enums are immutable and
// heap-allocated, so they can be recursive
enum Expr {
    Num(int);
    Add(Expr, Expr);
    Neg(Expr);
}

func eval(e: Expr): int {
    return match e {
        Expr::Num(n) => n,
        Expr::Add(a, b) => eval(a) + eval(b),
        Expr::Neg(inner) => 0 - eval(inner),
    };
}

// Generic functions and types are specialised for each set of type
// arguments. Type arguments are inferred where possible, or given
// with a turbofish: max::<int>(1, 2), Vec::<str>::new()
func max<T>(a: T, b: T): T {
    if a > b {
        return a;
    }
    return b;
}

struct Pair<A, B> {
    first: A;
    second: B;
}

func main {
    var p = Point { x: 3, y: 4 };
    p.shift(1);
    printf("%d\n", p.length_squared());

    var e = Expr::Add(Expr::Num(2), Expr::Neg(Expr::Num(5)));
    printf("%d\n", eval(e));

    var names: Vec<str> = Vec::new();
    names.push("azula");
    var ages: Map<int> = Map::new();
    ages.insert("azula", 7);
    match ages.get("azula") {
        Option::Some(age) => { printf("%s is %d\n", names.get(0), age); },
        Option::None => { printf("unknown\n"); },
    }

    var pair = Pair { first: max(3, 9), second: "nine" };
    printf("%d %s\n", pair.first, pair.second);

    // Bitwise operators, sizeof, casts and raw pointers
    var flags = (1 << 4) | 0x3;
    var buffer: &int = malloc(8 * sizeof(int));
    buffer[0] = flags & 0xf;
    printf("%d %d\n", buffer[0], 'a' as int);
}
```

Other things to know:

- Types: `int` (64-bit), `i8`–`i64`, `u8`–`u64`, `float`, `f32`, `bool`, `str`
  (a C string), `&T` pointers, arrays `[T; n]`, structs, enums and generic instances.
- Variables are declared with `var` (mutable) or `const`; parameters are immutable.
- Loops are `while cond { }`, `for cond { }` and `for { }`, with `break` and `continue`.
- `match` works on enums and integers (including character literals); arms can be
  blocks, and a `match` can be used as a statement or an expression.
- Methods are declared in a type's body and receive `self` implicitly (a reference for
  structs, the value for enums); a function there that doesn't use `self` is static and is
  called as `Type::name()`. `extend Type { ... }` adds methods to a type from elsewhere.
- Struct fields and enum variants end with `;`, and `new Type { ... }` allocates on the heap.
- `extern func name(types): type;` (optionally `extern varargs func`) declares C functions;
  `stdlib/libc.azl` declares the common ones.
- Memory: see below.

## Memory management

Programs built by the self-hosted compiler are garbage collected. `new Struct { ... }`,
enum values with payloads, array literals and `malloc`/`calloc`/`realloc` all allocate from
a conservative, non-moving mark-and-sweep collector (`stdlib/gc.azl`, written in Azula).
`free` still releases memory immediately, but is never required.

For many short-lived objects with a shared lifetime, use an arena:

```
var arena = Arena::new();
var node = new Node { value: 1, next: null } in arena;
...
arena.free_all();   // frees everything allocated in the arena at once
```

Arena memory isn't collected, but the collector scans it, so objects it points to stay alive.
Using arena objects after `free_all` is an error.

Set `AZULA_GC_VERBOSE=1` to see collections, or `AZULA_GC_STRESS=N` to collect every N
allocations (useful for testing). The Rust compiler doesn't include the collector: programs
it builds use `malloc` directly and never free memory implicitly.

## Requirements

* LLVM 18 (for the Rust compiler)
* clang (for the self-hosted compiler)
