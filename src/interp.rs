//! A fuel-metered stack interpreter for an integer-only subset of the
//! instruction set.
//!
//! Fuel is charged before an instruction runs, so a body that would overrun
//! its budget stops at the instruction that cannot be paid for instead of one
//! past it. Nothing here panics on guest input: every failure is a `Trap`.
//!
//! There is no validation pass. A function body is scanned once, the first
//! time it is called, to find where each block ends and to reject opcodes and
//! immediates the interpreter does not handle; type errors that a validator
//! would catch up front show up as `Trap::TypeMismatch` when they execute.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;

use crate::leb;
use crate::parser::{Func, Module, ValType};

/// A runtime value. Only the integer types are executable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Val {
    I32(i32),
    I64(i64),
}

impl Val {
    fn ty(self) -> ValType {
        match self {
            Val::I32(_) => ValType::I32,
            Val::I64(_) => ValType::I64,
        }
    }
}

/// Every way execution can stop short of returning normally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trap {
    OutOfFuel,
    Unreachable,
    DivisionByZero,
    IntegerOverflow,
    StackUnderflow,
    StackOverflow,
    TypeMismatch,
    UndefinedFunction,
    CalledImport,
    CallDepthExceeded,
    InvalidLabel,
    InvalidLocal,
    UnsupportedOpcode(u8),
    UnsupportedBlockType,
    UnsupportedValueType,
    MalformedBody,
    ArgumentMismatch,
    ExportNotFound,
}

impl fmt::Display for Trap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Trap::OutOfFuel => f.write_str("out of fuel"),
            Trap::Unreachable => f.write_str("unreachable executed"),
            Trap::DivisionByZero => f.write_str("integer divide by zero"),
            Trap::IntegerOverflow => f.write_str("integer overflow"),
            Trap::StackUnderflow => f.write_str("value stack underflow"),
            Trap::StackOverflow => f.write_str("value stack overflow"),
            Trap::TypeMismatch => f.write_str("operand type mismatch"),
            Trap::UndefinedFunction => f.write_str("undefined function"),
            Trap::CalledImport => f.write_str("imported functions cannot be called"),
            Trap::CallDepthExceeded => f.write_str("call depth exceeded"),
            Trap::InvalidLabel => f.write_str("branch to a label that does not exist"),
            Trap::InvalidLocal => f.write_str("local index out of range"),
            Trap::UnsupportedOpcode(op) => write!(f, "unsupported opcode 0x{op:02X}"),
            Trap::UnsupportedBlockType => f.write_str("unsupported block type"),
            Trap::UnsupportedValueType => f.write_str("unsupported value type"),
            Trap::MalformedBody => f.write_str("malformed function body"),
            Trap::ArgumentMismatch => f.write_str("arguments do not match the function signature"),
            Trap::ExportNotFound => f.write_str("no such exported function"),
        }
    }
}

impl std::error::Error for Trap {}

#[derive(Clone, Copy)]
enum Class {
    Constant,
    Local,
    Arithmetic,
    Division,
    Comparison,
    Control,
    Branch,
    Call,
}

/// The cost class of an opcode, or `None` if the interpreter cannot run it.
fn class(op: u8) -> Option<Class> {
    Some(match op {
        0x41 | 0x42 => Class::Constant,
        0x20..=0x22 => Class::Local,
        0x6D..=0x70 | 0x7F..=0x82 => Class::Division,
        0x67..=0x8A | 0xA7 | 0xAC | 0xAD => Class::Arithmetic,
        0x45..=0x5A => Class::Comparison,
        0x00..=0x05 | 0x0B | 0x1A | 0x1B => Class::Control,
        0x0C | 0x0D | 0x0F => Class::Branch,
        0x10 => Class::Call,
        _ => return None,
    })
}

/// What each class of instruction costs in fuel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CostTable {
    pub constant: u64,
    pub local: u64,
    pub arithmetic: u64,
    pub division: u64,
    pub comparison: u64,
    pub control: u64,
    pub branch: u64,
    pub call: u64,
    overrides: HashMap<u8, u64>,
}

impl Default for CostTable {
    fn default() -> Self {
        CostTable {
            constant: 1,
            local: 1,
            arithmetic: 1,
            division: 8,
            comparison: 1,
            control: 1,
            branch: 2,
            call: 10,
            overrides: HashMap::new(),
        }
    }
}

impl CostTable {
    /// Every instruction costs `cost`, which makes fuel an instruction counter
    /// when `cost` is 1.
    pub fn uniform(cost: u64) -> Self {
        CostTable {
            constant: cost,
            local: cost,
            arithmetic: cost,
            division: cost,
            comparison: cost,
            control: cost,
            branch: cost,
            call: cost,
            overrides: HashMap::new(),
        }
    }

    /// Prices one opcode, taking precedence over its class.
    pub fn with_opcode_cost(mut self, opcode: u8, cost: u64) -> Self {
        self.overrides.insert(opcode, cost);
        self
    }

