//! The binary format decoder: module header, section framing and ordering,
//! and the type/import/function/export/start/code sections. Table, memory,
//! global, element, data and data-count sections are recognized and skipped
//! by their declared length so a module using them still parses.
//!
//! Every entry point here is total: malformed input returns a `ParseError`
//! with the offset that broke, never a panic.

use std::fmt;

/// A local, function, global or table value type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValType {
    I32,
    I64,
    F32,
    F64,
}

impl fmt::Display for ValType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ValType::I32 => "i32",
            ValType::I64 => "i64",
            ValType::F32 => "f32",
            ValType::F64 => "f64",
        })
    }
}

/// A function signature: parameter types in, result types out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuncType {
    pub params: Vec<ValType>,
    pub results: Vec<ValType>,
}

impl fmt::Display for FuncType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let params: Vec<String> = self.params.iter().map(ValType::to_string).collect();
        write!(f, "({}) -> ", params.join(", "))?;
        match self.results.as_slice() {
            [] => f.write_str("()"),
            [single] => write!(f, "{single}"),
            many => {
                let results: Vec<String> = many.iter().map(ValType::to_string).collect();
                write!(f, "({})", results.join(", "))
            }
        }
    }
}

/// What kind of thing an import or export refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternKind {
    Func,
    Table,
    Memory,
    Global,
}

/// An entry from the import section. Only `type_index` is kept for function
/// imports; table/memory/global import descriptors are validated for shape
/// (so the byte stream still decodes correctly) but their details are not
/// retained, since nothing in this crate runs against them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Import {
    pub module: String,
    pub name: String,
    pub kind: ExternKind,
    pub type_index: Option<u32>,
}

/// An entry from the export section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Export {
    pub name: String,
    pub kind: ExternKind,
    pub index: u32,
}

/// A locally defined function: its signature, its expanded local
/// declarations (beyond the parameters), and its raw, undecoded body bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Func {
    pub type_index: u32,
    pub locals: Vec<ValType>,
    pub body: Vec<u8>,
}

/// A decoded module.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Module {
    pub types: Vec<FuncType>,
    pub imports: Vec<Import>,
    pub funcs: Vec<Func>,
    pub exports: Vec<Export>,
    pub start: Option<u32>,
    pub custom_sections: Vec<String>,
    pub skipped_sections: Vec<u8>,
}

impl Module {
    /// The function index exported under `name`, if any.
    pub fn export_func(&self, name: &str) -> Option<u32> {
        self.exports
            .iter()
            .find(|e| e.kind == ExternKind::Func && e.name == name)
            .map(|e| e.index)
    }

    /// How many of `imports` are function imports. Imported functions occupy
    /// the low function indices, ahead of `funcs`.
    pub fn imported_func_count(&self) -> u32 {
        self.imports
            .iter()
            .filter(|i| i.kind == ExternKind::Func)
            .count() as u32
    }

    /// The signature of function `index`, imports included.
    pub fn func_type(&self, index: u32) -> Option<&FuncType> {
        let imported = self.imported_func_count();
        if index < imported {
            let type_index = self
                .imports
                .iter()
                .filter(|i| i.kind == ExternKind::Func)
                .nth(index as usize)?
                .type_index?;
            self.types.get(type_index as usize)
        } else {
            let local = self.funcs.get((index - imported) as usize)?;
            self.types.get(local.type_index as usize)
        }
    }

    /// A human-readable line per export, e.g. `"func square: (i32) -> i32"`.
    pub fn describe_exports(&self) -> Vec<String> {
        self.exports
            .iter()
            .map(|e| match e.kind {
                ExternKind::Func => match self.func_type(e.index) {
                    Some(ty) => format!("func {}: {ty}", e.name),
                    None => format!("func {}: <unknown type>", e.name),
                },
                ExternKind::Table => format!("table {}", e.name),
                ExternKind::Memory => format!("memory {}", e.name),
                ExternKind::Global => format!("global {}", e.name),
            })
            .collect()
    }
}

/// Why parsing failed, and where.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseError {
    pub offset: usize,
    pub kind: ParseErrorKind,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at byte {}", self.kind, self.offset)
    }
}

impl std::error::Error for ParseError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseErrorKind {
    NotWasm,
    UnsupportedVersion,
    UnexpectedEof,
    Leb,
    UnknownSectionId,
    SectionOutOfOrder,
    SectionSizeMismatch,
    InvalidValType,
    InvalidFuncType,
    InvalidExternKind,
    InvalidLimits,
    InvalidUtf8,
    FunctionCodeMismatch,
    TypeIndexOutOfRange,
    TooManyLocals,
    MissingEnd,
}

