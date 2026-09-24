# Examples

Run any of these with `azula run FILE` (or build them with `azula build FILE`).
Every example is built with both the Rust compiler and the self-hosted compiler
by `tests/run_examples.sh`, which checks that they print the same thing.

| Example | Shows |
| --- | --- |
| [hello.azl](hello.azl) | The smallest program |
| [fib.azl](fib.azl), [factorial.azl](factorial.azl), [fizzbuzz.azl](fizzbuzz.azl) | Functions, recursion and conditionals |
| [structs.azl](structs.azl) | Structs, methods, `&self` vs `self`, nested fields and `alloc` |
| [enums.azl](enums.azl) | Plain enums, enums with payloads, and `match` on enums and characters |
| [generics.azl](generics.azl) | Generic functions, structs and enums, `Option`, inference and turbofish |
| [sorting.azl](sorting.azl) | A generic quicksort over `Vec<T>` |
| [word_count.azl](word_count.azl) | `Map`, `Vec` and `StringBuilder` together |
| [sieve.azl](sieve.azl) | Constants and a raw heap array (`malloc`, `sizeof`, pointer indexing) |
| [game_of_life.azl](game_of_life.azl) | Conway's Game of Life on a wrapping grid |
| [calculator.azl](calculator.azl) | A tokenizer, recursive-descent parser and evaluator; reads its expression from the command line |
| [brainfuck.azl](brainfuck.azl) | An interpreter for Brainfuck |
| [modules/main.azl](modules/main.azl) | Splitting a program across files with `import` |

For a much bigger example, the compiler itself is written in Azula: see [`compiler/`](../compiler).