    fn cost(&self, op: u8) -> u64 {
        if let Some(&cost) = self.overrides.get(&op) {
            return cost;
        }
        match class(op).unwrap_or(Class::Control) {
            Class::Constant => self.constant,
            Class::Local => self.local,
            Class::Arithmetic => self.arithmetic,
            Class::Division => self.division,
            Class::Comparison => self.comparison,
            Class::Control => self.control,
            Class::Branch => self.branch,
            Class::Call => self.call,
        }
    }
}

/// Structure found by the one-time scan of a function body.
struct Scan {
    /// Offset of each `block`/`loop`/`if` opcode to the offset of its `end`.
    ends: HashMap<usize, usize>,
    /// Offset of each `if` opcode that has an `else` to that `else`.
    elses: HashMap<usize, usize>,
}

fn imm_u32(body: &[u8], pos: &mut usize) -> Result<u32, Trap> {
    leb::read_u32(body, pos).map_err(|_| Trap::MalformedBody)
}

/// Reads a block type immediate and returns how many results the block has.
/// Type-index block types need multi-value support, which isn't here.
fn block_arity(body: &[u8], pos: &mut usize) -> Result<usize, Trap> {
    let byte = *body.get(*pos).ok_or(Trap::MalformedBody)?;
    *pos += 1;
    match byte {
        0x40 => Ok(0),
        0x7F | 0x7E => Ok(1),
        0x7D | 0x7C => Err(Trap::UnsupportedValueType),
        _ => Err(Trap::UnsupportedBlockType),
    }
}

fn scan(body: &[u8]) -> Result<Scan, Trap> {
    let mut found = Scan { ends: HashMap::new(), elses: HashMap::new() };
    let mut open: Vec<usize> = Vec::new();
    let mut pos = 0;
    loop {
        let at = pos;
        let op = *body.get(pos).ok_or(Trap::MalformedBody)?;
        pos += 1;
        match op {
            0x02..=0x04 => {
                block_arity(body, &mut pos)?;
                open.push(at);
            }
            0x05 => {
                let &top = open.last().ok_or(Trap::MalformedBody)?;
                if body[top] != 0x04 || found.elses.insert(top, at).is_some() {
                    return Err(Trap::MalformedBody);
                }
            }
            0x0B => match open.pop() {
                Some(start) => {
                    found.ends.insert(start, at);
                }
                None => {
                    // The function's own `end` has to be the last byte.
                    if pos != body.len() {
                        return Err(Trap::MalformedBody);
                    }
                    return Ok(found);
                }
            },
            0x0C | 0x0D | 0x10 | 0x20..=0x22 => {
                imm_u32(body, &mut pos)?;
            }
            0x41 => {
                leb::read_i32(body, &mut pos).map_err(|_| Trap::MalformedBody)?;
            }
            0x42 => {
                leb::read_i64(body, &mut pos).map_err(|_| Trap::MalformedBody)?;
            }
            _ => {
                if class(op).is_none() {
                    return Err(Trap::UnsupportedOpcode(op));
                }
            }
        }
    }
}

/// An open `block`, `loop` or `if` while a body executes.
#[derive(Clone, Copy)]
struct Label {
    is_loop: bool,
    /// Where execution resumes when this label is branched to: the top of the
    /// body for a loop, the byte after the `end` otherwise.
    cont: usize,
    /// Value stack height when the construct was entered.
    height: usize,
    /// How many values a branch to this label carries. Always 0 for a loop,
    /// since loops take no parameters here.
    arity: usize,
}

pub struct Interpreter<'m> {
    module: &'m Module,
    costs: CostTable,
    fuel: u64,
    consumed: u64,
    max_depth: usize,
    max_stack: usize,
    stack: Vec<Val>,
    scans: Vec<Option<Rc<Scan>>>,
}

impl<'m> Interpreter<'m> {
    /// A fresh interpreter with the default cost table, one million fuel, a
    /// call depth of 256 and a value stack of 16384 entries.
    pub fn new(module: &'m Module) -> Self {
        Interpreter {
            module,
            costs: CostTable::default(),
            fuel: 1_000_000,
            consumed: 0,
            max_depth: 256,
            max_stack: 16 * 1024,
            stack: Vec::new(),
            scans: vec![None; module.funcs.len()],
        }
    }

    pub fn with_costs(mut self, costs: CostTable) -> Self {
        self.costs = costs;
        self
    }

    pub fn with_fuel(mut self, fuel: u64) -> Self {
        self.set_fuel(fuel);
        self
    }

    pub fn with_max_call_depth(mut self, depth: usize) -> Self {
        self.max_depth = depth;
        self
    }

    pub fn with_max_stack(mut self, entries: usize) -> Self {
        self.max_stack = entries;
        self
    }

    /// Refills the budget and resets the consumed counter.
    pub fn set_fuel(&mut self, fuel: u64) {
        self.fuel = fuel;
        self.consumed = 0;
    }

    pub fn fuel_remaining(&self) -> u64 {
        self.fuel
    }

    /// Fuel spent since the last `set_fuel` (or construction), across calls.
    pub fn fuel_consumed(&self) -> u64 {
        self.consumed
    }

    pub fn call_export(&mut self, name: &str, args: &[Val]) -> Result<Vec<Val>, Trap> {
        let index = self.module.export_func(name).ok_or(Trap::ExportNotFound)?;
        self.call(index, args)
    }

