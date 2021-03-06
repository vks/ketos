//! Compiles expressions into bytecode objects.

use std::borrow::Cow::{self, Borrowed, Owned};
use std::f64;
use std::fmt;
use std::mem::replace;
use std::rc::Rc;

use bytecode::{code_flags, Code, CodeBlock,
    Instruction, JumpInstruction, MAX_SHORT_OPERAND};
use error::Error;
use exec::execute_lambda;
use function::{Arity, Lambda};
use function::Arity::*;
use name::{get_system_fn, is_system_operator, standard_names,
    Name, NameDisplay, NameMap, NameSet, NameStore,
    NUM_SYSTEM_OPERATORS, SYSTEM_OPERATORS_BEGIN};
use scope::{GlobalScope, MasterScope, Scope};
use value::{StructDef, Value};

const MAX_MACRO_RECURSION: u32 = 100;

/// Represents an error generated while compiling to bytecode.
#[derive(Debug)]
pub enum CompileError {
    /// Error in arity for call to system function
    ArityError{
        /// Name of function
        name: Name,
        /// Expected count or range of arguments
        expected: Arity,
        /// Number of arguments present
        found: u32,
    },
    /// Attempt to define name of standard value or operator
    CannotDefine(Name),
    /// Duplicate `exports` declaration
    DuplicateExports,
    /// Duplicate name in parameter list
    DuplicateParameter(Name),
    /// Attempt to export nonexistent name from module
    ExportError{
        /// Module name
        module: Name,
        /// Imported name
        name: Name,
    },
    /// Recursion in module imports
    ImportCycle(Name),
    /// Attempt to import nonexistent name from module
    ImportError{
        /// Module name
        module: Name,
        /// Imported name
        name: Name,
    },
    /// Attempt to import name which already exists
    ImportShadow{
        /// Module name
        module: Name,
        /// Imported name
        name: Name,
    },
    /// Invalid expression to function call
    InvalidCallExpression(&'static str),
    /// `,@expr` form outside of a list
    InvalidCommaAt,
    /// Module name contains invalid characters
    InvalidModuleName(Name),
    /// Recursion limit exceeded while expanding macros
    MacroRecursionExceeded,
    /// Missing `export` declaration in loaded module
    MissingExport,
    /// Failed to load a module
    ModuleError(Name),
    /// Operand value overflow
    OperandOverflow(u32),
    /// Attempt to import value that is not exported
    PrivacyError{
        /// Module name
        module: Name,
        /// Imported name
        name: Name,
    },
    /// Error in parsing operator syntax
    SyntaxError(&'static str),
    /// More commas than backquotes
    UnbalancedComma,
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::CompileError::*;

        match *self {
            ArityError{expected, found, ..} =>
                write!(f, "expected {}; found {}", expected, found),
            CannotDefine(_) =>
                f.write_str("cannot define name of standard value or operator"),
            DuplicateExports => f.write_str("duplicate `exports` declaration"),
            DuplicateParameter(_) => f.write_str("duplicate parameter"),
            ExportError{..} => f.write_str("export name not found in module"),
            ImportCycle(_) => f.write_str("import cycle detected"),
            ImportError{..} => f.write_str("import name not found in module"),
            ImportShadow{..} => f.write_str("import shadows an existing name"),
            InvalidCallExpression(ty) =>
                write!(f, "invalid call expression of type `{}`", ty),
            InvalidCommaAt =>
                f.write_str("`,@expr` form is invalid outside of a list"),
            InvalidModuleName(_) => f.write_str("invalid module name"),
            MacroRecursionExceeded => f.write_str("macro recursion exceeded"),
            MissingExport => f.write_str("missing `export` declaration"),
            ModuleError(_) => f.write_str("module not found"),
            OperandOverflow(n) =>
                write!(f, "operand overflow: {}", n),
            PrivacyError{..} => f.write_str("name is private"),
            SyntaxError(e) => f.write_str(e),
            UnbalancedComma => f.write_str("unbalanced ` and ,"),
        }
    }
}

impl NameDisplay for CompileError {
    fn fmt(&self, names: &NameStore, f: &mut fmt::Formatter) -> fmt::Result {
        use self::CompileError::*;

        match *self {
            ArityError{name, ..} => write!(f, "`{}` {}", names.get(name), self),
            CannotDefine(name) |
            DuplicateParameter(name) |
            InvalidModuleName(name) |
            ModuleError(name) => write!(f, "{}: {}", self, names.get(name)),
            ExportError{module, name} =>
                write!(f, "cannot export name `{}`; not found in module `{}`",
                    names.get(name), names.get(module)),
            ImportCycle(name) =>
                write!(f, "import cycle in loading module `{}`", names.get(name)),
            ImportError{module, name} =>
                write!(f, "cannot import name `{}`; not found in module `{}`",
                    names.get(name), names.get(module)),
            ImportShadow{module, name} =>
                write!(f, "importing `{}` from `{}` shadows an existing value",
                    names.get(name), names.get(module)),
            PrivacyError{module, name} =>
                write!(f, "name `{}` in module `{}` is private",
                    names.get(name), names.get(module)),
            _ => fmt::Display::fmt(self, f)
        }
    }
}

/// Compiles an expression into a code object.
pub fn compile(scope: &Scope, value: &Value) -> Result<Code, Error> {
    Compiler::new(scope).compile(value)
}

fn compile_lambda(compiler: &Compiler,
        name: Option<Name>,
        params: Vec<(Name, Option<Value>)>,
        req_params: u32,
        kw_params: Vec<(Name, Option<Value>)>,
        rest: Option<Name>, value: &Value)
        -> Result<(Code, Vec<Name>), Error> {
    let outer = compiler.outer.iter().cloned()
        .chain(Some(compiler)).collect::<Vec<_>>();

    Compiler::with_outer(&compiler.scope, name, &outer)
        .compile_lambda(name, params, req_params, kw_params, rest, value)
}

/// Compiles a single expression or function body
struct Compiler<'a> {
    /// Compile scope
    scope: &'a Scope,
    /// Const values referenced from bytecode
    consts: Vec<Value>,
    /// Blocks of bytecode
    blocks: Vec<CodeBlock>,
    /// Current bytecode block
    cur_block: usize,
    /// Named stack values, paired with stack offset
    stack: Vec<(Name, u32)>,
    /// Current offset in stack; tracks addition and subtraction of named and
    /// unnamed stack values.
    stack_offset: u32,
    /// Set of names from outer scope captured by lambda
    captures: Vec<Name>,
    /// Names in outer scopes available to lambda
    outer: &'a [&'a Compiler<'a>],
    /// Name of lambda being compiled; used to detect tail calls
    self_name: Option<Name>,
    /// Depth of macro expansion
    macro_recursion: u32,
}

