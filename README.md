# wasm-fuel

A WebAssembly binary parser and a **fuel-metered** stack interpreter, in safe
Rust with **zero dependencies**.

Fuel metering is how a host bounds untrusted computation without a watchdog
thread: every instruction costs something from a budget, the budget is charged
*before* the instruction runs, and when it hits zero execution stops with a
trap. That makes "how much work did this code do" a number you can measure,
bill, and cap - deterministically, so the same module with the same arguments
always burns exactly the same amount.

This crate implements both halves of that from scratch:

1. **A real binary format decoder.** Magic number, version, section ordering,
   LEB128 immediates with width and canonicity checks, type/import/function/
   export/start/code sections. Strict and total: any byte string either decodes
   or returns a `ParseError` with the offset that broke. It never panics.
2. **A fuel-metered interpreter** for a defined subset of the instruction set,
   with structured control flow (`block` / `loop` / `if` / `br` / `br_if` /
   `call`), a configurable cost table, and every failure - out of fuel, divide
   by zero, stack underflow, illegal opcode - surfaced as `Err(Trap)`.

It is deliberately small enough to read in one sitting. See
[Supported subset](#supported-subset) for exactly what it does and does not do.

## Install

```toml
[dependencies]
wasm-fuel = { git = "https://github.com/lindah11/wasm-fuel" }
```

Requires Rust 1.82 or newer. `#![forbid(unsafe_code)]`, no build script, no
transitive dependencies.

## Usage

```rust
use wasm_fuel::{CostTable, Interpreter, Trap, Val};

// (module (func (export "square") (param i32) (result i32)
//   local.get 0  local.get 0  i32.mul))
const SQUARE: &[u8] = &[
    0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00,
    0x01, 0x06, 0x01, 0x60, 0x01, 0x7F, 0x01, 0x7F,
    0x03, 0x02, 0x01, 0x00,
    0x07, 0x0A, 0x01, 0x06, 0x73, 0x71, 0x75, 0x61, 0x72, 0x65, 0x00, 0x00,
    0x0A, 0x09, 0x01, 0x07, 0x00, 0x20, 0x00, 0x20, 0x00, 0x6C, 0x0B,
];

let module = wasm_fuel::parse(SQUARE).unwrap();
assert_eq!(module.describe_exports(), vec!["func square: (i32) -> i32"]);

let mut interp = Interpreter::new(&module)
    .with_costs(CostTable::uniform(1))
    .with_fuel(1_000);

assert_eq!(interp.call_export("square", &[Val::I32(9)]), Ok(vec![Val::I32(81)]));
assert_eq!(interp.fuel_consumed(), 4); // get, get, mul, end

// Starve it and the same call stops instead of running.
interp.set_fuel(3);
assert_eq!(interp.call_export("square", &[Val::I32(9)]), Err(Trap::OutOfFuel));
```

The interesting property is the one on the last two lines: a module that loops
forever is not a hang, it is an `Err`.

```rust
// (func (export "spin") (loop br 0)), bytes elided
let module = wasm_fuel::parse(SPIN)?;
let mut interp = Interpreter::new(&module).with_fuel(5_000);
assert_eq!(interp.call_export("spin", &[]), Err(Trap::OutOfFuel));
assert_eq!(interp.fuel_consumed(), 5_000);
```

### Inspecting a file

The bundled example parses a `.wasm` off disk, prints what it found, and
optionally runs an export:

```sh
$ cargo run --example inspect -- sum.wasm
sum.wasm: 66 bytes
  1 types, 0 imported functions, 1 local functions
  exports:
    func sum: (i32) -> i32

$ cargo run --example inspect -- sum.wasm sum 100 --fuel 10000
...
calling sum(i32) -> i32 with 10000 fuel
  result: [I32(5050)]
  fuel used: 1408

$ cargo run --example inspect -- sum.wasm sum 100 --fuel 50
...
calling sum(i32) -> i32 with 50 fuel
  trap: out of fuel
  fuel used: 50
```

## API

### Parsing

```rust
let module: Module = wasm_fuel::parse(&bytes)?;   // Result<Module, ParseError>

module.types;                    // Vec<FuncType>
module.imports;                  // Vec<Import>
module.funcs;                    // Vec<Func>: type index, locals, body bytes
module.exports;                  // Vec<Export>
module.start;                    // Option<u32>
module.custom_sections;          // names, in order
module.skipped_sections;         // ids of sections that were parsed over

module.export_func("run");       // Option<u32>, the function index
module.func_type(index);         // Option<&FuncType>, imports included
module.imported_func_count();    // imports occupy the low indices
module.describe_exports();       // ["func run: (i32) -> i32", ...]
```

`ParseError` carries `offset` (the byte that broke) and `kind`, one of
`NotWasm`, `UnsupportedVersion`, `UnexpectedEof`, `Leb`, `UnknownSectionId`,
`SectionOutOfOrder`, `SectionSizeMismatch`, `InvalidValType`, `InvalidFuncType`,
`InvalidExternKind`, `InvalidLimits`, `InvalidUtf8`, `FunctionCodeMismatch`,
`TypeIndexOutOfRange`, `TooManyLocals` or `MissingEnd`.

### Executing

```rust
let mut interp = Interpreter::new(&module)
    .with_costs(CostTable::default())
    .with_fuel(1_000_000)
    .with_max_call_depth(256)     // default
    .with_max_stack(16 * 1024);   // default

interp.call_export("run", &[Val::I32(7)])?;  // Result<Vec<Val>, Trap>
interp.call(index, &args)?;
interp.set_fuel(1_000);                      // refill, resets the counter
interp.fuel_remaining();
interp.fuel_consumed();
```

### The cost table

`CostTable` prices instructions by class, and the defaults encode a rough guess
at relative cost:

| Class | Default | Instructions |
| --- | --- | --- |
| `constant` | 1 | `i32.const`, `i64.const` |
| `local` | 1 | `local.get`, `local.set`, `local.tee` |
| `arithmetic` | 1 | add, sub, mul, bitwise, shifts, rotates, clz/ctz/popcnt, conversions |
| `division` | 8 | `div_s`, `div_u`, `rem_s`, `rem_u` |
| `comparison` | 1 | `eqz`, `eq`, `ne`, `lt`, `gt`, `le`, `ge` |
| `control` | 1 | `nop`, `block`, `loop`, `if`, `else`, `end`, `drop`, `select`, `unreachable` |
| `branch` | 2 | `br`, `br_if`, `return` |
| `call` | 10 | `call` |

`CostTable::uniform(1)` turns fuel into a plain instruction counter, which is
what the tests use. `with_opcode_cost(opcode, cost)` overrides a single opcode,
so you can make one instruction free or ruinously expensive without touching
its class:

```rust
let costs = CostTable::uniform(1).with_opcode_cost(0x10, 100); // calls cost 100
```

### Traps

Every failure is a `Trap`, never a panic: `OutOfFuel`, `Unreachable`,
`DivisionByZero`, `IntegerOverflow`, `StackUnderflow`, `StackOverflow`,
`TypeMismatch`, `UndefinedFunction`, `CalledImport`, `CallDepthExceeded`,
`InvalidLabel`, `InvalidLocal`, `UnsupportedOpcode`, `UnsupportedBlockType`,
`UnsupportedValueType`, `MalformedBody`, `ArgumentMismatch`, `ExportNotFound`.

## Supported subset

This is the honest part. The parser accepts far more than the interpreter runs.

### The parser handles

- the module header, section framing and ordering rules
- type, import, function, export, start and code sections
- table, memory, global, element, data and data-count sections are **skipped**
  by length and recorded in `skipped_sections` - a module using them still
  parses and still lists its exports
- custom sections (names collected, contents ignored)
- LEB128 with width checks: a `u32` may not arrive in six bytes, and the
  padding bits of the final byte must extend the value correctly
- `f32` / `f64` in signatures, so they show up in `describe_exports`

### The interpreter executes

| Group | Instructions |
| --- | --- |
| control | `unreachable`, `nop`, `block`, `loop`, `if`, `else`, `end`, `br`, `br_if`, `return`, `call` |
| parametric | `drop`, `select` |
| variable | `local.get`, `local.set`, `local.tee` |
| i32 | `const`, `eqz`, `eq`, `ne`, `lt_s/u`, `gt_s/u`, `le_s/u`, `ge_s/u`, `clz`, `ctz`, `popcnt`, `add`, `sub`, `mul`, `div_s/u`, `rem_s/u`, `and`, `or`, `xor`, `shl`, `shr_s/u`, `rotl`, `rotr` |
| i64 | the same set |
| conversion | `i32.wrap_i64`, `i64.extend_i32_s`, `i64.extend_i32_u` |

Semantics follow the specification where it differs from Rust: shift counts are
taken modulo the width, `div_s` of `MIN / -1` traps with `IntegerOverflow`,
`rem_s` of `MIN % -1` is `0` rather than a trap, and division by zero traps.

### Not implemented

- **No linear memory.** `i32.load`, `i32.store` and friends are
  `UnsupportedOpcode`. Nothing in this crate allocates a heap for guest code.
- **No floats.** `f32` / `f64` parse in signatures, but calling a function that
  takes or returns one gives `UnsupportedValueType`, and every float opcode is
  unsupported.
- **No imports at run time.** There are no host bindings; calling an imported
  function is `Trap::CalledImport`. Imports are parsed so that function indices
  come out right.
- **No globals, tables or `call_indirect`.**
- **No `br_table`**, no multi-value blocks, and none of the post-MVP proposals
  (sign extension, saturating conversions, bulk memory, SIMD, reference types,
  exceptions, threads).
- **No static validation pass.** Bodies are scanned once for structure and
  illegal opcodes; type errors that a validator would reject up front instead
  surface as a `Trap::TypeMismatch` when the instruction runs. That is safe but
  it is not conformance.
- **Not a wasmtime replacement.** If you need to run real toolchain output,
  use wasmtime, which has had fuel metering for years. Use this when you want a
  few hundred lines you can audit, or when you want to understand how the
  format and the metering actually work.

Guest code cannot escape: no unsafe, no memory, no host calls, a bounded value
stack, a bounded call depth and a bounded instruction count.

## Test

```sh
cargo test
cargo clippy --all-targets
```

The suite covers LEB128 against its overflow and truncation edges, the parser
against hand-assembled modules (every module in the tests is a byte array with
the encoding spelled out in comments - no `.wat` compiler, no fixture files),
every single-byte corruption and every truncation of those modules for
robustness, and the interpreter for loops, `if`/`else`, nested calls, division
traps, recursion depth, and exact fuel accounting.

## License

MIT. See [LICENSE](LICENSE).