impl fmt::Display for ParseErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ParseErrorKind::NotWasm => "not a wasm module",
            ParseErrorKind::UnsupportedVersion => "unsupported version",
            ParseErrorKind::UnexpectedEof => "unexpected end of file",
            ParseErrorKind::Leb => "malformed LEB128 value",
            ParseErrorKind::UnknownSectionId => "unknown section id",
            ParseErrorKind::SectionOutOfOrder => "section out of order",
            ParseErrorKind::SectionSizeMismatch => "section size does not match its contents",
            ParseErrorKind::InvalidValType => "invalid value type",
            ParseErrorKind::InvalidFuncType => "invalid function type",
            ParseErrorKind::InvalidExternKind => "invalid extern kind",
            ParseErrorKind::InvalidLimits => "invalid limits",
            ParseErrorKind::InvalidUtf8 => "invalid utf-8",
            ParseErrorKind::FunctionCodeMismatch => "function and code section counts differ",
            ParseErrorKind::TypeIndexOutOfRange => "type index out of range",
            ParseErrorKind::TooManyLocals => "too many locals",
            ParseErrorKind::MissingEnd => "function body missing end opcode",
        })
    }
}

/// Above this many expanded locals in one function body, refuse to keep
/// going. A crafted `(count, type)` pair can claim billions of locals in a
/// handful of bytes; this bounds the allocation to something a real module
/// would never need.
const MAX_LOCALS: u64 = 50_000;

fn err(offset: usize, kind: ParseErrorKind) -> ParseError {
    ParseError { offset, kind }
}

fn read_byte(bytes: &[u8], pos: &mut usize) -> Result<u8, ParseError> {
    let byte = *bytes
        .get(*pos)
        .ok_or_else(|| err(*pos, ParseErrorKind::UnexpectedEof))?;
    *pos += 1;
    Ok(byte)
}

fn read_bytes<'a>(bytes: &'a [u8], pos: &mut usize, len: usize) -> Result<&'a [u8], ParseError> {
    let start = *pos;
    let end = start
        .checked_add(len)
        .ok_or_else(|| err(start, ParseErrorKind::UnexpectedEof))?;
    if end > bytes.len() {
        return Err(err(start, ParseErrorKind::UnexpectedEof));
    }
    *pos = end;
    Ok(&bytes[start..end])
}

fn read_u32(bytes: &[u8], pos: &mut usize) -> Result<u32, ParseError> {
    let start = *pos;
    crate::leb::read_u32(bytes, pos).map_err(|_| err(start, ParseErrorKind::Leb))
}

fn read_name(bytes: &[u8], pos: &mut usize) -> Result<String, ParseError> {
    let start = *pos;
    let len = read_u32(bytes, pos)? as usize;
    let raw = read_bytes(bytes, pos, len)?;
    String::from_utf8(raw.to_vec()).map_err(|_| err(start, ParseErrorKind::InvalidUtf8))
}

fn read_valtype(bytes: &[u8], pos: &mut usize) -> Result<ValType, ParseError> {
    let start = *pos;
    match read_byte(bytes, pos)? {
        0x7F => Ok(ValType::I32),
        0x7E => Ok(ValType::I64),
        0x7D => Ok(ValType::F32),
        0x7C => Ok(ValType::F64),
        _ => Err(err(start, ParseErrorKind::InvalidValType)),
    }
}

fn read_vec<T>(
    bytes: &[u8],
    pos: &mut usize,
    mut item: impl FnMut(&[u8], &mut usize) -> Result<T, ParseError>,
) -> Result<Vec<T>, ParseError> {
    let count = read_u32(bytes, pos)? as usize;
    let mut out = Vec::new();
    for _ in 0..count {
        out.push(item(bytes, pos)?);
    }
    Ok(out)
}

fn check_section_size(pos: usize, content_end: usize) -> Result<(), ParseError> {
    if pos == content_end {
        Ok(())
    } else {
        Err(err(pos, ParseErrorKind::SectionSizeMismatch))
    }
}

fn read_functype(bytes: &[u8], pos: &mut usize) -> Result<FuncType, ParseError> {
    let start = *pos;
    if read_byte(bytes, pos)? != 0x60 {
        return Err(err(start, ParseErrorKind::InvalidFuncType));
    }
    let params = read_vec(bytes, pos, read_valtype)?;
    let results = read_vec(bytes, pos, read_valtype)?;
    Ok(FuncType { params, results })
}