impl<'a> Compiler<'a> {
    fn new(scope: &'a Scope) -> Compiler<'a> {
        Compiler::with_outer(scope, None, &[])
    }

    fn with_outer(scope: &'a Scope, name: Option<Name>,
            outer: &'a [&'a Compiler<'a>]) -> Compiler<'a> {
        Compiler{
            scope: scope,
            consts: Vec::new(),
            blocks: vec![CodeBlock::new()],
            cur_block: 0,
            stack: Vec::new(),
            stack_offset: 0,
            captures: Vec::new(),
            outer: outer,
            self_name: name,
            macro_recursion: 0,
        }
    }

    fn assemble_code(&mut self) -> Result<Box<[u8]>, CompileError> {
        let total = try!(self.write_jumps());
        let mut res = Vec::with_capacity(total);

        for block in &mut self.blocks {
            res.extend(block.get_bytes());
        }

        assert_eq!(res.len(), total);
        Ok(res.into_boxed_slice())
    }

    /// Writes jump instructions with real offsets to each code blocks.
    /// Returns the total size, in bytes, of code blocks.
    fn write_jumps(&mut self) -> Result<usize, CompileError> {
        // If all possible offsets can be shortened, shorten them.
        let short = estimate_size(&self.blocks) <= MAX_SHORT_OPERAND as usize;

        let n_blocks = self.blocks.len();

        let mut new_blocks = Vec::with_capacity(n_blocks);
        let mut offsets = vec![!0; n_blocks];
        // Blocks which are a target of a conditional jump must be written out
        // even if they contain only a Return instruction.
        let mut must_live = vec![false; n_blocks];
        let mut off = 0;
        let mut i = 0;

        loop {
            let mut b = replace(&mut self.blocks[i], CodeBlock::empty());
            let next = b.next;
            let mut skip_block = false;

            match b.jump {
                Some((JumpInstruction::Jump, _)) => (),
                Some((_, n)) => must_live[n as usize] = true,
                _ => ()
            }

            if block_returns(&b, &self.blocks) {
                // If the block is empty and no other blocks will conditionally
                // jump to it, then the block may be pruned altogether.
                // Any blocks which would *unconditionally* jump will
                // instead themselves return.
                skip_block = !must_live[i] && b.is_mostly_empty();

                if !skip_block {
                    // If the block returns, its jump could not possibly have
                    // pointed anywhere but an empty, returning block.
                    b.jump = None;
                    try!(b.push_instruction(Instruction::Return));
                }
            }

            if !skip_block {
                try!(b.flush());

                // Jump block numbers refer to initial ordering
                offsets[i] = off as u32;
                off += b.calculate_size(short);
                new_blocks.push(b);
            }

            match next {
                Some(n) => i = n as usize,
                None => break
            }
        }

        replace(&mut self.blocks, new_blocks);

        for block in &mut self.blocks {
            if let Some((_, dest)) = block.jump {
                let dest_off = offsets[dest as usize];
                assert!(dest_off != !0, "jump to dead block {}", dest);
                try!(block.write_jump(dest_off, short));
            }
        }

        Ok(off)
    }

    fn compile(mut self, value: &Value) -> Result<Code, Error> {
        try!(self.compile_value(value));

        Ok(Code{
            name: None,
            code: try!(self.assemble_code()),
            consts: self.consts.into_boxed_slice(),
            kw_params: vec![].into_boxed_slice(),
            n_params: 0,
            req_params: 0,
            flags: 0,
        })
    }

    fn compile_lambda(mut self, name: Option<Name>,
            params: Vec<(Name, Option<Value>)>,
            req_params: u32,
            kw_params: Vec<(Name, Option<Value>)>,
            rest: Option<Name>, value: &Value)
            -> Result<(Code, Vec<Name>), Error> {
        let total_params = params.len() + kw_params.len() +
            if rest.is_some() { 1 } else { 0 };

        let n_params = params.len();

        // Insert dummy names to preserve stack locations.
        // Default values for parameters may reference only previously declared
        // parameters. Additionally, the code for default parameter values may
        // push variables onto the stack (immediately ahead of parameter values)
        // and absolute stack references to these additional values must
        // account for the space occupied by inaccessible parameter values.
        //
        // For example, when a function is called such as the following:
        //
        //     (define (foo a :optional (b (bar a)) c) ...)
        //
        // At the moment when `(bar a)` is called, the stack will look like this:
        //
        //     [param-a] [param-b] [param-c] [push-a]
        //
        // `param-` values are input parameters; `push-a` is the value of `a`
        // pushed onto the stack to be accepted by the call to `bar`.
        assert!(self.stack.is_empty());
        self.stack.extend((0..total_params as u32).map(|n| (Name::dummy(), n)));
        self.stack_offset = total_params as u32;

        let mut flags = 0;

        if name.is_some() {
            flags |= code_flags::HAS_NAME;
        }

        assert!(kw_params.is_empty() || rest.is_none(),
            "keyword parameters and rest parameters are mutually exclusive");

        if !kw_params.is_empty() {
            flags |= code_flags::HAS_KW_PARAMS;
        } else if rest.is_some() {
            flags |= code_flags::HAS_REST_PARAMS;
        }

        let mut kw_names = Vec::with_capacity(kw_params.len());

        for (i, (name, default)) in params.into_iter().enumerate() {
            if (i as u32) >= req_params {
                if let Some(default) = default {
                    try!(self.branch_if_unbound(i as u32, &default));
                } else {
                    try!(self.push_instruction(
                        Instruction::UnboundToUnit(i as u32)));
                }
            }

            self.stack[i].0 = name;
        }

        for (i, (name, default)) in kw_params.into_iter().enumerate() {
            if let Some(default) = default {
                try!(self.branch_if_unbound((n_params + i) as u32, &default));
            } else {
                try!(self.push_instruction(
                    Instruction::UnboundToUnit((n_params + i) as u32)));
            }

            self.stack[n_params + i].0 = name;
            kw_names.push(name);
        }

        if let Some(rest) = rest {
            let n = self.stack.len();
            self.stack[n - 1].0 = rest;
        }

        try!(self.compile_value(value));

        let code = Code{
            name: name,
            code: try!(self.assemble_code()),
            consts: self.consts.into_boxed_slice(),
            kw_params: kw_names.into_boxed_slice(),
            n_params: n_params as u32,
            req_params: req_params,
            flags: flags,
        };

        Ok((code, self.captures))
    }

    fn compile_value(&mut self, value: &Value) -> Result<(), Error> {
        match *value {
            Value::Name(name) => {
                let loaded = try!(self.load_local_name(name));

                if !loaded {
                    let c = self.add_const(Owned(Value::Name(name)));
                    try!(self.push_instruction(Instruction::GetDef(c)));
                }
            }
            Value::List(ref li) => {
                let fn_v = &li[0];

                let mut pushed_fn = false;

                match *fn_v {
                    Value::Name(name) => {
                        if try!(self.load_local_name(name)) {
                            try!(self.push_instruction(Instruction::Push));
                            pushed_fn = true;
                        } else if self.self_name == Some(name) {
                            () // This is handled later
                        } else if self.is_macro(name) {
                            self.macro_recursion += 1;
                            let v = try!(self.expand_macro(name, &li[1..]));
                            try!(self.compile_value(&v));
                            self.macro_recursion -= 1;

                            return Ok(());
                        } else if is_system_operator(name) {
                            return self.compile_operator(name, &li[1..]);
                        } else if try!(self.inline_call(name, &li[1..])) {
                            return Ok(());
                        }
                    }
                    Value::List(_) => {
                        try!(self.compile_value(fn_v));
                        try!(self.push_instruction(Instruction::Push));
                        pushed_fn = true;
                    }
                    ref v => return Err(From::from(
                        CompileError::InvalidCallExpression(v.type_name())))
                }

                for v in &li[1..] {
                    try!(self.compile_value(v));
                    try!(self.push_instruction(Instruction::Push));
                }

                let n_args = (li.len() - 1) as u32;

                if pushed_fn {
                    try!(self.push_instruction(Instruction::Call(n_args)));
                } else {
                    if let Value::Name(name) = *fn_v {
                        if self.self_name == Some(name) {
                            try!(self.push_instruction(
                                Instruction::CallSelf(n_args)));
                        } else {
                            match get_system_fn(name) {
                                Some(sys_fn) => {
                                    if !sys_fn.arity.accepts(n_args) {
                                        return Err(From::from(CompileError::ArityError{
                                            name: name,
                                            expected: sys_fn.arity,
                                            found: n_args,
                                        }));
                                    }

                                    try!(self.write_call_sys(name, sys_fn.arity, n_args));
                                }
                                None => {
                                    let c = self.add_const(Owned(Value::Name(name)));
                                    try!(self.push_instruction(
                                        Instruction::CallConst(c, n_args)));
                                }
                            }
                        }
                    }
                }
            }
            Value::Comma(_, _) | Value::CommaAt(_, _) =>
                return Err(From::from(CompileError::UnbalancedComma)),
            ref v @ Value::Quasiquote(_, _) =>
                // We pass the whole value at depth 0 to handle
                // multiply-quasiquoted values properly
                try!(self.compile_quasiquote(v, 0)),
            _ => try!(self.load_const_value(value))
        }

        Ok(())
    }

    fn is_macro(&self, name: Name) -> bool {
        self.scope.contains_macro(name)
    }

    fn expand_macro(&self, name: Name, args: &[Value]) -> Result<Value, Error> {
        if self.macro_recursion >= MAX_MACRO_RECURSION {
            return Err(From::from(CompileError::MacroRecursionExceeded));
        }

        let lambda = self.scope.get_macro(name)
            .expect("macro not found in expand_macro");

        execute_lambda(lambda, args.to_vec())
    }

    fn compile_operator(&mut self, name: Name, args: &[Value]) -> Result<(), Error> {
        let op = get_system_operator(name);
        let n_args = args.len() as u32;

        if !op.arity.accepts(n_args) {
            Err(From::from(CompileError::ArityError{
                name: name,
                expected: op.arity,
                found: n_args,
            }))
        } else {
            (op.callback)(self, args)
        }
    }

    fn compile_quasiquote(&mut self, value: &Value, depth: u32) -> Result<(), Error> {
        match *value {
            Value::Comma(ref v, n) if n == depth =>
                self.compile_value(v),
            Value::Comma(_, n) if n > depth =>
                Err(From::from(CompileError::UnbalancedComma)),
            Value::Comma(ref v, n) => {
                try!(self.compile_quasiquote(v, depth - n));
                try!(self.push_instruction(Instruction::Comma(n)));
                Ok(())
            }
            Value::CommaAt(_, n) if n > depth =>
                Err(From::from(CompileError::UnbalancedComma)),
            Value::CommaAt(_, n) if n == depth =>
                Err(From::from(CompileError::InvalidCommaAt)),
            Value::List(ref li) =>
                self.compile_quasiquote_list(li, depth),
            Value::Quote(ref v, n) => {
                try!(self.compile_quasiquote(v, depth));
                try!(self.push_instruction(Instruction::Quote(n)));
                Ok(())
            }
            Value::Quasiquote(ref v, n) => {
                try!(self.compile_quasiquote(v, depth + n));
                if depth == 0 {
                    if n != 1 {
                        try!(self.push_instruction(Instruction::Quasiquote(n - 1)));
                    }
                } else {
                    try!(self.push_instruction(Instruction::Quasiquote(n)));
                }
                Ok(())
            }
            _ => {
                try!(self.load_quoted_value(Borrowed(value)));
                Ok(())
            }
        }
    }

    fn compile_quasiquote_list(&mut self, li: &[Value], depth: u32) -> Result<(), Error> {
        let mut n_items = 0;
        let mut n_lists = 0;

        for v in li.iter() {
            if n_items == 0 && n_lists == 1 {
                try!(self.push_instruction(Instruction::Push));
            }

            match *v {
                Value::CommaAt(ref v, n) if n == depth => {
                    if n_items != 0 {
                        try!(self.push_instruction(Instruction::List(n_items)));
                        try!(self.push_instruction(Instruction::Push));
                        n_lists += 1;
                        n_items = 0;
                    }
                    try!(self.compile_value(v));
                    if n_lists != 0 {
                        try!(self.push_instruction(Instruction::Push));
                    }
                    n_lists += 1;
                }
                Value::CommaAt(_, n) if n > depth =>
                    return Err(From::from(CompileError::UnbalancedComma)),
                Value::CommaAt(ref v, n) => {
                    n_items += 1;
                    try!(self.compile_quasiquote(v, depth - n));
                    try!(self.push_instruction(
                        Instruction::CommaAt(depth - n)));
                    try!(self.push_instruction(Instruction::Push));
                }
                _ => {
                    n_items += 1;
                    try!(self.compile_quasiquote(v, depth));
                    try!(self.push_instruction(Instruction::Push));
                }
            }
        }

        if n_items != 0 {
            try!(self.push_instruction(Instruction::List(n_items)));

            if n_lists != 0 {
                try!(self.push_instruction(Instruction::Push));
                n_lists += 1;
            }
        }

        if n_lists > 1 {
            try!(self.push_instruction(Instruction::CallSysArgs(
                standard_names::CONCAT.get(), n_lists)));
        }

        Ok(())
    }

    fn inline_call(&mut self, name: Name, args: &[Value]) -> Result<bool, Error> {
        match name {
            standard_names::NULL if args.len() == 1 => {
                try!(self.compile_value(&args[0]));
                try!(self.push_instruction(Instruction::Null));
            }
            standard_names::EQ if args.len() == 2 => {
                let lhs = &args[0];
                let rhs = &args[1];

                if is_constant(rhs) {
                    try!(self.compile_value(lhs));
                    let c = self.add_const_value(rhs);
                    try!(self.push_instruction(Instruction::EqConst(c)));
                } else if is_constant(lhs) {
                    let c = self.add_const_value(lhs);
                    try!(self.compile_value(rhs));
                    try!(self.push_instruction(Instruction::EqConst(c)));
                } else {
                    try!(self.compile_value(lhs));
                    try!(self.push_instruction(Instruction::Push));
                    try!(self.compile_value(rhs));
                    try!(self.push_instruction(Instruction::Eq));
                }
            }
            standard_names::NOT_EQ if args.len() == 2 => {
                let lhs = &args[0];
                let rhs = &args[1];

                if is_constant(rhs) {
                    try!(self.compile_value(lhs));
                    let c = self.add_const_value(rhs);
                    try!(self.push_instruction(Instruction::NotEqConst(c)));
                } else if is_constant(lhs) {
                    let c = self.add_const_value(lhs);
                    try!(self.compile_value(rhs));
                    try!(self.push_instruction(Instruction::NotEqConst(c)));
                } else {
                    try!(self.compile_value(lhs));
                    try!(self.push_instruction(Instruction::Push));
                    try!(self.compile_value(rhs));
                    try!(self.push_instruction(Instruction::NotEq));
                }
            }
            standard_names::NOT if args.len() == 1 => {
                try!(self.compile_value(&args[0]));
                try!(self.push_instruction(Instruction::Not));
            }
            standard_names::INF if args.len() == 0 => {
                try!(self.load_const_value(&f64::INFINITY.into()));
            }
            standard_names::NAN if args.len() == 0 => {
                try!(self.load_const_value(&f64::NAN.into()));
            }
            standard_names::APPEND if args.len() == 2 => {
                try!(self.compile_value(&args[0]));
                try!(self.push_instruction(Instruction::Push));
                try!(self.compile_value(&args[1]));
                try!(self.push_instruction(Instruction::Append));
            }
            standard_names::FIRST if args.len() == 1 => {
                try!(self.compile_value(&args[0]));
                try!(self.push_instruction(Instruction::First));
            }
            standard_names::TAIL if args.len() == 1 => {
                try!(self.compile_value(&args[0]));
                try!(self.push_instruction(Instruction::Tail));
            }
            standard_names::INIT if args.len() == 1 => {
                try!(self.compile_value(&args[0]));
                try!(self.push_instruction(Instruction::Init));
            }
            standard_names::LAST if args.len() == 1 => {
                try!(self.compile_value(&args[0]));
                try!(self.push_instruction(Instruction::Last));
            }
            standard_names::LIST => {
                if args.is_empty() {
                    try!(self.push_instruction(Instruction::Unit));
                } else {
                    for arg in args {
                        try!(self.compile_value(arg));
                        try!(self.push_instruction(Instruction::Push));
                    }
                    try!(self.push_instruction(
                        Instruction::List(args.len() as u32)));
                }
            }
            standard_names::ID if args.len() == 1 => {
                try!(self.compile_value(&args[0]));
            }
            _ => return Ok(false)
        }

        Ok(true)
    }

    fn add_const(&mut self, value: Cow<Value>) -> u32 {
        match self.consts.iter().position(|v| v.is_identical(&value)) {
            Some(pos) => pos as u32,
            None => {
                let n = self.consts.len() as u32;
                self.consts.push(value.into_owned());
                n
            }
        }
    }

    fn add_const_value(&mut self, value: &Value) -> u32 {
        match *value {
            Value::Quote(ref v, 1) => self.add_const(Borrowed(v)),
            Value::Quote(ref v, n) =>
                self.add_const(Owned(Value::Quote(v.clone(), n - 1))),
            _ => self.add_const(Borrowed(value))
        }
    }

    fn branch_if_unbound(&mut self, pos: u32, value: &Value) -> Result<(), Error> {
        let bind_block = self.new_block();
        let final_block = self.new_block();

        self.current_block().jump_to(JumpInstruction::JumpIfBound(pos), final_block);

        self.use_next(bind_block);
        try!(self.compile_value(value));
        try!(self.push_instruction(Instruction::Store(pos)));
        self.use_next(final_block);
        Ok(())
    }

    fn load_lambda(&mut self, n: u32, captures: &[Name]) -> Result<(), CompileError> {
        if captures.is_empty() {
            self.push_instruction(Instruction::Const(n))
        } else {
            for &name in captures {
                let _loaded = try!(self.load_local_name(name));
                assert!(_loaded);
                try!(self.push_instruction(Instruction::Push));
            }

            self.push_instruction(
                Instruction::BuildClosure(n, captures.len() as u32))
        }
    }

    /// Emits code to load a local value from the stack or closure values.
    /// Returns `Ok(true)` if a named value was found and loaded.
    fn load_local_name(&mut self, name: Name) -> Result<bool, CompileError> {
        match self.stack.iter().rev().find(|&&(n, _)| n == name) {
            Some(&(_, pos)) => {
                try!(self.push_instruction(Instruction::Load(pos)));
                return Ok(true);
            }
            None => ()
        }

        // self name is more local than enclosed values
        if self.self_name == Some(name) {
            return Ok(false);
        }

        match self.closure_value(name) {
            Some(n) => {
                try!(self.push_instruction(Instruction::LoadC(n)));
                Ok(true)
            }
            None => Ok(false)
        }
    }

    /// Searches for a named value from enclosing scope.
    /// The name will be added to the set of captures if not already present.
    /// If the name is found, returns value index for use in `LoadC` instruction.
    fn closure_value(&mut self, name: Name) -> Option<u32> {
        match self.captures.iter().position(|&n| n == name) {
            Some(pos) => Some(pos as u32),
            None => {
                for o in self.outer {
                    if o.stack.iter().any(|&(n, _)| n == name) {
                        let n = self.captures.len() as u32;
                        self.captures.push(name);
                        return Some(n);
                    }
                }

                None
            }
        }
    }

    /// Generates instructions to load a constant value.
    fn load_const_value(&mut self, value: &Value) -> Result<(), CompileError> {
        match *value {
            Value::Unit => self.push_instruction(Instruction::Unit),
            Value::Bool(true) => self.push_instruction(Instruction::True),
            Value::Bool(false) => self.push_instruction(Instruction::False),
            Value::Quote(ref v, 1) => {
                self.load_quoted_value(Borrowed(v))
            }
            Value::Quote(ref v, n) => {
                let v = Value::Quote(v.clone(), n - 1);
                self.load_quoted_value(Owned(v))
            }
            ref v => {
                let c = self.add_const(Borrowed(v));
                self.push_instruction(Instruction::Const(c))
            }
        }
    }

    fn load_quoted_value(&mut self, value: Cow<Value>) -> Result<(), CompileError> {
        match *value {
            Value::Unit => self.push_instruction(Instruction::Unit),
            Value::Bool(true) => self.push_instruction(Instruction::True),
            Value::Bool(false) => self.push_instruction(Instruction::False),
            _ => {
                let c = self.add_const(value);
                self.push_instruction(Instruction::Const(c))
            }
        }
    }

    /// Adds a named value to the list of stack values.
    /// Should be followed by a `Push` instruction to adjust `stack_offset`.
    fn push_var(&mut self, name: Name) {
        self.stack.push((name, self.stack_offset));
    }

    /// Remove `n` named values from the list of stack values.
    /// Should be followed by a `Skip` instruction to adjust `stack_offset`.
    fn pop_vars(&mut self, n: u32) {
        let n = self.stack.len() - n as usize;
        let _ = self.stack.drain(n..);
    }

    fn write_call_sys(&mut self, name: Name, arity: Arity, n_args: u32) -> Result<(), CompileError> {
        match arity {
            Arity::Exact(n) => {
                // The only stack_offset adjustment that's done manually.
                self.stack_offset -= n;
                self.push_instruction(Instruction::CallSys(name.get()))
            }
            _ => self.push_instruction(
                Instruction::CallSysArgs(name.get(), n_args))
        }
    }

    fn current_block(&mut self) -> &mut CodeBlock {
        self.blocks.get_mut(self.cur_block).expect("invalid cur_block")
    }

    fn new_block(&mut self) -> u32 {
        let n = self.blocks.len() as u32;
        self.blocks.push(CodeBlock::new());
        n
    }

    fn use_block(&mut self, block: u32) {
        self.cur_block = block as usize;
    }

    fn use_next(&mut self, block: u32) {
        self.current_block().set_next(block);
        self.use_block(block);
    }

    fn flush_instructions(&mut self) -> Result<(), CompileError> {
        self.current_block().flush()
    }

    fn push_instruction(&mut self, instr: Instruction) -> Result<(), CompileError> {
        match instr {
            Instruction::Push => {
                self.stack_offset += 1;
            }
            Instruction::BuildClosure(_, n) |
            Instruction::List(n) |
            Instruction::Skip(n) => {
                self.stack_offset -= n;
            }
            // CallSys is handled at the push site
            // to avoid duplicate get_system_fn call
            Instruction::CallSysArgs(_, n) |
            Instruction::CallSelf(n) |
            Instruction::CallConst(_, n) => {
                self.stack_offset -= n;
            }
            Instruction::Call(n) |
            Instruction::Apply(n) => {
                self.stack_offset -= n + 1;
            }
            Instruction::Eq |
            Instruction::NotEq => {
                self.stack_offset -= 1;
            }
            _ => ()
        }

        self.current_block().push_instruction(instr)
    }
}

fn block_returns<'a>(mut b: &'a CodeBlock, blocks: &'a [CodeBlock]) -> bool {
    loop {
        match (b.jump, b.next) {
            (_, None) => return true,
            // This assumes that jumps cannot be cyclical.
            // Currently, the compiler does not emit cyclical jumps.
            (Some((JumpInstruction::Jump, n)), _) |
            (_, Some(n)) if blocks[n as usize].is_mostly_empty() => {
                b = &blocks[n as usize];
            }
            _ => return false
        }
    }
}

fn estimate_size(blocks: &[CodeBlock]) -> usize {
    blocks.iter().map(|b| b.calculate_size(false))
        .fold(0, |a, b| a + b) + 1 // Plus one for final Return
}

fn is_constant(v: &Value) -> bool {
    match *v {
        Value::Unit |
        Value::Bool(_) |
        Value::Float(_) |
        Value::Integer(_) |
        Value::Ratio(_) |
        Value::Keyword(_) |
        Value::Char(_) |
        Value::String(_) |
        Value::Quote(_, _) => true,
        _ => false
    }
}

struct Operator {
    arity: Arity,
    callback: OperatorCallback,
}

type OperatorCallback = fn(&mut Compiler, args: &[Value]) -> Result<(), Error>;

macro_rules! sys_op {
    ( $callback:ident, $arity:expr ) => {
        Operator{
            arity: $arity,
            callback: $callback,
        }
    }
}

fn get_system_operator(name: Name) -> &'static Operator {
    &SYSTEM_OPERATORS[(name.get() - SYSTEM_OPERATORS_BEGIN) as usize]
}