    pub fn call(&mut self, index: u32, args: &[Val]) -> Result<Vec<Val>, Trap> {
        let module = self.module;
        let ty = module.func_type(index).ok_or(Trap::UndefinedFunction)?;
        if ty.params.iter().chain(&ty.results).any(|t| !matches!(t, ValType::I32 | ValType::I64)) {
            return Err(Trap::UnsupportedValueType);
        }
        if args.len() != ty.params.len() || args.iter().zip(&ty.params).any(|(a, p)| a.ty() != *p) {
            return Err(Trap::ArgumentMismatch);
        }

        self.stack.clear();
        for &arg in args {
            self.push(arg)?;
        }
        self.invoke(index, 0)?;

        let start = self
            .stack
            .len()
            .checked_sub(ty.results.len())
            .ok_or(Trap::StackUnderflow)?;
        let results = self.stack.split_off(start);
        self.stack.clear();
        Ok(results)
    }

    fn charge(&mut self, op: u8) -> Result<(), Trap> {
        let cost = self.costs.cost(op);
        if cost > self.fuel {
            // Spend what is left so the counter reflects the work done.
            self.consumed = self.consumed.saturating_add(self.fuel);
            self.fuel = 0;
            return Err(Trap::OutOfFuel);
        }
        self.fuel -= cost;
        self.consumed = self.consumed.saturating_add(cost);
        Ok(())
    }

    fn push(&mut self, val: Val) -> Result<(), Trap> {
        if self.stack.len() >= self.max_stack {
            return Err(Trap::StackOverflow);
        }
        self.stack.push(val);
        Ok(())
    }

    /// Pops a value, refusing to reach below the current function's frame.
    fn pop(&mut self, base: usize) -> Result<Val, Trap> {
        if self.stack.len() <= base {
            return Err(Trap::StackUnderflow);
        }
        self.stack.pop().ok_or(Trap::StackUnderflow)
    }

    fn pop_i32(&mut self, base: usize) -> Result<i32, Trap> {
        match self.pop(base)? {
            Val::I32(v) => Ok(v),
            Val::I64(_) => Err(Trap::TypeMismatch),
        }
    }

    fn pop_i64(&mut self, base: usize) -> Result<i64, Trap> {
        match self.pop(base)? {
            Val::I64(v) => Ok(v),
            Val::I32(_) => Err(Trap::TypeMismatch),
        }
    }

    /// Discards everything above `height` except the top `arity` values.
    fn unwind(&mut self, height: usize, arity: usize) -> Result<(), Trap> {
        let len = self.stack.len();
        if len < height + arity {
            return Err(Trap::StackUnderflow);
        }
        self.stack.drain(height..len - arity);
        Ok(())
    }

    fn scan_for(&mut self, local_index: usize, func: &Func) -> Result<Rc<Scan>, Trap> {
        if let Some(Some(cached)) = self.scans.get(local_index) {
            return Ok(Rc::clone(cached));
        }
        let fresh = Rc::new(scan(&func.body)?);
        if let Some(slot) = self.scans.get_mut(local_index) {
            *slot = Some(Rc::clone(&fresh));
        }
        Ok(fresh)
    }

    /// Runs function `index` against the arguments on top of the value stack,
    /// leaving its results there.
    fn invoke(&mut self, index: u32, depth: usize) -> Result<(), Trap> {
        if depth >= self.max_depth {
            return Err(Trap::CallDepthExceeded);
        }
        let module = self.module;
        let imported = module.imported_func_count();
        if index < imported {
            return Err(Trap::CalledImport);
        }
        let local_index = (index - imported) as usize;
        let func = module.funcs.get(local_index).ok_or(Trap::UndefinedFunction)?;
        let ty = module
            .types
            .get(func.type_index as usize)
            .ok_or(Trap::UndefinedFunction)?;
        let scan = self.scan_for(local_index, func)?;

        let base = self
            .stack
            .len()
            .checked_sub(ty.params.len())
            .ok_or(Trap::StackUnderflow)?;
        let mut locals: Vec<Val> = self.stack.drain(base..).collect();
        for local in &func.locals {
            locals.push(match local {
                ValType::I32 => Val::I32(0),
                ValType::I64 => Val::I64(0),
                ValType::F32 | ValType::F64 => return Err(Trap::UnsupportedValueType),
            });
        }
        self.run(func, &scan, locals, ty.results.len(), base, depth)
    }

    /// Takes a branch to the label `n` levels out. Returns false when the
    /// branch targets the function itself, meaning the caller should return.
    fn branch(
        &mut self,
        n: u32,
        labels: &mut Vec<Label>,
        pc: &mut usize,
        base: usize,
        results: usize,
    ) -> Result<bool, Trap> {
        let n = n as usize;
        if n == labels.len() {
            self.unwind(base, results)?;
            return Ok(false);
        }
        if n > labels.len() {
            return Err(Trap::InvalidLabel);
        }
        let idx = labels.len() - 1 - n;
        let label = labels[idx];
        self.unwind(label.height, label.arity)?;
        // A loop stays open so the next iteration can branch to it again.
        labels.truncate(if label.is_loop { idx + 1 } else { idx });
        *pc = label.cont;
        Ok(true)
    }