fn read_limits(bytes: &[u8], pos: &mut usize) -> Result<(), ParseError> {
    let start = *pos;
    match read_byte(bytes, pos)? {
        0x00 => {
            read_u32(bytes, pos)?;
            Ok(())
        }
        0x01 => {
            read_u32(bytes, pos)?;
            read_u32(bytes, pos)?;
            Ok(())
        }
        _ => Err(err(start, ParseErrorKind::InvalidLimits)),
    }
}

fn read_import(bytes: &[u8], pos: &mut usize, types_len: usize) -> Result<Import, ParseError> {
    let module = read_name(bytes, pos)?;
    let name = read_name(bytes, pos)?;
    let start = *pos;
    match read_byte(bytes, pos)? {
        0x00 => {
            let type_index_start = *pos;
            let type_index = read_u32(bytes, pos)?;
            if type_index as usize >= types_len {
                return Err(err(type_index_start, ParseErrorKind::TypeIndexOutOfRange));
            }
            Ok(Import {
                module,
                name,
                kind: ExternKind::Func,
                type_index: Some(type_index),
            })
        }
        0x01 => {
            read_byte(bytes, pos)?; // element type, unused: tables aren't executed
            read_limits(bytes, pos)?;
            Ok(Import { module, name, kind: ExternKind::Table, type_index: None })
        }
        0x02 => {
            read_limits(bytes, pos)?;
            Ok(Import { module, name, kind: ExternKind::Memory, type_index: None })
        }
        0x03 => {
            read_valtype(bytes, pos)?;
            read_byte(bytes, pos)?; // mutability
            Ok(Import { module, name, kind: ExternKind::Global, type_index: None })
        }
        _ => Err(err(start, ParseErrorKind::InvalidExternKind)),
    }
}

fn read_export(bytes: &[u8], pos: &mut usize) -> Result<Export, ParseError> {
    let name = read_name(bytes, pos)?;
    let start = *pos;
    let kind = match read_byte(bytes, pos)? {
        0x00 => ExternKind::Func,
        0x01 => ExternKind::Table,
        0x02 => ExternKind::Memory,
        0x03 => ExternKind::Global,
        _ => return Err(err(start, ParseErrorKind::InvalidExternKind)),
    };
    let index = read_u32(bytes, pos)?;
    Ok(Export { name, kind, index })
}

fn parse_type_section(
    bytes: &[u8],
    pos: &mut usize,
    content_end: usize,
) -> Result<Vec<FuncType>, ParseError> {
    let types = read_vec(bytes, pos, read_functype)?;
    check_section_size(*pos, content_end)?;
    Ok(types)
}

fn parse_import_section(
    bytes: &[u8],
    pos: &mut usize,
    content_end: usize,
    types_len: usize,
) -> Result<Vec<Import>, ParseError> {
    let imports = read_vec(bytes, pos, |b, p| read_import(b, p, types_len))?;
    check_section_size(*pos, content_end)?;
    Ok(imports)
}

fn parse_function_section(
    bytes: &[u8],
    pos: &mut usize,
    content_end: usize,
    types_len: usize,
) -> Result<Vec<u32>, ParseError> {
    let indices = read_vec(bytes, pos, |b, p| {
        let start = *p;
        let idx = read_u32(b, p)?;
        if idx as usize >= types_len {
            return Err(err(start, ParseErrorKind::TypeIndexOutOfRange));
        }
        Ok(idx)
    })?;
    check_section_size(*pos, content_end)?;
    Ok(indices)
}

fn parse_export_section(
    bytes: &[u8],
    pos: &mut usize,
    content_end: usize,
) -> Result<Vec<Export>, ParseError> {
    let exports = read_vec(bytes, pos, read_export)?;
    check_section_size(*pos, content_end)?;
    Ok(exports)
}

fn parse_start_section(
    bytes: &[u8],
    pos: &mut usize,
    content_end: usize,
) -> Result<u32, ParseError> {
    let index = read_u32(bytes, pos)?;
    check_section_size(*pos, content_end)?;
    Ok(index)
}