/// System operator implementations.
///
/// These must correspond exactly to names `SYSTEM_OPERATORS_BEGIN` to
/// `SYSTEM_OPERATORS_END` in `name.rs`.
static SYSTEM_OPERATORS: [Operator; NUM_SYSTEM_OPERATORS] = [
    sys_op!(op_apply, Min(2)),
    sys_op!(op_do, Min(1)),
    sys_op!(op_let, Exact(2)),
    sys_op!(op_define, Exact(2)),
    sys_op!(op_macro, Exact(2)),
    sys_op!(op_struct, Exact(2)),
    sys_op!(op_if, Range(2, 3)),
    sys_op!(op_and, Min(1)),
    sys_op!(op_or, Min(1)),
    sys_op!(op_case, Min(2)),
    sys_op!(op_cond, Min(1)),
    sys_op!(op_lambda, Exact(2)),
    sys_op!(op_export, Exact(1)),
    sys_op!(op_use, Min(2)),
];

/// `apply` calls a function or lambda with a series of arguments.
/// Arguments to `apply` other than the last argument are passed directly
/// to the function; the last argument to `apply` is a list whose elements
/// will be passed to the function.
///
/// ```lisp
/// ; Calls (foo 1 2 3 4 5)
/// (apply foo 1 2 '(3 4 5))
/// ```
fn op_apply(compiler: &mut Compiler, args: &[Value]) -> Result<(), Error> {
    let last = args.len() - 1;

    for arg in &args[..last] {
        try!(compiler.compile_value(arg));
        try!(compiler.push_instruction(Instruction::Push));
    }

    try!(compiler.compile_value(&args[last]));
    try!(compiler.push_instruction(Instruction::Apply(last as u32 - 1)));

    Ok(())
}

