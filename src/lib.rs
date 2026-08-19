//! WebAssembly binary parser and fuel-metered interpreter. See the README
//! for what is and is not implemented.

#![forbid(unsafe_code)]

pub mod leb;
mod parser;

pub use parser::{
    parse, Export, ExternKind, Func, FuncType, Import, Module, ParseError, ParseErrorKind,
    ValType,
};