    fn run(
        &mut self,
        func: &Func,
        scan: &Scan,
        mut locals: Vec<Val>,
        results: usize,
        base: usize,
        depth: usize,
    ) -> Result<(), Trap> {
        let body = &func.body[..];
        let mut pc = 0usize;
        let mut labels: Vec<Label> = Vec::new();
        loop {
            let at = pc;
            let op = *body.get(pc).ok_or(Trap::MalformedBody)?;
            pc += 1;
            self.charge(op)?;
            match op {
                0x00 => return Err(Trap::Unreachable),
                0x01 => {}
                0x02 | 0x03 => {
                    let arity = block_arity(body, &mut pc)?;
                    let height = self.stack.len();
                    if op == 0x02 {
                        let end = *scan.ends.get(&at).ok_or(Trap::MalformedBody)?;
                        labels.push(Label { is_loop: false, cont: end + 1, height, arity });
                    } else {
                        labels.push(Label { is_loop: true, cont: pc, height, arity: 0 });
                    }
                }
                0x04 => {
                    let arity = block_arity(body, &mut pc)?;
                    let cond = self.pop_i32(base)?;
                    let end = *scan.ends.get(&at).ok_or(Trap::MalformedBody)?;
                    let height = self.stack.len();
                    let label = Label { is_loop: false, cont: end + 1, height, arity };
                    if cond != 0 {
                        labels.push(label);
                    } else if let Some(&else_at) = scan.elses.get(&at) {
                        labels.push(label);
                        pc = else_at + 1;
                    } else {
                        pc = end + 1;
                    }
                }
                0x05 => {
                    // Falling out of the then-arm: skip the else-arm.
                    let label = labels.pop().ok_or(Trap::MalformedBody)?;
                    self.unwind(label.height, label.arity)?;
                    pc = label.cont;
                }
                0x0B => match labels.pop() {
                    Some(label) => self.unwind(label.height, label.arity)?,
                    None => {
                        self.unwind(base, results)?;
                        return Ok(());
                    }
                },
                0x0C => {
                    let n = imm_u32(body, &mut pc)?;
                    if !self.branch(n, &mut labels, &mut pc, base, results)? {
                        return Ok(());
                    }
                }
                0x0D => {
                    let n = imm_u32(body, &mut pc)?;
                    if self.pop_i32(base)? != 0
                        && !self.branch(n, &mut labels, &mut pc, base, results)?
                    {
                        return Ok(());
                    }
                }
                0x0F => {
                    self.unwind(base, results)?;
                    return Ok(());
                }
                0x10 => {
                    let callee = imm_u32(body, &mut pc)?;
                    self.invoke(callee, depth + 1)?;
                }
                0x1A => {
                    self.pop(base)?;
                }
                0x1B => {
                    let cond = self.pop_i32(base)?;
                    let second = self.pop(base)?;
                    let first = self.pop(base)?;
                    if first.ty() != second.ty() {
                        return Err(Trap::TypeMismatch);
                    }
                    self.push(if cond != 0 { first } else { second })?;
                }
                0x20 => {
                    let i = imm_u32(body, &mut pc)? as usize;
                    let val = *locals.get(i).ok_or(Trap::InvalidLocal)?;
                    self.push(val)?;
                }
                0x21 | 0x22 => {
                    let i = imm_u32(body, &mut pc)? as usize;
                    let val = self.pop(base)?;
                    let slot = locals.get_mut(i).ok_or(Trap::InvalidLocal)?;
                    if slot.ty() != val.ty() {
                        return Err(Trap::TypeMismatch);
                    }
                    *slot = val;
                    if op == 0x22 {
                        self.push(val)?;
                    }
                }
                0x41 => {
                    let v = leb::read_i32(body, &mut pc).map_err(|_| Trap::MalformedBody)?;
                    self.push(Val::I32(v))?;
                }
                0x42 => {
                    let v = leb::read_i64(body, &mut pc).map_err(|_| Trap::MalformedBody)?;
                    self.push(Val::I64(v))?;
                }
                _ => self.numeric(op, base)?,
            }
        }
    }