/// `do` evaluates a series of expressions, yielding the value of the last
/// expression.
fn op_do(compiler: &mut Compiler, args: &[Value]) -> Result<(), Error> {
    for arg in args {
        try!(compiler.compile_value(arg));
    }
    Ok(())
}

/// `let` defines a series of named value bindings.
///
/// ```lisp
/// (let ((a (foo))
///       (b (bar)))
///   (baz a b))
/// ```
fn op_let(compiler: &mut Compiler, args: &[Value]) -> Result<(), Error> {
    let mut n_vars = 0;

    match args[0] {
        Value::Unit => (),
        Value::List(ref li) => {
            n_vars = li.len() as u32;
            for v in li.iter() {
                match *v {
                    Value::List(ref li) if li.len() == 2 => {
                        let name = try!(get_name(&li[0]));

                        try!(compiler.compile_value(&li[1]));
                        compiler.push_var(name);
                        try!(compiler.push_instruction(Instruction::Push));
                    }
                    _ => return Err(From::from(CompileError::SyntaxError(
                        "expected list of 2 elements")))
                }
            }
        }
        _ => return Err(From::from(CompileError::SyntaxError("expected list")))
    }

    try!(compiler.compile_value(&args[1]));

    // Create a new block containing the Skip.
    // This helps to optimize out unnecessary instructions in the assembly phase.
    let next_block = compiler.new_block();
    compiler.use_next(next_block);

    try!(compiler.push_instruction(Instruction::Skip(n_vars)));
    compiler.pop_vars(n_vars);

    Ok(())
}

