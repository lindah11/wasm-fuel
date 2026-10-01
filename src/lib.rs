//! WebAssembly binary parser and fuel-metered interpreter. See the README
//! for what is and is not implemented.

#![forbid(unsafe_code)]

mod interp;
pub mod leb;
mod parser;

pub use interp::{CostTable, Interpreter, Trap, Val};
pub use parser::{
    parse, Export, ExternKind, Func, FuncType, Import, Module, ParseError, ParseErrorKind,
    ValType,
};