    fn numeric(&mut self, op: u8, base: usize) -> Result<(), Trap> {
        match op {
            0x45 => {
                let a = self.pop_i32(base)?;
                self.push(Val::I32((a == 0) as i32))
            }
            0x46..=0x4F => {
                let b = self.pop_i32(base)?;
                let a = self.pop_i32(base)?;
                let holds = compare(op - 0x46, a.cmp(&b), (a as u32).cmp(&(b as u32)));
                self.push(Val::I32(holds as i32))
            }
            0x50 => {
                let a = self.pop_i64(base)?;
                self.push(Val::I32((a == 0) as i32))
            }
            0x51..=0x5A => {
                let b = self.pop_i64(base)?;
                let a = self.pop_i64(base)?;
                let holds = compare(op - 0x51, a.cmp(&b), (a as u64).cmp(&(b as u64)));
                self.push(Val::I32(holds as i32))
            }
            0x67..=0x69 => {
                let a = self.pop_i32(base)? as u32;
                let n = match op {
                    0x67 => a.leading_zeros(),
                    0x68 => a.trailing_zeros(),
                    _ => a.count_ones(),
                };
                self.push(Val::I32(n as i32))
            }
            0x6A..=0x78 => {
                let b = self.pop_i32(base)?;
                let a = self.pop_i32(base)?;
                self.push(Val::I32(i32_binary(op, a, b)?))
            }
            0x79..=0x7B => {
                let a = self.pop_i64(base)? as u64;
                let n = match op {
                    0x79 => a.leading_zeros(),
                    0x7A => a.trailing_zeros(),
                    _ => a.count_ones(),
                };
                self.push(Val::I64(i64::from(n)))
            }
            0x7C..=0x8A => {
                let b = self.pop_i64(base)?;
                let a = self.pop_i64(base)?;
                // The i64 arithmetic block repeats the i32 one at an offset.
                self.push(Val::I64(i64_binary(op - 0x12, a, b)?))
            }
            0xA7 => {
                let a = self.pop_i64(base)?;
                self.push(Val::I32(a as i32))
            }
            0xAC => {
                let a = self.pop_i32(base)?;
                self.push(Val::I64(i64::from(a)))
            }
            0xAD => {
                let a = self.pop_i32(base)?;
                self.push(Val::I64(i64::from(a as u32)))
            }
            _ => Err(Trap::UnsupportedOpcode(op)),
        }
    }
}

/// Evaluates one of the ten relational operators, numbered from `eq` in
/// opcode order: eq, ne, lt_s, lt_u, gt_s, gt_u, le_s, le_u, ge_s, ge_u.
fn compare(code: u8, signed: Ordering, unsigned: Ordering) -> bool {
    match code {
        0 => signed == Ordering::Equal,
        1 => signed != Ordering::Equal,
        2 => signed == Ordering::Less,
        3 => unsigned == Ordering::Less,
        4 => signed == Ordering::Greater,
        5 => unsigned == Ordering::Greater,
        6 => signed != Ordering::Greater,
        7 => unsigned != Ordering::Greater,
        8 => signed != Ordering::Less,
        _ => unsigned != Ordering::Less,
    }
}

/// `op` is an i32 opcode in 0x6A..=0x78.
fn i32_binary(op: u8, a: i32, b: i32) -> Result<i32, Trap> {
    Ok(match op {
        0x6A => a.wrapping_add(b),
        0x6B => a.wrapping_sub(b),
        0x6C => a.wrapping_mul(b),
        0x6D => {
            if b == 0 {
                return Err(Trap::DivisionByZero);
            }
            if a == i32::MIN && b == -1 {
                return Err(Trap::IntegerOverflow);
            }
            a / b
        }
        0x6E => {
            if b == 0 {
                return Err(Trap::DivisionByZero);
            }
            ((a as u32) / (b as u32)) as i32
        }
        0x6F => {
            if b == 0 {
                return Err(Trap::DivisionByZero);
            }
            a.wrapping_rem(b)
        }
        0x70 => {
            if b == 0 {
                return Err(Trap::DivisionByZero);
            }
            ((a as u32) % (b as u32)) as i32
        }
        0x71 => a & b,
        0x72 => a | b,
        0x73 => a ^ b,
        // Shift and rotate counts are taken modulo the bit width.
        0x74 => a.wrapping_shl(b as u32),
        0x75 => a.wrapping_shr(b as u32),
        0x76 => (a as u32).wrapping_shr(b as u32) as i32,
        0x77 => (a as u32).rotate_left(b as u32 % 32) as i32,
        0x78 => (a as u32).rotate_right(b as u32 % 32) as i32,
        _ => return Err(Trap::UnsupportedOpcode(op)),
    })
}