/// `define` declares a value binding or function binding in global scope.
///
/// ```lisp
/// (define foo 123)
///
/// (define (bar a) (+ a foo))
/// ```
fn op_define(compiler: &mut Compiler, args: &[Value]) -> Result<(), Error> {
    match args[0] {
        Value::Name(name) => {
            try!(test_define_name(name));
            try!(compiler.compile_value(&args[1]));
            let c = compiler.add_const(Owned(Value::Name(name)));
            try!(compiler.push_instruction(Instruction::SetDef(c)));
            Ok(())
        }
        Value::List(ref li) => {
            let name = try!(get_name(&li[0]));
            try!(test_define_name(name));
            let c = compiler.add_const(Owned(Value::Name(name)));

            let (lambda, captures) = try!(make_lambda(
                &compiler, Some(name), &li[1..], &args[1]));

            let code_c = compiler.add_const(Owned(Value::Lambda(lambda)));
            try!(compiler.load_lambda(code_c, &captures));
            try!(compiler.push_instruction(Instruction::SetDef(c)));
            Ok(())
        }
        _ => Err(From::from(CompileError::SyntaxError("expected name or list")))
    }
}

/// `macro` defines a compile-time macro function in global scope.
fn op_macro(compiler: &mut Compiler, args: &[Value]) -> Result<(), Error> {
    let (name, params) = match args[0] {
        Value::List(ref li) => {
            let name = try!(get_name(&li[0]));
            (name, &li[1..])
        }
        _ => return Err(From::from(CompileError::SyntaxError("expected list")))
    };

    try!(test_define_name(name));

    let (lambda, captures) = try!(make_lambda(compiler,
        Some(name), params, &args[1]));

    if !captures.is_empty() {
        return Err(From::from(CompileError::SyntaxError(
            "macro lambda cannot enclose values")));
    }

    compiler.scope.add_macro(name, lambda);

    let c = compiler.add_const(Owned(Value::Name(name)));
    try!(compiler.push_instruction(Instruction::Const(c)));
    Ok(())
}