fn parse_code_section(
    bytes: &[u8],
    pos: &mut usize,
    content_end: usize,
    func_type_indices: &[u32],
) -> Result<Vec<Func>, ParseError> {
    let count_start = *pos;
    let count = read_u32(bytes, pos)? as usize;
    if count != func_type_indices.len() {
        return Err(err(count_start, ParseErrorKind::FunctionCodeMismatch));
    }

    let mut funcs = Vec::with_capacity(count);
    for &type_index in func_type_indices {
        let body_size_start = *pos;
        let body_size = read_u32(bytes, pos)? as usize;
        let body_start = *pos;
        let body_end = body_start
            .checked_add(body_size)
            .filter(|&end| end <= content_end)
            .ok_or_else(|| err(body_size_start, ParseErrorKind::UnexpectedEof))?;

        let local_group_count = read_u32(bytes, pos)? as usize;
        let mut locals = Vec::new();
        let mut total_locals: u64 = 0;
        for _ in 0..local_group_count {
            let group_start = *pos;
            let n = read_u32(bytes, pos)?;
            let vt = read_valtype(bytes, pos)?;
            total_locals += u64::from(n);
            if total_locals > MAX_LOCALS {
                return Err(err(group_start, ParseErrorKind::TooManyLocals));
            }
            locals.extend(std::iter::repeat(vt).take(n as usize));
        }
        if *pos > body_end {
            return Err(err(*pos, ParseErrorKind::SectionSizeMismatch));
        }

        let body = bytes[*pos..body_end].to_vec();
        if body.last() != Some(&0x0B) {
            return Err(err(body_end.saturating_sub(1), ParseErrorKind::MissingEnd));
        }
        *pos = body_end;
        funcs.push(Func { type_index, locals, body });
    }

    check_section_size(*pos, content_end)?;
    Ok(funcs)
}