/// `op` is the i32 opcode that has the same meaning as the i64 one.
fn i64_binary(op: u8, a: i64, b: i64) -> Result<i64, Trap> {
    Ok(match op {
        0x6A => a.wrapping_add(b),
        0x6B => a.wrapping_sub(b),
        0x6C => a.wrapping_mul(b),
        0x6D => {
            if b == 0 {
                return Err(Trap::DivisionByZero);
            }
            if a == i64::MIN && b == -1 {
                return Err(Trap::IntegerOverflow);
            }
            a / b
        }
        0x6E => {
            if b == 0 {
                return Err(Trap::DivisionByZero);
            }
            ((a as u64) / (b as u64)) as i64
        }
        0x6F => {
            if b == 0 {
                return Err(Trap::DivisionByZero);
            }
            a.wrapping_rem(b)
        }
        0x70 => {
            if b == 0 {
                return Err(Trap::DivisionByZero);
            }
            ((a as u64) % (b as u64)) as i64
        }
        0x71 => a & b,
        0x72 => a | b,
        0x73 => a ^ b,
        0x74 => a.wrapping_shl(b as u32),
        0x75 => a.wrapping_shr(b as u32),
        0x76 => (a as u64).wrapping_shr(b as u32) as i64,
        0x77 => (a as u64).rotate_left((b as u64 % 64) as u32) as i64,
        0x78 => (a as u64).rotate_right((b as u64 % 64) as u32) as i64,
        _ => return Err(Trap::UnsupportedOpcode(op)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{Export, ExternKind, FuncType};

    /// One exported function "f" with the given signature, extra locals and body.
    fn single(params: &[ValType], results: &[ValType], locals: &[ValType], body: &[u8]) -> Module {
        Module {
            types: vec![FuncType { params: params.to_vec(), results: results.to_vec() }],
            funcs: vec![Func { type_index: 0, locals: locals.to_vec(), body: body.to_vec() }],
            exports: vec![Export { name: "f".into(), kind: ExternKind::Func, index: 0 }],
            ..Module::default()
        }
    }

    fn run_f(module: &Module, args: &[Val]) -> Result<Vec<Val>, Trap> {
        Interpreter::new(module)
            .with_costs(CostTable::uniform(1))
            .call_export("f", args)
    }

    const I32: ValType = ValType::I32;
    const I64: ValType = ValType::I64;

    #[test]
    fn squares_and_counts_fuel_exactly() {
        // local.get 0, local.get 0, i32.mul, end
        let module = single(&[I32], &[I32], &[], &[0x20, 0x00, 0x20, 0x00, 0x6C, 0x0B]);
        let mut interp = Interpreter::new(&module)
            .with_costs(CostTable::uniform(1))
            .with_fuel(1_000);
        assert_eq!(interp.call_export("f", &[Val::I32(9)]), Ok(vec![Val::I32(81)]));
        assert_eq!(interp.fuel_consumed(), 4);

        interp.set_fuel(3);
        assert_eq!(interp.call_export("f", &[Val::I32(9)]), Err(Trap::OutOfFuel));
        assert_eq!(interp.fuel_consumed(), 3);
        assert_eq!(interp.fuel_remaining(), 0);
    }

    #[test]
    fn an_infinite_loop_runs_out_of_fuel() {
        // loop; br 0; end; end
        let module = single(&[], &[], &[], &[0x03, 0x40, 0x0C, 0x00, 0x0B, 0x0B]);
        let mut interp = Interpreter::new(&module)
            .with_costs(CostTable::uniform(1))
            .with_fuel(5_000);
        assert_eq!(interp.call_export("f", &[]), Err(Trap::OutOfFuel));
        assert_eq!(interp.fuel_consumed(), 5_000);
    }

    #[test]
    fn sums_with_a_block_and_a_loop() {
        let body = [
            0x02, 0x40, // block
            0x03, 0x40, //   loop
            0x20, 0x00, //     local.get 0
            0x45, //           i32.eqz
            0x0D, 0x01, //     br_if 1 (out of the block)
            0x20, 0x01, //     local.get 1
            0x20, 0x00, //     local.get 0
            0x6A, //           i32.add
            0x21, 0x01, //     local.set 1
            0x20, 0x00, //     local.get 0
            0x41, 0x01, //     i32.const 1
            0x6B, //           i32.sub
            0x21, 0x00, //     local.set 0
            0x0C, 0x00, //     br 0 (next iteration)
            0x0B, //         end
            0x0B, //       end
            0x20, 0x01, // local.get 1
            0x0B, //       end
        ];
        let module = single(&[I32], &[I32], &[I32], &body);
        assert_eq!(run_f(&module, &[Val::I32(100)]), Ok(vec![Val::I32(5050)]));
        assert_eq!(run_f(&module, &[Val::I32(0)]), Ok(vec![Val::I32(0)]));
    }

    #[test]
    fn if_else_picks_an_arm() {
        let body = [
            0x20, 0x00, // local.get 0
            0x04, 0x7F, // if (result i32)
            0x41, 0x0A, //   i32.const 10
            0x05, //       else
            0x41, 0x14, //   i32.const 20
            0x0B, //       end
            0x0B, //       end
        ];
        let module = single(&[I32], &[I32], &[], &body);
        assert_eq!(run_f(&module, &[Val::I32(1)]), Ok(vec![Val::I32(10)]));
        assert_eq!(run_f(&module, &[Val::I32(0)]), Ok(vec![Val::I32(20)]));
    }

    #[test]
    fn if_without_else_skips_when_false() {
        let body = [
            0x20, 0x00, // local.get 0
            0x04, 0x40, // if
            0x41, 0x07, //   i32.const 7
            0x0F, //         return
            0x0B, //       end
            0x41, 0x09, // i32.const 9
            0x0B, //       end
        ];
        let module = single(&[I32], &[I32], &[], &body);
        assert_eq!(run_f(&module, &[Val::I32(1)]), Ok(vec![Val::I32(7)]));
        assert_eq!(run_f(&module, &[Val::I32(0)]), Ok(vec![Val::I32(9)]));
    }

    #[test]
    fn branching_out_of_a_block_carries_its_result() {
        let body = [
            0x02, 0x7F, // block (result i32)
            0x41, 0x05, //   i32.const 5
            0x0C, 0x00, //   br 0
            0x0B, //       end
            0x0B, //       end
        ];
        let module = single(&[], &[I32], &[], &body);
        assert_eq!(run_f(&module, &[]), Ok(vec![Val::I32(5)]));
    }

    #[test]
    fn division_traps() {
        let div_zero = single(&[], &[I32], &[], &[0x41, 0x01, 0x41, 0x00, 0x6D, 0x0B]);
        assert_eq!(run_f(&div_zero, &[]), Err(Trap::DivisionByZero));

        // i32.const i32::MIN, i32.const -1, then div_s or rem_s.
        let min_then_neg_one = |op: u8| {
            single(
                &[],
                &[I32],
                &[],
                &[0x41, 0x80, 0x80, 0x80, 0x80, 0x78, 0x41, 0x7F, op, 0x0B],
            )
        };
        assert_eq!(run_f(&min_then_neg_one(0x6D), &[]), Err(Trap::IntegerOverflow));
        assert_eq!(run_f(&min_then_neg_one(0x6F), &[]), Ok(vec![Val::I32(0)]));

        let i64_div_zero = single(&[], &[I64], &[], &[0x42, 0x01, 0x42, 0x00, 0x80, 0x0B]);
        assert_eq!(run_f(&i64_div_zero, &[]), Err(Trap::DivisionByZero));
    }

    #[test]
    fn shift_counts_wrap_at_the_width() {
        // 1 << 33 is 1 << 1 for i32.
        let module = single(&[], &[I32], &[], &[0x41, 0x01, 0x41, 0x21, 0x74, 0x0B]);
        assert_eq!(run_f(&module, &[]), Ok(vec![Val::I32(2)]));
        // 1 << 65 is 1 << 1 for i64.
        let module = single(&[], &[I64], &[], &[0x42, 0x01, 0x42, 0xC1, 0x00, 0x86, 0x0B]);
        assert_eq!(run_f(&module, &[]), Ok(vec![Val::I64(2)]));
    }

    #[test]
    fn unsigned_and_signed_comparisons_differ() {
        // -1 < 1 signed (lt_s), but 0xFFFFFFFF > 1 unsigned (lt_u).
        let cmp = |op: u8| single(&[], &[I32], &[], &[0x41, 0x7F, 0x41, 0x01, op, 0x0B]);
        assert_eq!(run_f(&cmp(0x48), &[]), Ok(vec![Val::I32(1)]));
        assert_eq!(run_f(&cmp(0x49), &[]), Ok(vec![Val::I32(0)]));
    }

    #[test]
    fn conversions_between_widths() {
        // i64.const -1, i32.wrap_i64
        let wrap = single(&[], &[I32], &[], &[0x42, 0x7F, 0xA7, 0x0B]);
        assert_eq!(run_f(&wrap, &[]), Ok(vec![Val::I32(-1)]));
        // i32.const -1, i64.extend_i32_u
        let zext = single(&[], &[I64], &[], &[0x41, 0x7F, 0xAD, 0x0B]);
        assert_eq!(run_f(&zext, &[]), Ok(vec![Val::I64(0xFFFF_FFFF)]));
        let sext = single(&[], &[I64], &[], &[0x41, 0x7F, 0xAC, 0x0B]);
        assert_eq!(run_f(&sext, &[]), Ok(vec![Val::I64(-1)]));
    }

    /// f0 = quadruple via two calls to f1; f1 = double.
    fn calling_module() -> Module {
        let ty = FuncType { params: vec![I32], results: vec![I32] };
        Module {
            types: vec![ty],
            funcs: vec![
                Func {
                    type_index: 0,
                    locals: vec![],
                    body: vec![0x20, 0x00, 0x10, 0x01, 0x10, 0x01, 0x0B],
                },
                Func {
                    type_index: 0,
                    locals: vec![],
                    body: vec![0x20, 0x00, 0x20, 0x00, 0x6A, 0x0B],
                },
            ],
            exports: vec![Export { name: "f".into(), kind: ExternKind::Func, index: 0 }],
            ..Module::default()
        }
    }

    #[test]
    fn calls_nest_and_are_metered() {
        let module = calling_module();
        let mut interp = Interpreter::new(&module).with_costs(CostTable::uniform(1));
        assert_eq!(interp.call_export("f", &[Val::I32(5)]), Ok(vec![Val::I32(20)]));
        // 4 in f0, 4 in each of the two f1 calls.
        assert_eq!(interp.fuel_consumed(), 12);
    }

    #[test]
    fn opcode_overrides_beat_the_class_price() {
        let module = calling_module();
        let costs = CostTable::uniform(1).with_opcode_cost(0x10, 100);
        let mut interp = Interpreter::new(&module).with_costs(costs);
        interp.call_export("f", &[Val::I32(1)]).unwrap();
        assert_eq!(interp.fuel_consumed(), 12 - 2 + 200);
    }

    #[test]
    fn default_costs_price_division_and_calls_higher() {
        let costs = CostTable::default();
        assert_eq!(costs.cost(0x6A), 1);
        assert_eq!(costs.cost(0x6D), 8);
        assert_eq!(costs.cost(0x0C), 2);
        assert_eq!(costs.cost(0x10), 10);
    }

    #[test]
    fn runaway_recursion_hits_the_depth_limit() {
        // call 0; end
        let module = single(&[], &[], &[], &[0x10, 0x00, 0x0B]);
        let mut interp = Interpreter::new(&module).with_max_call_depth(32);
        assert_eq!(interp.call_export("f", &[]), Err(Trap::CallDepthExceeded));
    }

    #[test]
    fn the_value_stack_is_bounded() {
        let module = single(&[], &[], &[], &[0x41, 0x01, 0x41, 0x02, 0x41, 0x03, 0x0B]);
        let mut interp = Interpreter::new(&module).with_max_stack(2);
        assert_eq!(interp.call_export("f", &[]), Err(Trap::StackOverflow));
    }

    #[test]
    fn underflow_and_type_errors_are_traps() {
        let underflow = single(&[], &[I32], &[], &[0x6A, 0x0B]);
        assert_eq!(run_f(&underflow, &[]), Err(Trap::StackUnderflow));

        let mismatch = single(&[], &[I32], &[], &[0x41, 0x01, 0x42, 0x01, 0x6A, 0x0B]);
        assert_eq!(run_f(&mismatch, &[]), Err(Trap::TypeMismatch));

        let unreachable = single(&[], &[], &[], &[0x00, 0x0B]);
        assert_eq!(run_f(&unreachable, &[]), Err(Trap::Unreachable));
    }

    #[test]
    fn bad_labels_and_locals_are_traps() {
        let label = single(&[], &[], &[], &[0x0C, 0x05, 0x0B]);
        assert_eq!(run_f(&label, &[]), Err(Trap::InvalidLabel));

        let local = single(&[], &[I32], &[], &[0x20, 0x03, 0x0B]);
        assert_eq!(run_f(&local, &[]), Err(Trap::InvalidLocal));
    }

    #[test]
    fn unsupported_things_are_reported_not_run() {
        // i32.load 2 0
        let load = single(&[], &[I32], &[], &[0x41, 0x00, 0x28, 0x02, 0x00, 0x0B]);
        assert_eq!(run_f(&load, &[]), Err(Trap::UnsupportedOpcode(0x28)));

        // block with a type-index block type
        let typed = single(&[], &[], &[], &[0x02, 0x00, 0x0B, 0x0B]);
        assert_eq!(run_f(&typed, &[]), Err(Trap::UnsupportedBlockType));

        let float = single(&[ValType::F32], &[], &[], &[0x0B]);
        assert_eq!(run_f(&float, &[Val::I32(0)]), Err(Trap::UnsupportedValueType));
    }

    #[test]
    fn malformed_bodies_are_rejected_before_running() {
        // `end` that is not the last byte.
        let early_end = single(&[], &[], &[], &[0x0B, 0x01]);
        assert_eq!(run_f(&early_end, &[]), Err(Trap::MalformedBody));

        // block never closed
        let unclosed = single(&[], &[], &[], &[0x02, 0x40, 0x0B]);
        assert_eq!(run_f(&unclosed, &[]), Err(Trap::MalformedBody));

        // else with no enclosing if
        let stray_else = single(&[], &[], &[], &[0x05, 0x0B]);
        assert_eq!(run_f(&stray_else, &[]), Err(Trap::MalformedBody));

        // truncated i32.const immediate
        let truncated = single(&[], &[], &[], &[0x41, 0x80]);
        assert_eq!(run_f(&truncated, &[]), Err(Trap::MalformedBody));
    }

    #[test]
    fn call_checks_its_arguments() {
        let module = single(&[I32], &[I32], &[], &[0x20, 0x00, 0x0B]);
        assert_eq!(run_f(&module, &[]), Err(Trap::ArgumentMismatch));
        assert_eq!(run_f(&module, &[Val::I64(1)]), Err(Trap::ArgumentMismatch));
        let mut interp = Interpreter::new(&module);
        assert_eq!(interp.call_export("missing", &[]), Err(Trap::ExportNotFound));
        assert_eq!(interp.call(9, &[]), Err(Trap::UndefinedFunction));
    }

    #[test]
    fn imported_functions_cannot_be_called() {
        use crate::parser::Import;
        let mut module = single(&[], &[], &[], &[0x0B]);
        module.imports.push(Import {
            module: "env".into(),
            name: "log".into(),
            kind: ExternKind::Func,
            type_index: Some(0),
        });
        // Function index 0 is now the import.
        let mut interp = Interpreter::new(&module);
        assert_eq!(interp.call(0, &[]), Err(Trap::CalledImport));
        assert_eq!(interp.call(1, &[]), Ok(vec![]));
    }

    #[test]
    fn select_and_drop() {
        // 10, 20, cond=0 -> 20; then drop of an extra value.
        let module = single(
            &[],
            &[I32],
            &[],
            &[0x41, 0x63, 0x1A, 0x41, 0x0A, 0x41, 0x14, 0x41, 0x00, 0x1B, 0x0B],
        );
        assert_eq!(run_f(&module, &[]), Ok(vec![Val::I32(20)]));
    }

    #[test]
    fn local_tee_keeps_the_value() {
        // (local.tee 1 (i32.const 3)) + local.get 1
        let body = [0x41, 0x03, 0x22, 0x01, 0x20, 0x01, 0x6A, 0x0B];
        let module = single(&[], &[I32], &[I32], &body);
        assert_eq!(run_f(&module, &[]), Ok(vec![Val::I32(6)]));
    }
}