/// `struct` creates a struct definition and binds to global scope.
///
/// ```lisp
/// (struct Foo ((name string)
///              (num integer)))
/// ```
fn op_struct(compiler: &mut Compiler, args: &[Value]) -> Result<(), Error> {
    let name = try!(get_name(&args[0]));
    try!(test_define_name(name));
    let mut fields = NameMap::new();

    match args[1] {
        Value::Unit => (),
        Value::List(ref li) => {
            for v in li.iter() {
                match *v {
                    Value::List(ref li) if li.len() == 2 => {
                        let fname = try!(get_name(&li[0]));
                        let fty = try!(get_name(&li[1]));

                        fields.insert(fname, fty);
                    }
                    _ => return Err(From::from(CompileError::SyntaxError(
                        "expected list of 2 elements")))
                }
            }
        }
        _ => return Err(From::from(CompileError::SyntaxError("expected list")))
    }

    let def = Value::StructDef(Rc::new(StructDef::new(name, fields.into_slice())));

    let name_c = compiler.add_const(Owned(Value::Name(name)));
    let c = compiler.add_const(Owned(def));
    try!(compiler.push_instruction(Instruction::Const(c)));
    try!(compiler.push_instruction(Instruction::SetDef(name_c)));
    Ok(())
}

/// `if` evaluates a boolean condition expression and chooses a branch based
/// on the result.
///
/// ```lisp
/// (if (foo)
///   (bar)
///   (baz))
/// ```
fn op_if(compiler: &mut Compiler, args: &[Value]) -> Result<(), Error> {
    let then_block = compiler.new_block();
    let else_block = compiler.new_block();
    let final_block = compiler.new_block();

    try!(compiler.compile_value(&args[0]));
    compiler.current_block().jump_to(JumpInstruction::JumpIfNot, else_block);

    compiler.use_next(then_block);
    try!(compiler.compile_value(&args[1]));
    compiler.current_block().jump_to(JumpInstruction::Jump, final_block);

    compiler.use_next(else_block);
    match args.get(2) {
        Some(value) => try!(compiler.compile_value(value)),
        None => try!(compiler.push_instruction(Instruction::Unit))
    }

    compiler.use_next(final_block);
    Ok(())
}

/// `and` evaluates a series of boolean expressions, yielding the logical AND
/// of all expressions. If a `false` value is evaluated, no further expressions
/// will be evaluated.
fn op_and(compiler: &mut Compiler, args: &[Value]) -> Result<(), Error> {
    let (last, init) = args.split_last().unwrap();
    let last_block = compiler.new_block();

    for arg in init {
        try!(compiler.compile_value(arg));

        // The `and` operator expects a boolean value in the value register
        // after this jump instruction is run. Therefore, we must prevent
        // the compiler from merging it with a previous instruction,
        // which might result in a different value, e.g. () for JumpIfNotNull.
        try!(compiler.flush_instructions());
        compiler.current_block().jump_to(JumpInstruction::JumpIfNot, last_block);

        let block = compiler.new_block();
        compiler.use_next(block);
    }

    try!(compiler.compile_value(last));
    compiler.use_next(last_block);
    Ok(())
}

/// `and` evaluates a series of boolean expressions, yielding the logical OR
/// of all expressions. If a `true` value is evaluated, no further expressions
/// will be evaluated.
fn op_or(compiler: &mut Compiler, args: &[Value]) -> Result<(), Error> {
    let (last, init) = args.split_last().unwrap();
    let last_block = compiler.new_block();

    for arg in init {
        try!(compiler.compile_value(arg));

        // The `or` operator expects a boolean value in the value register
        // after this jump instruction is run. Therefore, we must prevent
        // the compiler from merging it with a previous instruction,
        // which might result in a different value, e.g. () for JumpIfNull.
        try!(compiler.flush_instructions());
        compiler.current_block().jump_to(JumpInstruction::JumpIf, last_block);

        let block = compiler.new_block();
        compiler.use_next(block);
    }

    try!(compiler.compile_value(last));
    compiler.use_next(last_block);
    Ok(())
}