/// Decodes a WebAssembly binary module.
pub fn parse(bytes: &[u8]) -> Result<Module, ParseError> {
    if !bytes.starts_with(b"\0asm") {
        return Err(err(0, ParseErrorKind::NotWasm));
    }
    let mut pos = 4usize;
    let version_bytes = read_bytes(bytes, &mut pos, 4)?;
    let version = u32::from_le_bytes(version_bytes.try_into().unwrap());
    if version != 1 {
        return Err(err(4, ParseErrorKind::UnsupportedVersion));
    }

    let mut module = Module::default();
    let mut last_id: i32 = -1;
    let mut func_type_indices: Option<Vec<u32>> = None;
    let mut code_seen = false;

    while pos < bytes.len() {
        let section_start = pos;
        let id = read_byte(bytes, &mut pos)?;
        let size = read_u32(bytes, &mut pos)? as usize;
        let content_start = pos;
        let content_end = content_start
            .checked_add(size)
            .filter(|&end| end <= bytes.len())
            .ok_or_else(|| err(content_start, ParseErrorKind::UnexpectedEof))?;

        if id == 0 {
            let mut cpos = content_start;
            let name = read_name(bytes, &mut cpos)?;
            if cpos > content_end {
                return Err(err(content_end, ParseErrorKind::SectionSizeMismatch));
            }
            module.custom_sections.push(name);
            pos = content_end;
            continue;
        }

        if i32::from(id) <= last_id {
            return Err(err(section_start, ParseErrorKind::SectionOutOfOrder));
        }
        last_id = i32::from(id);

        match id {
            1 => module.types = parse_type_section(bytes, &mut pos, content_end)?,
            2 => {
                let types_len = module.types.len();
                module.imports = parse_import_section(bytes, &mut pos, content_end, types_len)?;
            }
            3 => {
                let types_len = module.types.len();
                func_type_indices =
                    Some(parse_function_section(bytes, &mut pos, content_end, types_len)?);
            }
            4 | 5 | 6 | 9 | 11 | 12 => {
                module.skipped_sections.push(id);
                pos = content_end;
            }
            7 => module.exports = parse_export_section(bytes, &mut pos, content_end)?,
            8 => module.start = Some(parse_start_section(bytes, &mut pos, content_end)?),
            10 => {
                let indices = func_type_indices.as_deref().unwrap_or(&[]);
                module.funcs = parse_code_section(bytes, &mut pos, content_end, indices)?;
                code_seen = true;
            }
            _ => return Err(err(section_start, ParseErrorKind::UnknownSectionId)),
        }
    }

    if let Some(indices) = &func_type_indices {
        if !code_seen && !indices.is_empty() {
            return Err(err(bytes.len(), ParseErrorKind::FunctionCodeMismatch));
        }
    }

    Ok(module)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: [u8; 8] = [0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00];

    fn module_bytes(sections: &[&[u8]]) -> Vec<u8> {
        let mut out = HEADER.to_vec();
        for section in sections {
            out.extend_from_slice(section);
        }
        out
    }

    #[test]
    fn parses_the_empty_module() {
        let module = parse(&HEADER).unwrap();
        assert_eq!(module, Module::default());
    }

    #[test]
    fn parses_the_square_function_from_the_readme() {
        const SQUARE: &[u8] = &[
            0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00, 0x01, 0x06, 0x01, 0x60, 0x01, 0x7F,
            0x01, 0x7F, 0x03, 0x02, 0x01, 0x00, 0x07, 0x0A, 0x01, 0x06, 0x73, 0x71, 0x75, 0x61,
            0x72, 0x65, 0x00, 0x00, 0x0A, 0x09, 0x01, 0x07, 0x00, 0x20, 0x00, 0x20, 0x00, 0x6C,
            0x0B,
        ];
        let module = parse(SQUARE).unwrap();

        assert_eq!(module.types, vec![FuncType { params: vec![ValType::I32], results: vec![ValType::I32] }]);
        assert_eq!(module.funcs.len(), 1);
        assert_eq!(module.funcs[0].type_index, 0);
        assert_eq!(module.funcs[0].locals, vec![]);
        assert_eq!(module.funcs[0].body, vec![0x20, 0x00, 0x20, 0x00, 0x6C, 0x0B]);
        assert_eq!(module.export_func("square"), Some(0));
        assert_eq!(module.describe_exports(), vec!["func square: (i32) -> i32"]);
        assert_eq!(module.imported_func_count(), 0);
    }

    #[test]
    fn rejects_bad_magic() {
        let error = parse(b"not-wasm-at-all").unwrap_err();
        assert_eq!(error, err(0, ParseErrorKind::NotWasm));
    }

    #[test]
    fn rejects_unsupported_version() {
        let bytes = [0x00, 0x61, 0x73, 0x6D, 0x02, 0x00, 0x00, 0x00];
        let error = parse(&bytes).unwrap_err();
        assert_eq!(error, err(4, ParseErrorKind::UnsupportedVersion));
    }

    #[test]
    fn rejects_a_truncated_header() {
        let error = parse(&HEADER[..4]).unwrap_err();
        assert_eq!(error, err(4, ParseErrorKind::UnexpectedEof));
    }

    #[test]
    fn rejects_sections_out_of_order() {
        let empty_export_section: &[u8] = &[0x07, 0x01, 0x00];
        let bytes = module_bytes(&[empty_export_section, empty_export_section]);
        let error = parse(&bytes).unwrap_err();
        assert_eq!(error, err(HEADER.len() + empty_export_section.len(), ParseErrorKind::SectionOutOfOrder));
    }

    #[test]
    fn skips_unhandled_sections_by_length() {
        let empty_memory_section: &[u8] = &[0x05, 0x00];
        let bytes = module_bytes(&[empty_memory_section]);
        let module = parse(&bytes).unwrap();
        assert_eq!(module.skipped_sections, vec![5]);
    }

    #[test]
    fn records_custom_section_names() {
        let custom_section: &[u8] = &[0x00, 0x03, 0x02, 0x68, 0x69]; // name "hi"
        let bytes = module_bytes(&[custom_section]);
        let module = parse(&bytes).unwrap();
        assert_eq!(module.custom_sections, vec!["hi".to_string()]);
    }

    #[test]
    fn rejects_function_code_count_mismatch() {
        let type_section: &[u8] = &[0x01, 0x04, 0x01, 0x60, 0x00, 0x00]; // () -> ()
        let function_section: &[u8] = &[0x03, 0x02, 0x01, 0x00]; // 1 func, type 0
        let empty_code_section: &[u8] = &[0x0A, 0x01, 0x00]; // 0 code entries
        let bytes = module_bytes(&[type_section, function_section, empty_code_section]);
        let error = parse(&bytes).unwrap_err();
        assert_eq!(error.kind, ParseErrorKind::FunctionCodeMismatch);
    }

    #[test]
    fn rejects_a_body_missing_the_end_opcode() {
        let type_section: &[u8] = &[0x01, 0x04, 0x01, 0x60, 0x00, 0x00];
        let function_section: &[u8] = &[0x03, 0x02, 0x01, 0x00];
        // 1 code entry, body_size 2, 0 locals, one non-`end` byte.
        let code_section: &[u8] = &[0x0A, 0x04, 0x01, 0x02, 0x00, 0x01];
        let bytes = module_bytes(&[type_section, function_section, code_section]);
        let error = parse(&bytes).unwrap_err();
        assert_eq!(error.kind, ParseErrorKind::MissingEnd);
    }

    #[test]
    fn rejects_unknown_section_ids() {
        let bogus_section: &[u8] = &[0x0D, 0x00]; // id 13 does not exist
        let bytes = module_bytes(&[bogus_section]);
        let error = parse(&bytes).unwrap_err();
        assert_eq!(error, err(HEADER.len(), ParseErrorKind::UnknownSectionId));
    }
}