/// `case` evaluates an expression and selects a branch by comparing the value
/// to a series of constant expressions.
///
/// The last branch may use `else` as its pattern to match all values.
/// If there is not a successful match, the value `()` is yielded.
///
/// ```lisp
/// (case foo
///   ((0 2 4 6 8) 'even)
///   ((1 3 5 7 9) 'odd))
///
/// (case bar
///   ((0 1 2 3) 'a)
///   ((4 5 6 7) 'b)
///   (else      'c))
/// ```
fn op_case(compiler: &mut Compiler, args: &[Value]) -> Result<(), Error> {
    let final_block = compiler.new_block();
    let mut code_blocks = Vec::with_capacity(args.len());
    let mut else_case = false;

    try!(compiler.compile_value(&args[0]));

    for case in &args[1..] {
        if else_case {
            return Err(From::from(CompileError::SyntaxError("unreachable case")));
        }

        let li = match *case {
            Value::List(ref li) if li.len() == 2 => li,
            _ => return Err(From::from(CompileError::SyntaxError(
                "expected list of 2 elements")))
        };

        let pat = &li[0];
        let code = &li[1];

        let code_begin = compiler.new_block();

        match *pat {
            Value::List(ref li) => {
                for v in li.iter() {
                    match *v {
                        Value::Unit => compiler.current_block().jump_to(
                            JumpInstruction::JumpIfNull, code_begin),
                        Value::Bool(true) => compiler.current_block().jump_to(
                            JumpInstruction::JumpIf, code_begin),
                        Value::Bool(false) => compiler.current_block().jump_to(
                            JumpInstruction::JumpIfNot, code_begin),
                        ref v => {
                            let c = compiler.add_const(Borrowed(v));
                            compiler.current_block().jump_to(
                                JumpInstruction::JumpIfEqConst(c), code_begin);
                        }
                    }
                    let b = compiler.new_block();
                    compiler.use_next(b);
                }
            }
            Value::Name(standard_names::ELSE) => {
                else_case = true;
                compiler.current_block().jump_to(JumpInstruction::Jump, code_begin);
            }
            _ => return Err(From::from(CompileError::SyntaxError(
                "expected list or `else`")))
        }

        let prev_block = compiler.cur_block as u32;
        compiler.use_block(code_begin);
        try!(compiler.compile_value(code));
        compiler.current_block().jump_to(JumpInstruction::Jump, final_block);
        let code_end = compiler.cur_block as u32;
        code_blocks.push((code_begin, code_end));

        let b = compiler.new_block();
        compiler.use_block(prev_block);
        compiler.use_next(b);
    }

    if !else_case {
        try!(compiler.push_instruction(Instruction::Unit));
        compiler.current_block().jump_to(JumpInstruction::Jump, final_block);
    }

    for (begin, end) in code_blocks {
        compiler.current_block().set_next(begin);
        compiler.use_block(end);
    }

    compiler.use_next(final_block);
    Ok(())
}

/// `cond` evaluates a series of boolean expressions and chooses the branch
/// of the first expression evaluating to `true`.
///
/// ```lisp
/// (cond
///   ((<  a 50) 'low)
///   ((>= a 50) 'high))
///
/// (cond
///   ((< a 10)  'low)
///   ((< a 90)  'mid)
///   ((< a 100) 'high)
///   (else      'huge))
/// ```
fn op_cond(compiler: &mut Compiler, args: &[Value]) -> Result<(), Error> {
    let final_block = compiler.new_block();
    let mut code_blocks = Vec::with_capacity(args.len());
    let mut else_case = false;

    for arg in args {
        if else_case {
            return Err(From::from(CompileError::SyntaxError(
                "unreachable condition")));
        }

        let case = match *arg {
            Value::List(ref li) if li.len() == 2 => li,
            _ => return Err(From::from(CompileError::SyntaxError(
                "expected list of 2 elements")))
        };

        let cond = &case[0];
        let code = &case[1];

        let code_begin = compiler.new_block();

        if let Value::Name(standard_names::ELSE) = *cond {
            else_case = true;
            compiler.current_block().jump_to(JumpInstruction::Jump, code_begin);
        } else {
            try!(compiler.compile_value(cond));
            compiler.current_block().jump_to(JumpInstruction::JumpIf, code_begin);
        }

        let prev_block = compiler.cur_block as u32;
        compiler.use_block(code_begin);
        try!(compiler.compile_value(code));
        compiler.current_block().jump_to(JumpInstruction::Jump, final_block);
        let code_end = compiler.cur_block as u32;
        code_blocks.push((code_begin, code_end));

        let b = compiler.new_block();
        compiler.use_block(prev_block);
        compiler.use_next(b);
    }

    if !else_case {
        try!(compiler.push_instruction(Instruction::Unit));
        compiler.current_block().jump_to(JumpInstruction::Jump, final_block);
    }

    for (begin, end) in code_blocks {
        compiler.current_block().set_next(begin);
        compiler.use_block(end);
    }

    compiler.use_next(final_block);
    Ok(())
}

/// `lambda` defines an anonymous lambda function which may enclose named values
/// from the enclosing scope.
///
/// ```lisp
/// (define (plus-n n)
///   (lambda (v) (+ v n)))
/// ```
fn op_lambda(compiler: &mut Compiler, args: &[Value]) -> Result<(), Error> {
    let li = match args[0] {
        Value::Unit => &[][..],
        Value::List(ref li) => &li[..],
        _ => return Err(From::from(CompileError::SyntaxError("expected list")))
    };

    let (lambda, captures) = try!(make_lambda(
        &compiler, None, li, &args[1]));

    let c = compiler.add_const(Owned(Value::Lambda(lambda)));
    try!(compiler.load_lambda(c, &captures));
    Ok(())
}

/// `export` declares the set of names exported from a code module.
///
/// ```lisp
/// (export (foo bar baz))
/// ```
fn op_export(compiler: &mut Compiler, args: &[Value]) -> Result<(), Error> {
    if compiler.scope.with_exports(|e| e.is_some()) {
        return Err(From::from(CompileError::DuplicateExports));
    }

    let li = match args[0] {
        Value::Unit => &[][..],
        Value::List(ref li) => &li[..],
        _ => return Err(From::from(CompileError::SyntaxError(
            "expected list of names in `export`")))
    };

    let mut names = NameSet::new();

    for v in li {
        names.insert(try!(get_name(v)));
    }

    compiler.scope.set_exports(names.into_slice());

    try!(compiler.push_instruction(Instruction::Unit));
    Ok(())
}

/// `use` imports a series of names from a module.
///
/// ```lisp
/// (use foo (alpha beta gamma))
///
/// (use foo (alpha beta)
///          :macro (gamma))
/// ```
fn op_use(compiler: &mut Compiler, args: &[Value]) -> Result<(), Error> {
    let mod_name = try!(get_name(&args[0]));
    let mods = compiler.scope.get_modules();
    let m = try!(mods.get_module(mod_name, compiler.scope));

    match args[1] {
        Value::Keyword(standard_names::ALL) => {
            m.scope.import_all_values(compiler.scope);
        }
        Value::Unit => (),
        Value::List(ref li) => {
            try!(import_values(mod_name, compiler.scope, &m.scope, li));
        }
        _ => return Err(From::from(CompileError::SyntaxError(
            "expected list of names or `:all`")))
    }

    let mut iter = args[2..].iter();

    while let Some(arg) = iter.next() {
        match *arg {
            Value::Keyword(standard_names::MACRO) => {
                match iter.next() {
                    Some(&Value::Keyword(standard_names::ALL)) =>
                        m.scope.import_all_macros(compiler.scope),
                    Some(&Value::Unit) => (),
                    Some(&Value::List(ref li)) =>
                        try!(import_macros(mod_name, compiler.scope, &m.scope, li)),
                    _ => return Err(From::from(CompileError::SyntaxError(
                        "expected `:all` or list of names after keyword")))
                }
            }
            _ => return Err(From::from(CompileError::SyntaxError(
                "expected keyword `:macro`")))
        }
    }

    try!(compiler.push_instruction(Instruction::Unit));
    Ok(())
}

fn import_macros(mod_name: Name, a: &GlobalScope, b: &GlobalScope,
        names: &[Value]) -> Result<(), CompileError> {
    each_import(names, |src, dest| {
        match b.get_macro(src) {
            Some(v) => {
                if !b.is_exported(src) {
                    return Err(CompileError::PrivacyError{
                        module: mod_name,
                        name: src,
                    });
                }

                a.add_macro(dest, v);
            }
            None => return Err(CompileError::ImportError{
                module: mod_name,
                name: src,
            })
        }

        Ok(())
    })
}

fn import_values(mod_name: Name, a: &GlobalScope, b: &GlobalScope,
        names: &[Value]) -> Result<(), CompileError> {
    each_import(names, |src, dest| {
        match b.get_value(src) {
            Some(v) => {
                if !b.is_exported(src) {
                    return Err(CompileError::PrivacyError{
                        module: mod_name,
                        name: src,
                    });
                }

                a.add_value(dest, v);
            }
            None => return Err(CompileError::ImportError{
                module: mod_name,
                name: src,
            })
        }

        Ok(())
    })
}

fn each_import<F>(items: &[Value], mut f: F) -> Result<(), CompileError>
        where F: FnMut(Name, Name) -> Result<(), CompileError> {
    let mut iter = items.iter();

    while let Some(item) = iter.next() {
        let (src, dest) = match *item {
            Value::Keyword(dest) => match iter.next() {
                Some(&Value::Name(src)) => (src, dest),
                _ => return Err(CompileError::SyntaxError(
                    "expected name following keyword"))
            },
            Value::Name(name) => (name, name),
            _ => return Err(CompileError::SyntaxError(
                "expected name or keyword"))
        };

        try!(f(src, dest));
    }

    Ok(())
}

fn get_name(v: &Value) -> Result<Name, CompileError> {
    match *v {
        Value::Name(name) => Ok(name),
        _ => Err(CompileError::SyntaxError("expected name"))
    }
}

fn test_define_name(name: Name) -> Result<(), CompileError> {
    if MasterScope::can_define(name) {
        Ok(())
    } else {
        Err(CompileError::CannotDefine(name))
    }
}

/// Creates a `Lambda` object using scope and local values from the given compiler.
/// Returns the `Lambda` object and the set of names captured by the lambda.
fn make_lambda(compiler: &Compiler, name: Option<Name>,
        args: &[Value], body: &Value) -> Result<(Lambda, Vec<Name>), Error> {
    let mut params = Vec::new();
    let mut req_params = 0;
    let mut kw_params = Vec::new();
    // Whether we've encountered `:key`
    let mut key = false;
    // Whether we've encountered `:optional`
    let mut optional = false;
    // `:rest` argument, if encountered
    let mut rest = None;

    let mut iter = args.iter();

    while let Some(v) = iter.next() {
        let (name, default) = match *v {
            Value::Name(name) => (name, None),
            Value::Keyword(kw) => {
                match kw {
                    standard_names::KEY => {
                        if key {
                            return Err(From::from(CompileError::SyntaxError(
                                "duplicate `:key`")));
                        } else if optional {
                            return Err(From::from(CompileError::SyntaxError(
                                "`:key` and `:optional` are mutually exclusive")));
                        }
                        key = true;
                        req_params = params.len() as u32;
                    }
                    standard_names::OPTIONAL => {
                        if optional {
                            return Err(From::from(CompileError::SyntaxError(
                                "duplicate `:optional`")));
                        } else if key {
                            return Err(From::from(CompileError::SyntaxError(
                                "`:key` and `:optional` are mutually exclusive")));
                        }
                        optional = true;
                        req_params = params.len() as u32;
                    }
                    standard_names::REST => {
                        if key {
                            return Err(From::from(CompileError::SyntaxError(
                                "`:key` and `:rest` are mutually exclusive")));
                        }

                        let arg = match iter.next() {
                            Some(arg) => arg,
                            None => return Err(From::from(CompileError::SyntaxError(
                                "expected name after `:rest`")))
                        };

                        rest = Some(try!(get_name(arg)));

                        match iter.next() {
                            Some(_) => return Err(From::from(CompileError::SyntaxError(
                                "extraneous token after `:rest` argument"))),
                            None => break
                        }
                    }
                    _ => return Err(From::from(CompileError::SyntaxError(
                        "expected :key, :optional, or :rest")))
                }
                continue;
            }
            Value::List(ref li) if li.len() == 2 => {
                let name = try!(get_name(&li[0]));
                (name, Some(li[1].clone()))
            }
            _ => return Err(From::from(CompileError::SyntaxError(
                "expected name, keyword, or list of 2 elements")))
        };

        let exists = params.iter().any(|&(n, _)| n == name) ||
            kw_params.iter().any(|&(n, _)| n == name);

        if exists {
            return Err(From::from(CompileError::DuplicateParameter(name)));
        }

        if key {
            kw_params.push((name, default));
        } else {
            params.push((name, default));
        }
    }

    if !optional {
        req_params = params.len() as u32;
    }

    if key && kw_params.is_empty() {
        return Err(From::from(CompileError::SyntaxError(
            "expected arguments after `:key`")));
    }

    if optional && req_params == params.len() as u32 {
        return Err(From::from(CompileError::SyntaxError(
            "expected arguments after `:optional`")));
    }

    let (code, captures) = try!(compile_lambda(&compiler,
        name, params, req_params, kw_params, rest, body));

    Ok((Lambda::new(Rc::new(code), &compiler.scope), captures))
}
