use core::panic;
use std::{collections::HashMap, ops::Deref, rc::Rc};

use azula_ast::prelude::*;
use azula_ir::prelude::*;
use azula_type::prelude::AzulaType;

fn struct_byte_size<'a>(module: &Module<'a>, typ: &AzulaType<'a>) -> usize {
    match typ {
        AzulaType::Named(name) => {
            if let Some(s) = module.structs.get(name.as_str()) {
                s.attributes
                    .iter()
                    .map(|(t, _)| struct_byte_size(module, t))
                    .sum()
            } else {
                8
            }
        }
        _ => 8,
    }
}

/// Copy every argument into a stack slot so that arguments behave exactly like
/// local variables (they can be addressed with `&` and have fields assigned).
fn spill_arguments<'a>(function: &mut Function<'a>) {
    for (index, (name, typ)) in function.arguments.clone().into_iter().enumerate() {
        let value = function.load_arg(index, typ.clone());
        function.store(name.clone(), value, typ.clone());
        function.variables.insert(name, typ);
    }
}

/// The type of the elements produced by indexing a value of type `typ`.
fn element_type<'a>(typ: &AzulaType<'a>) -> AzulaType<'a> {
    match typ {
        AzulaType::Array(inner, _) => inner.as_ref().clone(),
        AzulaType::Str => AzulaType::SizedSignedInt(8),
        AzulaType::Pointer(inner) => inner.as_ref().clone(),
        _ => unreachable!("array access on non-array type {:?}", typ),
    }
}

fn current_block_terminated(func: &Function<'_>) -> bool {
    let last = func
        .blocks
        .iter()
        .find(|(name, _)| *name == func.current_block)
        .and_then(|(_, block)| block.instructions.last());
    is_terminator(last)
}

fn is_terminator(instr: Option<&Instruction<'_>>) -> bool {
    matches!(
        instr,
        Some(Instruction::Return(_))
            | Some(Instruction::Jump(_))
            | Some(Instruction::Jcond(..))
            | Some(Instruction::Unreachable)
    )
}

pub struct Codegen<'a> {
    root: Statement<'a>,

    pub module: Module<'a>,
    pub function_calls: HashMap<String, Vec<AzulaType<'a>>>,
    loop_end_stack: Vec<String>,
    loop_continue_stack: Vec<String>,
    /// Lexical scopes mapping source variable names to their unique IR names
    scopes: Vec<HashMap<String, String>>,
    var_counter: usize,
    /// Argument types of every function and method, by (mangled) name
    signatures: HashMap<String, Vec<AzulaType<'a>>>,
}

impl<'a> Codegen<'a> {
    pub fn new(name: &'a str, root: Statement<'a>) -> Self {
        Self {
            root,
            module: Module::new(name),
            function_calls: HashMap::new(),
            loop_end_stack: vec![],
            loop_continue_stack: vec![],
            scopes: vec![HashMap::new()],
            var_counter: 0,
            signatures: HashMap::new(),
        }
    }

    pub fn codegen(&mut self) {
        let stmts = if let Statement::Root(stmts) = &self.root {
            stmts.clone()
        } else {
            return;
        };
        // Pass 1: register types, externs, globals and function signatures (so
        // everything is known before any function body is processed)
        for stmt in stmts.clone() {
            match stmt {
                Statement::ExternFunction {
                    name,
                    varargs,
                    args,
                    returns,
                    ..
                } => self.module.add_extern_function(
                    name,
                    ExternFunction {
                        varargs,
                        arguments: args,
                        returns: returns,
                    },
                ),
                Statement::Impl { struct_impl, funcs, .. } => {
                    for func in funcs {
                        if let Statement::Function { name, args, .. } = func {
                            self.signatures.insert(
                                format!("{}.{}", struct_impl.to_string(), name),
                                args.into_iter().map(|(t, _)| t).collect(),
                            );
                        }
                    }
                }
                Statement::Function { name, args, .. } => {
                    self.signatures
                        .insert(name.to_string(), args.into_iter().map(|(t, _)| t).collect());
                }
                Statement::Assign(_, name, _, val, ..) => {
                    let value = match val.expression {
                        Expression::Integer(i) => GlobalValue::Int(i),
                        Expression::Negate(inner) => match inner.expression {
                            Expression::Integer(i) => GlobalValue::Int(-i),
                            Expression::Float(f) => GlobalValue::Float(-f),
                            _ => unreachable!(),
                        },
                        Expression::Float(f) => GlobalValue::Float(f),
                        Expression::Boolean(b) => GlobalValue::Bool(b),
                        Expression::String(s) => {
                            let ptr = match self.module.add_string(s) {
                                Value::Global(v) => v,
                                _ => unreachable!(),
                            };
                            GlobalValue::String(ptr)
                        }
                        _ => unreachable!(),
                    };
                    self.module.global_values.insert(name, value);
                }
                Statement::Struct {
                    name, attributes, ..
                } => {
                    self.module.add_struct(name, Struct { name, attributes });
                }
                Statement::Enum { name, variants, payloads, .. } => {
                    self.module.add_enum(
                        name.to_string(),
                        variants.iter().map(|v| v.to_string()).collect(),
                    );
                    if payloads.iter().any(|p| !p.is_empty()) {
                        self.register_boxed_enum(name, &variants, &payloads);
                    }
                }
                _ => {}
            }
        }
        // Pass 2: codegen function bodies
        for stmt in stmts {
            match stmt {
                Statement::Function { .. } => self.codegen_function(stmt.clone()),
                Statement::Impl { .. } => self.codegen_impl(stmt.clone()),
                _ => {}
            }
        }
    }

    pub fn insert_implicit_return(&mut self) {
        for (_, func) in self.module.functions.iter_mut() {
            // Falling off the end of a function that returns a value can't happen
            // in a well-formed program (every path returns), so mark it unreachable.
            let terminator = if func.returns == AzulaType::Void {
                Instruction::Return(None)
            } else {
                Instruction::Unreachable
            };
            for (_, block) in func.blocks.iter_mut() {
                if !is_terminator(block.instructions.last()) {
                    block.instructions.push(terminator.clone());
                }
            }
        }
    }

    pub fn codegen_function(&mut self, stmt: Statement<'a>) {
        if let Statement::Function {
            name,
            args,
            returns,
            body,
            ..
        } = stmt
        {
            let mut arguments = vec![];
            for (typ, name) in args {
                arguments.push((name.to_string(), typ));
            }

            let mut function = Function::new();
            function.arguments = arguments;
            function.returns = returns;
            spill_arguments(&mut function);
            self.scopes = vec![HashMap::new()];

            if let Statement::Block(stmts) = body.as_ref().clone() {
                for stmt in stmts {
                    self.codegen_statement(stmt, &mut function);
                }
            }

            self.module.add_function(name.to_string(), function)
        } else {
            unreachable!()
        }
    }

    pub fn codegen_impl(&mut self, stmt: Statement<'a>) {
        if let Statement::Impl {
            struct_impl,
            trait_impl: _,
            funcs,
            span: _,
        } = stmt
        {
            for func in funcs {
                match func {
                    Statement::Function {
                        name,
                        args,
                        returns,
                        body,
                        ..
                    } => {
                        let mut arguments = vec![];
                        for (typ, name) in args {
                            arguments.push((name.to_string(), typ));
                        }

                        let mut function = Function::new();
                        function.arguments = arguments;
                        function.returns = returns;
                        spill_arguments(&mut function);
                        self.scopes = vec![HashMap::new()];

                        if let Statement::Block(stmts) = body.as_ref().clone() {
                            for stmt in stmts {
                                self.codegen_statement(stmt, &mut function);
                            }
                        }

                        let gen_name = format!("{}.{}", struct_impl.to_string(), name);

                        self.module.add_function(gen_name, function)
                    }
                    _ => unreachable!(),
                };
            }
        } else {
            unreachable!()
        }
    }

    pub fn codegen_statement(&mut self, stmt: Statement<'a>, func: &mut Function<'a>) {
        match stmt {
            Statement::Assign(..) => self.codegen_assign(stmt, func),
            Statement::Return(..) => self.codegen_return(stmt, func),
            Statement::Group(stmts) => {
                for stmt in stmts {
                    self.codegen_statement(stmt, func);
                }
            }
            Statement::ExpressionStatement(expr, ..) => {
                self.codegen_expr(expr.clone(), func, true);
            }
            Statement::If(..) => self.codegen_if(stmt, func),
            Statement::While(..) => self.codegen_while(stmt, func),
            Statement::For(..) => self.codegen_for(stmt, func),
            Statement::Break(..) => {
                let end = self.loop_end_stack.last().expect("break outside loop").clone();
                func.jump(end);
            }
            Statement::Continue(..) => {
                let cont = self.loop_continue_stack.last().expect("continue outside loop").clone();
                func.jump(cont);
            }
            Statement::Reassign(..) => self.codegen_reassign(stmt, func),
            Statement::CompoundAssign(target, op, value, _) => {
                // Find the target once, then load, combine and store through it
                let typ = target.typed.clone();
                let ptr = self.codegen_address(target, func);
                let zero = func.const_int(0);
                let old = func.access_element(ptr.clone(), zero.clone(), typ.clone());
                let rhs = self.codegen_expr(value, func, true);
                let result = func.emit(|dest| match op {
                    Operator::Add => Instruction::Add(old, rhs, dest),
                    Operator::Sub => Instruction::Sub(old, rhs, dest),
                    Operator::Mul => Instruction::Mul(old, rhs, dest),
                    Operator::Div => Instruction::Div(old, rhs, dest),
                    Operator::Mod => Instruction::Mod(old, rhs, dest),
                    Operator::BitAnd => Instruction::BitAnd(old, rhs, dest),
                    Operator::BitOr => Instruction::BitOr(old, rhs, dest),
                    Operator::BitXor => Instruction::BitXor(old, rhs, dest),
                    Operator::Shl => Instruction::Shl(old, rhs, dest),
                    Operator::Shr => Instruction::Shr(old, rhs, dest),
                    _ => unreachable!("compound assignment with {:?}", op),
                });
                func.store_element(ptr, zero, result, typ);
            }
            Statement::Block(stmts) => {
                self.scopes.push(HashMap::new());
                for s in stmts {
                    self.codegen_statement(s, func);
                }
                self.scopes.pop();
            }
            _ => panic!(),
        }
    }

    pub fn codegen_assign(&mut self, stmt: Statement<'a>, func: &mut Function<'a>) {
        if let Statement::Assign(_, name, annotation, expr, _) = stmt {
            let value = self.codegen_expr(expr.clone(), func, true);
            let var_type = annotation.unwrap_or_else(|| expr.typed.clone());
            let name = self.declare_variable(&name, var_type.clone(), func);
            func.store(name, value, var_type);
        } else {
            unreachable!()
        }
    }

    pub fn codegen_reassign(&mut self, stmt: Statement<'a>, func: &mut Function<'a>) {
        if let Statement::Reassign(var, val, _) = stmt {
            let value = self.codegen_expr(val.clone(), func, true);
            match var.expression {
                Expression::Identifier(v) => {
                    let name = self.variable_name(&v, func);
                    func.store(name, value, var.typed.clone())
                }
                Expression::Deref(pointer) => {
                    let ptr = self.codegen_expr(pointer.deref().clone(), func, true);
                    let zero = func.const_int(0);
                    func.store_element(ptr, zero, value, var.typed.clone());
                }
                Expression::ArrayAccess(array, index) => {
                    let elem_type = element_type(&array.typed);
                    let array = self.codegen_expr(array.deref().clone(), func, true);
                    let index = self.codegen_expr(index.deref().clone(), func, true);
                    func.store_element(array.clone(), index, value, elem_type);
                }
                Expression::StructAccess(struc, member) => {
                    // For heap pointers (&T), load the pointer value; for stack structs (T), take its address.
                    let struc_val = if matches!(struc.typed, AzulaType::Pointer(_)) {
                        self.codegen_expr(struc.deref().clone(), func, true)
                    } else {
                        self.codegen_address(struc.deref().clone(), func)
                    };
                    let member_name = match &member.expression {
                        Expression::Identifier(v) => v,
                        _ => unreachable!(),
                    };
                    let struct_name = match &struc.typed {
                        AzulaType::Named(name) => name.clone(),
                        AzulaType::Pointer(nested) => match nested.deref().clone() {
                            AzulaType::Named(name) => name.clone(),
                            _ => unreachable!("{:?}", struc.typed),
                        },
                        _ => unreachable!("{:?}", struc.typed),
                    };

                    let struct_def = self.module.structs.get(struct_name.as_str()).unwrap();
                    let index = struct_def
                        .attributes
                        .iter()
                        .enumerate()
                        .find(|(_, (_, name))| name.to_string() == member_name.to_string())
                        .map(|(index, _)| index)
                        .unwrap();
                    func.store_struct_member(struc_val.clone(), index, value, struct_name)
                }
                _ => todo!(),
            }
        } else {
            unreachable!()
        }
    }

    pub fn codegen_return(&mut self, stmt: Statement<'a>, func: &mut Function<'a>) {
        if let Statement::Return(val, _) = stmt {
            match val {
                Some(expr) => {
                    let value = self.codegen_expr(expr, func, true);
                    func.ret(Some(value));
                }
                None => func.ret(None),
            }
        } else {
            unreachable!()
        }
    }

    pub fn codegen_if(&mut self, stmt: Statement<'a>, func: &mut Function<'a>) {
        if let Statement::If(cond, body, else_branch, ..) = stmt {
            let cond = self.codegen_expr(cond, func, true);

            let true_name = format!("true-{}", func.if_block_index);
            let else_name = format!("else-{}", func.if_block_index);
            let end_name = format!("end-{}", func.if_block_index);

            func.if_block_index += 1;

            let false_target = if else_branch.is_some() { else_name.clone() } else { end_name.clone() };
            func.jcond(cond, true_name.clone(), false_target);
            func.blocks.push((true_name.clone(), Block::new()));

            func.current_block = true_name.clone();

            self.scopes.push(HashMap::new());
            for stmt in body {
                self.codegen_statement(stmt, func);
            }
            self.scopes.pop();

            for (name, block) in &func.blocks.clone() {
                if name.clone() == func.current_block {
                    if !is_terminator(block.instructions.last()) {
                        func.jump(end_name.clone());
                    }
                }
            }

            if let Some(else_stmt) = else_branch {
                func.blocks.push((else_name.clone(), Block::new()));
                func.current_block = else_name.clone();
                self.scopes.push(HashMap::new());
                self.codegen_statement(else_stmt.as_ref().clone(), func);
                self.scopes.pop();

                for (name, block) in &func.blocks.clone() {
                    if name.clone() == func.current_block {
                        if !is_terminator(block.instructions.last()) {
                            func.jump(end_name.clone());
                        }
                    }
                }
            }

            func.blocks.push((end_name.clone(), Block::new()));
            func.current_block = end_name.clone();
        } else {
            unreachable!()
        }
    }

    pub fn codegen_while(&mut self, stmt: Statement<'a>, func: &mut Function<'a>) {
        if let Statement::While(cond, body, ..) = stmt {
            let eval_name = format!("eval-{}", func.if_block_index);
            let true_name = format!("loop-{}", func.if_block_index);
            let end_name = format!("end-{}", func.if_block_index);

            func.if_block_index += 1;

            self.loop_end_stack.push(end_name.clone());
            self.loop_continue_stack.push(eval_name.clone());

            func.jump(eval_name.clone());
            func.blocks.push((eval_name.clone(), Block::new()));
            func.current_block = eval_name.clone();
            let cond_val = self.codegen_expr(cond.clone(), func, true);
            func.jcond(cond_val, true_name.clone(), end_name.clone());

            func.blocks.push((true_name.clone(), Block::new()));
            func.current_block = true_name.clone();

            self.scopes.push(HashMap::new());
            for stmt in body {
                self.codegen_statement(stmt, func);
            }
            self.scopes.pop();
            func.jump(eval_name.clone());

            self.loop_end_stack.pop();
            self.loop_continue_stack.pop();

            func.blocks.push((end_name.clone(), Block::new()));
            func.current_block = end_name.clone();
        } else {
            unreachable!()
        }
    }

    pub fn codegen_for(&mut self, stmt: Statement<'a>, func: &mut Function<'a>) {
        if let Statement::For(cond, body, ..) = stmt {
            let loop_name = format!("loop-{}", func.if_block_index);
            let end_name = format!("end-{}", func.if_block_index);

            self.loop_end_stack.push(end_name.clone());

            match cond {
                Some(cond) => {
                    let eval_name = format!("eval-{}", func.if_block_index);
                    func.if_block_index += 1;

                    // continue → jump back to eval (re-check condition)
                    self.loop_continue_stack.push(eval_name.clone());

                    func.jump(eval_name.clone());
                    func.blocks.push((eval_name.clone(), Block::new()));
                    func.current_block = eval_name.clone();
                    let cond_val = self.codegen_expr(cond, func, true);
                    func.jcond(cond_val, loop_name.clone(), end_name.clone());

                    func.blocks.push((loop_name.clone(), Block::new()));
                    func.current_block = loop_name.clone();
                    self.scopes.push(HashMap::new());
                    for s in body {
                        self.codegen_statement(s, func);
                    }
                    self.scopes.pop();
                    let last = func.blocks.iter()
                        .find(|(n, _)| n == &func.current_block)
                        .and_then(|(_, b)| b.instructions.last());
                    if !is_terminator(last) {
                        func.jump(eval_name.clone());
                    }
                    self.loop_continue_stack.pop();
                }
                None => {
                    func.if_block_index += 1;

                    // continue → jump back to loop top
                    self.loop_continue_stack.push(loop_name.clone());

                    func.jump(loop_name.clone());
                    func.blocks.push((loop_name.clone(), Block::new()));
                    func.current_block = loop_name.clone();
                    self.scopes.push(HashMap::new());
                    for s in body {
                        self.codegen_statement(s, func);
                    }
                    self.scopes.pop();
                    let last = func.blocks.iter()
                        .find(|(n, _)| n == &func.current_block)
                        .and_then(|(_, b)| b.instructions.last());
                    if !is_terminator(last) {
                        func.jump(loop_name.clone());
                    }
                    self.loop_continue_stack.pop();
                }
            }

            self.loop_end_stack.pop();

            func.blocks.push((end_name.clone(), Block::new()));
            func.current_block = end_name.clone();
        } else {
            unreachable!()
        }
    }

    pub fn codegen_expr(
        &mut self,
        expr: ExpressionNode<'a>,
        func: &mut Function<'a>,
        resolve_pointer: bool,
    ) -> Value {
        // A call of a function returning `!` never comes back
        let never = expr.typed == AzulaType::Never && matches!(expr.expression, Expression::FunctionCall { .. });
        let value = self.codegen_expr_inner(expr, func, resolve_pointer);
        if never {
            func.unreachable();
        }
        value
    }

    fn codegen_expr_inner(
        &mut self,
        expr: ExpressionNode<'a>,
        func: &mut Function<'a>,
        resolve_pointer: bool,
    ) -> Value {
        match expr.expression {
            Expression::Infix(..) => self.codegen_infix(expr, func, resolve_pointer),
            Expression::Integer(val) => func.const_int(val),
            Expression::Float(val) => func.const_float(val),
            Expression::Identifier(name) if resolve_pointer => {
                if let Some(name) = self.lookup_variable(&name, func) {
                    func.load(name, expr.typed)
                } else if let Some(val) = self.module.global_values.get(&name) {
                    if let GlobalValue::String(v) = val {
                        return Value::Global(*v);
                    }
                    func.load_global(name, expr.typed)
                } else if name == "nil" {
                    func.const_null()
                } else {
                    unreachable!()
                }
            }
            Expression::Identifier(name) => {
                let name = self.variable_name(&name, func);
                func.ptr(name)
            }
            Expression::String(val) => self.module.add_string(val),
            Expression::Boolean(val) => {
                if val {
                    func.const_true()
                } else {
                    func.const_false()
                }
            }
            Expression::FunctionCall { function, args } if self.as_boxed_variant(&function).is_some() => {
                let (enum_name, variant) = self.as_boxed_variant(&function).unwrap();
                self.construct_variant(&enum_name, &variant, args, func)
            }
            Expression::FunctionCall { function, mut args } => {
                let name = self.resolve_function(function.deref().clone());

                // if name == "__array_len" {
                //     match args[0].typed {
                //         AzulaType::Array(_, size) => return func.const_int(size.unwrap() as i64),
                //         _ => unreachable!("{:?}", args[0].typed),
                //     }
                // }

                if let Expression::StructAccess(left, right) = &function.expression {
                    // Check if the method's first parameter is a pointer (mutating self)
                    let receiver_type = left.typed.to_string();
                    let method_name = if let Expression::Identifier(m) = &right.expression { m.clone() } else { String::new() };
                    let mangled = format!("{}.{}", receiver_type, method_name);
                    let self_is_ptr = self.signatures.get(&mangled)
                        .and_then(|args| args.first())
                        .map(|t| matches!(t, AzulaType::Pointer(_)))
                        .unwrap_or(false);
                    let receiver_is_ptr = matches!(left.typed, AzulaType::Pointer(_));

                    if self_is_ptr && !receiver_is_ptr {
                        // Receiver is a value type — take its address
                        args.insert(0, ExpressionNode {
                            expression: Expression::Pointer(left.clone()),
                            typed: AzulaType::Pointer(Rc::new(left.typed.clone())),
                            span: left.span.clone(),
                        });
                    } else if !self_is_ptr && receiver_is_ptr {
                        // Method takes the value but we have a pointer — dereference it
                        let pointee = match &left.typed {
                            AzulaType::Pointer(inner) => inner.as_ref().clone(),
                            _ => unreachable!(),
                        };
                        args.insert(0, ExpressionNode {
                            expression: Expression::ArrayAccess(left.clone(), Rc::new(ExpressionNode {
                                expression: Expression::Integer(0),
                                typed: AzulaType::Int,
                                span: left.span.clone(),
                            })),
                            typed: pointee,
                            span: left.span.clone(),
                        });
                    } else {
                        args.insert(0, left.as_ref().clone());
                    }
                }

                let args = args
                    .iter()
                    .map(|arg| self.codegen_expr(arg.clone(), func, true))
                    .collect();

                func.function_call(name.clone(), args)
            }
            Expression::Not(expr) => {
                let val = self.codegen_expr(expr.as_ref().clone(), func, true);

                func.not(val)
            }
            Expression::BitNot(expr) => {
                let val = self.codegen_expr(expr.as_ref().clone(), func, true);
                func.not(val)
            }
            Expression::SizeOf(typ) => func.emit(|dest| Instruction::SizeOf(typ, dest)),
            Expression::Negate(expr) => {
                let inner = expr.as_ref().clone();
                let zero = match inner.typed {
                    AzulaType::Float => func.const_float(0.0),
                    _ => func.const_int(0),
                };
                let val = self.codegen_expr(inner, func, true);
                func.sub(zero, val)
            }
            Expression::Pointer(expr) => self.codegen_address(expr.deref().clone(), func),
            Expression::Interpolation(_) | Expression::Tuple(_) | Expression::Closure(..) | Expression::Try(_) => {
                unreachable!("interpolations, tuples and closures are rewritten by the typechecker")
            }
            // A closure object: { code, captured cell, ... }
            Expression::MakeClosure(name, captures) => {
                let size = func.const_int(8 * (captures.len() as i64 + 1));
                let object = func.function_call("malloc".to_string(), vec![size]);
                let code = func.function_address(name);
                func.function_call("ptr_write_str".to_string(), vec![object.clone(), code]);
                for (i, capture) in captures.into_iter().enumerate() {
                    let cell = self.codegen_expr(capture, func, true);
                    let offset = func.const_int(8 * (i as i64 + 1));
                    let slot = func.function_call("ptr_add".to_string(), vec![object.clone(), offset]);
                    func.function_call("ptr_write_str".to_string(), vec![slot, cell]);
                }
                object
            }
            Expression::NewCell(value) => {
                let typ = value.typed.clone();
                let size = func.const_int(struct_byte_size(&self.module, &typ).max(8) as i64);
                let v = self.codegen_expr(value.as_ref().clone(), func, true);
                let cell = func.function_call("malloc".to_string(), vec![size]);
                let zero = func.const_int(0);
                func.store_element(cell.clone(), zero, v, typ);
                cell
            }
            // Captured cell `n` of the closure being run
            Expression::EnvCell(n) => {
                let name = self.lookup_variable("$env", func).expect("closure environment");
                let env = func.load(name, AzulaType::Str);
                let offset = func.const_int(8 * (n as i64 + 1));
                let slot = func.function_call("ptr_add".to_string(), vec![env, offset]);
                func.function_call("ptr_read_str".to_string(), vec![slot])
            }
            // Call the object's code, passing the object as the environment
            Expression::CallClosure(callee, args) => {
                let (params, returns) = match &callee.typed {
                    AzulaType::Function(params, returns) => (params.clone(), returns.as_ref().clone()),
                    other => unreachable!("calling a {:?}", other),
                };
                let object = self.codegen_expr(callee.as_ref().clone(), func, true);
                let code = func.function_call("ptr_read_str".to_string(), vec![object.clone()]);
                let mut values = vec![object];
                for arg in args {
                    values.push(self.codegen_expr(arg, func, true));
                }
                let mut all_params = vec![AzulaType::Str];
                all_params.extend(params);
                let result = func.indirect_call(code, values, all_params, returns.clone());
                if returns == AzulaType::Never {
                    func.unreachable();
                }
                result
            }
            Expression::Deref(pointer) => {
                let ptr = self.codegen_expr(pointer.deref().clone(), func, true);
                let zero = func.const_int(0);
                func.access_element(ptr, zero, expr.typed)
            }
            Expression::Array(vals) => {
                let elem_type = vals[0].typed.clone();
                let array = func.create_array(elem_type.clone(), vals.len());

                for (index, val) in vals.iter().enumerate() {
                    let gened = self.codegen_expr(val.clone(), func, true);
                    let index = func.const_int(index as i64);
                    func.store_element(array.clone(), index, gened, elem_type.clone());
                }

                return array;
            }
            Expression::ArrayAccess(array, index) => {
                let elem_type = element_type(&array.typed);
                let array = self.codegen_expr(array.deref().clone(), func, true);
                let index = self.codegen_expr(index.deref().clone(), func, true);

                func.access_element(array, index, elem_type)
            }
            Expression::StructInitialisation(struc, vals) => {
                let values: Vec<_> = vals
                    .iter()
                    .map(|(_, v)| self.codegen_expr(v.deref().clone(), func, true))
                    .collect();

                let name = match &struc.expression {
                    Expression::Identifier(s) => s,
                    _ => unreachable!(),
                };

                func.create_struct(name.clone(), values)
            }
            Expression::StructAccess(struc, member) => {
                let struct_value = self.codegen_expr(struc.deref().clone(), func, true);

                let member_name = match &member.expression {
                    Expression::Identifier(s) => s.clone(),
                    _ => unreachable!(),
                };

                let struct_name = match &struc.typed {
                    AzulaType::Named(name) => name.clone(),
                    AzulaType::Pointer(nested) => match nested.deref().clone() {
                        AzulaType::Named(name) => name.clone(),
                        _ => unreachable!("{:?}", struc.typed),
                    },
                    _ => unreachable!("{:?}", struc.typed),
                };

                let struct_def = self.module.structs.get(struct_name.as_str()).unwrap();
                let index = struct_def
                    .attributes
                    .iter()
                    .enumerate()
                    .find(|(_, (_, name))| name.to_string() == member_name)
                    .map(|(index, _)| index)
                    .unwrap();

                func.access_struct_member(struct_value, index, resolve_pointer, struct_name)
            }
            Expression::NamespaceAccess(ns, variant) => {
                let enum_name = match &ns.expression {
                    Expression::Identifier(s) => s.clone(),
                    _ => unreachable!(),
                };
                let variant_name = match &variant.expression {
                    Expression::Identifier(s) => s.clone(),
                    _ => unreachable!(),
                };
                if self.module.boxed_enums.contains(&enum_name) {
                    return self.construct_variant(&enum_name, &variant_name, vec![], func);
                }
                func.const_int(self.variant_index(&enum_name, &variant_name) as i64)
            }
            Expression::Match(scrutinee, arms) => {
                self.codegen_match(scrutinee, arms, expr.typed, func)
            }
            Expression::Cast(inner, target_type) => {
                let val = self.codegen_expr(inner.as_ref().clone(), func, true);
                func.cast(val, target_type)
            }
            Expression::Null => func.const_null(),
            Expression::Turbofish(..) => unreachable!("turbofish should be resolved by the typechecker"),
            Expression::Block(stmts, final_expr) => {
                self.scopes.push(HashMap::new());
                for stmt in stmts {
                    self.codegen_statement(stmt, func);
                }
                let value = match final_expr {
                    Some(fe) => self.codegen_expr(fe.as_ref().clone(), func, resolve_pointer),
                    None => Value::LiteralInteger(0),
                };
                self.scopes.pop();
                value
            }
            Expression::Alloc(inner) => {
                if let Expression::StructInitialisation(struc, vals) = &inner.expression {
                    let struct_name = match &struc.expression {
                        Expression::Identifier(s) => s.clone(),
                        _ => unreachable!(),
                    };
                    // Compute field values before malloc so they don't alias the pointer
                    let field_vals: Vec<_> = vals
                        .iter()
                        .map(|(_, v)| self.codegen_expr(v.clone(), func, true))
                        .collect();
                    // Compute the true byte size by summing field sizes (struct fields may be >8 bytes)
                    let size_bytes: usize = if let Some(struc_def) = self.module.structs.get(struct_name.as_str()) {
                        struc_def.attributes.iter().map(|(t, _)| struct_byte_size(&self.module, t)).sum()
                    } else {
                        vals.len() * 8
                    };
                    let size = func.const_int(size_bytes as i64);
                    let ptr = func.function_call("malloc".to_string(), vec![size]);
                    // Store each field through the pointer
                    for (i, val) in field_vals.into_iter().enumerate() {
                        func.store_struct_member(ptr.clone(), i, val, struct_name.clone());
                    }
                    ptr
                } else {
                    unreachable!("alloc requires a struct initialisation expression")
                }
            }
        }
    }

    /// Declare a new variable in the innermost scope, returning its unique IR name.
    fn declare_variable(&mut self, name: &str, typ: AzulaType<'a>, func: &mut Function<'a>) -> String {
        let unique = if func.variables.contains_key(name) {
            self.var_counter += 1;
            format!("{}.{}", name, self.var_counter)
        } else {
            name.to_string()
        };
        func.variables.insert(unique.clone(), typ);
        self.scopes.last_mut().unwrap().insert(name.to_string(), unique.clone());
        unique
    }

    /// Find the IR name of a variable visible from the current scope.
    fn lookup_variable(&self, name: &str, func: &Function<'a>) -> Option<String> {
        for scope in self.scopes.iter().rev() {
            if let Some(unique) = scope.get(name) {
                return Some(unique.clone());
            }
        }
        // Arguments and compiler temporaries live directly in the function's variables
        func.variables.contains_key(name).then(|| name.to_string())
    }

    fn variable_name(&self, name: &str, func: &Function<'a>) -> String {
        self.lookup_variable(name, func)
            .unwrap_or_else(|| panic!("Unknown variable {}", name))
    }

    /// Lay out each variant of an enum with payloads as a struct `Enum.Variant`
    /// holding the tag followed by the payload fields.
    fn register_boxed_enum(&mut self, name: &str, variants: &[&'a str], payloads: &[Vec<AzulaType<'a>>]) {
        self.module.boxed_enums.insert(name.to_string());
        let leak = |s: String| -> &'a str { Box::leak(s.into_boxed_str()) };
        self.module.add_struct(
            leak(format!("{}.__tag", name)),
            Struct { name: leak(format!("{}.__tag", name)), attributes: vec![(AzulaType::Int, "tag")] },
        );
        for (variant, payload) in variants.iter().zip(payloads) {
            let struct_name = leak(format!("{}.{}", name, variant));
            let mut attributes = vec![(AzulaType::Int, "tag")];
            for (i, typ) in payload.iter().enumerate() {
                attributes.push((typ.clone(), leak(format!("_{}", i))));
            }
            self.module.add_struct(struct_name, Struct { name: struct_name, attributes });
        }
    }

    fn variant_index(&self, enum_name: &str, variant: &str) -> usize {
        self.module
            .enums
            .get(enum_name)
            .unwrap_or_else(|| panic!("Unknown enum {}", enum_name))
            .iter()
            .position(|v| v == variant)
            .unwrap_or_else(|| panic!("Unknown variant {} on {}", variant, enum_name))
    }

    /// Allocate a boxed enum value holding `variant` and its payload.
    fn construct_variant(&mut self, enum_name: &str, variant: &str, args: Vec<ExpressionNode<'a>>, func: &mut Function<'a>) -> Value {
        let struct_name = format!("{}.{}", enum_name, variant);
        let values: Vec<_> = args.into_iter().map(|a| self.codegen_expr(a, func, true)).collect();
        let size = func.emit(|dest| Instruction::SizeOf(AzulaType::Named(struct_name.clone()), dest));
        let ptr = func.function_call("malloc".to_string(), vec![size]);
        let tag = func.const_int(self.variant_index(enum_name, variant) as i64);
        func.store_struct_member(ptr.clone(), 0, tag, struct_name.clone());
        for (i, value) in values.into_iter().enumerate() {
            func.store_struct_member(ptr.clone(), i + 1, value, struct_name.clone());
        }
        ptr
    }

    /// If `function` names a variant of an enum with payloads, return (enum, variant).
    fn as_boxed_variant(&self, function: &ExpressionNode<'a>) -> Option<(String, String)> {
        if let Expression::NamespaceAccess(ns, variant) = &function.expression {
            if let (Expression::Identifier(ns), Expression::Identifier(variant)) = (&ns.expression, &variant.expression) {
                let is_variant = self.module.enums.get(ns).map(|v| v.contains(variant)).unwrap_or(false);
                if self.module.boxed_enums.contains(ns) && is_variant {
                    return Some((ns.clone(), variant.clone()));
                }
            }
        }
        None
    }

    /// Generate a pointer to the storage behind an lvalue expression. Expressions
    /// that are not lvalues are evaluated into a fresh temporary whose address is returned.
    fn codegen_address(&mut self, expr: ExpressionNode<'a>, func: &mut Function<'a>) -> Value {
        match &expr.expression {
            Expression::Identifier(name) if self.lookup_variable(name, func).is_some() => {
                let name = self.variable_name(name, func);
                func.ptr(name)
            }
            Expression::StructAccess(struc, member) => {
                let struct_name = match &struc.typed {
                    AzulaType::Named(name) => name.clone(),
                    AzulaType::Pointer(nested) => match nested.deref() {
                        AzulaType::Named(name) => name.clone(),
                        _ => unreachable!("{:?}", struc.typed),
                    },
                    _ => unreachable!("{:?}", struc.typed),
                };
                let base = if matches!(struc.typed, AzulaType::Pointer(_)) {
                    self.codegen_expr(struc.deref().clone(), func, true)
                } else {
                    self.codegen_address(struc.deref().clone(), func)
                };
                let member_name = match &member.expression {
                    Expression::Identifier(s) => s.clone(),
                    _ => unreachable!(),
                };
                let index = self.struct_member_index(&struct_name, &member_name);
                func.access_struct_member(base, index, false, struct_name)
            }
            Expression::ArrayAccess(array, index) => {
                let elem_type = element_type(&array.typed);
                let array = self.codegen_expr(array.deref().clone(), func, true);
                let index = self.codegen_expr(index.deref().clone(), func, true);
                func.emit(|dest| Instruction::ElementPtr(array, index, dest, elem_type))
            }
            Expression::Deref(pointer) => self.codegen_expr(pointer.deref().clone(), func, true),
            _ => {
                let temp = format!("__tmp_{}", func.if_block_index);
                func.if_block_index += 1;
                let typ = expr.typed.clone();
                let value = self.codegen_expr(expr, func, true);
                func.variables.insert(temp.clone(), typ.clone());
                func.store(temp.clone(), value, typ);
                func.ptr(temp)
            }
        }
    }

    fn struct_member_index(&self, struct_name: &str, member_name: &str) -> usize {
        self.module
            .structs
            .get(struct_name)
            .unwrap_or_else(|| panic!("Unknown struct {}", struct_name))
            .attributes
            .iter()
            .position(|(_, name)| *name == member_name)
            .unwrap_or_else(|| panic!("Unknown member {} on {}", member_name, struct_name))
    }

    fn codegen_match(
        &mut self,
        scrutinee: Rc<ExpressionNode<'a>>,
        arms: Vec<(MatchPattern<'a>, ExpressionNode<'a>)>,
        result_type: AzulaType<'a>,
        func: &mut Function<'a>,
    ) -> Value {
        let n = func.match_block_index;
        func.match_block_index += 1;

        let enum_name = match &scrutinee.typed {
            AzulaType::Named(name) => name.clone(),
            _ => String::new(),
        };
        let boxed = self.module.boxed_enums.contains(&enum_name);

        let scrut_val = self.codegen_expr(scrutinee.deref().clone(), func, true);
        // Boxed enums dispatch on the tag stored at the start of the cell
        let tag_val = if boxed {
            func.access_struct_member(scrut_val.clone(), 0, true, format!("{}.__tag", enum_name))
        } else {
            scrut_val.clone()
        };

        let is_void = result_type == AzulaType::Void;
        let result_var = format!("__match_{}", n);
        if !is_void {
            func.variables.insert(result_var.clone(), result_type.clone());
        }

        let end_block = format!("match-end-{}", n);

        let arm_count = arms.len();
        for (i, (pattern, body)) in arms.into_iter().enumerate() {
            let arm_block = format!("match-arm-{}-{}", n, i);
            let next_block = if i + 1 < arm_count {
                format!("match-next-{}-{}", n, i + 1)
            } else {
                end_block.clone()
            };

            let is_wildcard = matches!(pattern, MatchPattern::Wildcard);
            match &pattern {
                MatchPattern::Wildcard => func.jump(arm_block.clone()),
                MatchPattern::Tuple(items) => {
                    let mut tests = 0;
                    let prefix = format!("match-test-{}-{}", n, i);
                    self.codegen_tuple_test(items, scrut_val.clone(), &enum_name, &prefix, &mut tests, &next_block, func);
                    func.jump(arm_block.clone());
                }
                MatchPattern::Binding(_) => unreachable!("bindings only appear inside tuple patterns"),
                MatchPattern::Integer(value) => {
                    let expected = func.const_int(*value);
                    let cond = func.eq(tag_val.clone(), expected);
                    func.jcond(cond, arm_block.clone(), next_block.clone());
                }
                MatchPattern::Variant(enum_name, variant) | MatchPattern::Destructure(enum_name, variant, _) => {
                    let expected = func.const_int(self.variant_index(enum_name, variant) as i64);
                    let cond = func.eq(tag_val.clone(), expected);
                    func.jcond(cond, arm_block.clone(), next_block.clone());
                }
            }

            func.blocks.push((arm_block.clone(), Block::new()));
            func.current_block = arm_block.clone();
            self.scopes.push(HashMap::new());

            if let MatchPattern::Tuple(items) = &pattern {
                self.bind_tuple_pattern(items, scrut_val.clone(), &enum_name, func);
            }
            if let MatchPattern::Destructure(enum_name, variant, bindings) = &pattern {
                let struct_name = format!("{}.{}", enum_name, variant);
                for (field, binding) in bindings.iter().enumerate() {
                    if let Some(name) = binding {
                        let typ = self.module.structs[struct_name.as_str()].attributes[field + 1].0.clone();
                        let value = func.access_struct_member(scrut_val.clone(), field + 1, true, struct_name.clone());
                        let unique = self.declare_variable(name, typ.clone(), func);
                        func.store(unique, value, typ);
                    }
                }
            }

            let body_val = self.codegen_expr(body, func, true);
            if !current_block_terminated(func) {
                if !is_void {
                    func.store(result_var.clone(), body_val, result_type.clone());
                }
                func.jump(end_block.clone());
            }
            self.scopes.pop();

            if is_wildcard {
                // Later arms are unreachable
                break;
            }
            if i + 1 < arm_count {
                func.blocks.push((next_block.clone(), Block::new()));
                func.current_block = next_block.clone();
            }
        }

        func.blocks.push((end_block.clone(), Block::new()));
        func.current_block = end_block.clone();

        if is_void {
            Value::LiteralInteger(0)
        } else {
            func.load(result_var, result_type)
        }
    }

    /// Branch to `fail` unless the tuple `value` (of struct `tuple`) matches `items`
    fn codegen_tuple_test(
        &mut self,
        items: &[MatchPattern<'a>],
        value: Value,
        tuple: &str,
        prefix: &str,
        tests: &mut usize,
        fail: &str,
        func: &mut Function<'a>,
    ) {
        for (index, item) in items.iter().enumerate() {
            let cond = match item {
                MatchPattern::Wildcard | MatchPattern::Binding(_) => continue,
                MatchPattern::Integer(expected) => {
                    let element = func.access_struct_member(value.clone(), index, true, tuple.to_string());
                    let expected = func.const_int(*expected);
                    func.eq(element, expected)
                }
                MatchPattern::Variant(enum_name, variant) => {
                    let element = func.access_struct_member(value.clone(), index, true, tuple.to_string());
                    let tag = if self.module.boxed_enums.contains(&enum_name.to_string()) {
                        func.access_struct_member(element, 0, true, format!("{}.__tag", enum_name))
                    } else {
                        element
                    };
                    let expected = func.const_int(self.variant_index(enum_name, variant) as i64);
                    func.eq(tag, expected)
                }
                MatchPattern::Tuple(inner) => {
                    let element = func.access_struct_member(value.clone(), index, true, tuple.to_string());
                    let inner_tuple = self.module.structs[tuple].attributes[index].0.to_string();
                    self.codegen_tuple_test(inner, element, &inner_tuple, prefix, tests, fail, func);
                    continue;
                }
                MatchPattern::Destructure(..) => unreachable!(),
            };
            let pass = format!("{}-{}", prefix, tests);
            *tests += 1;
            func.jcond(cond, pass.clone(), fail.to_string());
            func.blocks.push((pass.clone(), Block::new()));
            func.current_block = pass;
        }
    }

    /// Declare the variables bound by a tuple pattern
    fn bind_tuple_pattern(&mut self, items: &[MatchPattern<'a>], value: Value, tuple: &str, func: &mut Function<'a>) {
        for (index, item) in items.iter().enumerate() {
            match item {
                MatchPattern::Binding(name) => {
                    let typ = self.module.structs[tuple].attributes[index].0.clone();
                    let element = func.access_struct_member(value.clone(), index, true, tuple.to_string());
                    let unique = self.declare_variable(name, typ.clone(), func);
                    func.store(unique, element, typ);
                }
                MatchPattern::Tuple(inner) => {
                    let element = func.access_struct_member(value.clone(), index, true, tuple.to_string());
                    let inner_tuple = self.module.structs[tuple].attributes[index].0.to_string();
                    self.bind_tuple_pattern(inner, element, &inner_tuple, func);
                }
                _ => {}
            }
        }
    }

    pub fn codegen_infix(
        &mut self,
        expr: ExpressionNode<'a>,
        func: &mut Function<'a>,
        _: bool,
    ) -> Value {
        if let Expression::Infix(val1, op, val2) = expr.expression {
            match op {
                Operator::Add => {
                    let val1 = self.codegen_expr(val1.as_ref().clone(), func, true);
                    let val2 = self.codegen_expr(val2.as_ref().clone(), func, true);

                    func.add(val1, val2)
                }
                Operator::Sub => {
                    let val1 = self.codegen_expr(val1.as_ref().clone(), func, true);
                    let val2 = self.codegen_expr(val2.as_ref().clone(), func, true);

                    func.sub(val1, val2)
                }
                Operator::Mul => {
                    let val1 = self.codegen_expr(val1.as_ref().clone(), func, true);
                    let val2 = self.codegen_expr(val2.as_ref().clone(), func, true);

                    func.mul(val1, val2)
                }
                Operator::Div => {
                    let val1 = self.codegen_expr(val1.as_ref().clone(), func, true);
                    let val2 = self.codegen_expr(val2.as_ref().clone(), func, true);

                    func.div(val1, val2)
                }
                Operator::Mod => {
                    let val1 = self.codegen_expr(val1.as_ref().clone(), func, true);
                    let val2 = self.codegen_expr(val2.as_ref().clone(), func, true);

                    func.modulus(val1, val2)
                }
                Operator::Power => {
                    let val1 = self.codegen_expr(val1.as_ref().clone(), func, true);
                    let val2 = self.codegen_expr(val2.as_ref().clone(), func, true);

                    func.pow(val1, val2)
                }
                Operator::Or => {
                    // Short-circuit: if LHS is true, skip RHS
                    let result_name = format!("__sc_{}", func.if_block_index);
                    let rhs_block = format!("sc_rhs_{}", func.if_block_index);
                    let end_block = format!("sc_end_{}", func.if_block_index);
                    func.if_block_index += 1;

                    func.variables.insert(result_name.clone(), AzulaType::Bool);
                    let t = func.const_true();
                    func.store(result_name.clone(), t, AzulaType::Bool);

                    let lhs = self.codegen_expr(val1.as_ref().clone(), func, true);
                    func.jcond(lhs, end_block.clone(), rhs_block.clone());

                    func.blocks.push((rhs_block.clone(), Block::new()));
                    func.current_block = rhs_block.clone();
                    let rhs = self.codegen_expr(val2.as_ref().clone(), func, true);
                    func.store(result_name.clone(), rhs, AzulaType::Bool);
                    func.jump(end_block.clone());

                    func.blocks.push((end_block.clone(), Block::new()));
                    func.current_block = end_block.clone();
                    func.load(result_name, AzulaType::Bool)
                }
                Operator::And => {
                    // Short-circuit: if LHS is false, skip RHS
                    let result_name = format!("__sc_{}", func.if_block_index);
                    let rhs_block = format!("sc_rhs_{}", func.if_block_index);
                    let end_block = format!("sc_end_{}", func.if_block_index);
                    func.if_block_index += 1;

                    func.variables.insert(result_name.clone(), AzulaType::Bool);
                    let f = func.const_false();
                    func.store(result_name.clone(), f, AzulaType::Bool);

                    let lhs = self.codegen_expr(val1.as_ref().clone(), func, true);
                    func.jcond(lhs, rhs_block.clone(), end_block.clone());

                    func.blocks.push((rhs_block.clone(), Block::new()));
                    func.current_block = rhs_block.clone();
                    let rhs = self.codegen_expr(val2.as_ref().clone(), func, true);
                    func.store(result_name.clone(), rhs, AzulaType::Bool);
                    func.jump(end_block.clone());

                    func.blocks.push((end_block.clone(), Block::new()));
                    func.current_block = end_block.clone();
                    func.load(result_name, AzulaType::Bool)
                }
                Operator::BitAnd | Operator::BitOr | Operator::BitXor | Operator::Shl | Operator::Shr => {
                    let val1 = self.codegen_expr(val1.as_ref().clone(), func, true);
                    let val2 = self.codegen_expr(val2.as_ref().clone(), func, true);
                    func.emit(|dest| match op {
                        Operator::BitAnd => Instruction::BitAnd(val1, val2, dest),
                        Operator::BitOr => Instruction::BitOr(val1, val2, dest),
                        Operator::BitXor => Instruction::BitXor(val1, val2, dest),
                        Operator::Shl => Instruction::Shl(val1, val2, dest),
                        _ => Instruction::Shr(val1, val2, dest),
                    })
                }
                Operator::Eq => {
                    let val1 = self.codegen_expr(val1.as_ref().clone(), func, true);
                    let val2 = self.codegen_expr(val2.as_ref().clone(), func, true);

                    func.eq(val1, val2)
                }
                Operator::Neq => {
                    let val1 = self.codegen_expr(val1.as_ref().clone(), func, true);
                    let val2 = self.codegen_expr(val2.as_ref().clone(), func, true);

                    func.neq(val1, val2)
                }
                Operator::Lt => {
                    let val1 = self.codegen_expr(val1.as_ref().clone(), func, true);
                    let val2 = self.codegen_expr(val2.as_ref().clone(), func, true);

                    func.lt(val1, val2)
                }
                Operator::Lte => {
                    let val1 = self.codegen_expr(val1.as_ref().clone(), func, true);
                    let val2 = self.codegen_expr(val2.as_ref().clone(), func, true);

                    func.lte(val1, val2)
                }
                Operator::Gt => {
                    let val1 = self.codegen_expr(val1.as_ref().clone(), func, true);
                    let val2 = self.codegen_expr(val2.as_ref().clone(), func, true);

                    func.gt(val1, val2)
                }
                Operator::Gte => {
                    let val1 = self.codegen_expr(val1.as_ref().clone(), func, true);
                    let val2 = self.codegen_expr(val2.as_ref().clone(), func, true);

                    func.gte(val1, val2)
                }
            }
        } else {
            unreachable!()
        }
    }

    fn resolve_function(&self, func: ExpressionNode<'a>) -> String {
        match func.expression {
            Expression::Identifier(name) => name,
            Expression::NamespaceAccess(ns, func) => {
                let namespace = if let Expression::Identifier(s) = ns.deref().clone().expression {
                    s
                } else {
                    unreachable!()
                };

                let func = if let Expression::Identifier(func) = &func.expression {
                    func
                } else {
                    unreachable!()
                };

                format!("{}.{}", namespace, func)
            }
            Expression::StructAccess(left, right) => {
                let namespace = left.deref().clone().typed.to_string();

                let func = if let Expression::Identifier(func) = &right.expression {
                    func
                } else {
                    unreachable!()
                };

                format!("{}.{}", namespace, func)
            }
            _ => unreachable!("{:?}", func.expression),
        }
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    use azula_ast::prelude::Span;
    use azula_type::prelude::*;
    use std::rc::Rc;

    #[test]
    fn test_codegen_function() {
        let mut codegen = Codegen::new("test", Statement::Root(vec![]));

        codegen.codegen_function(Statement::Function {
            name: "test",
            args: vec![(AzulaType::Bool, "x")],
            returns: AzulaType::Int,
            body: Rc::new(Statement::Block(vec![])),
            span: Span { start: 0, end: 1 },
        });

        assert_eq!(codegen.module.functions.len(), 1);
        let function = codegen.module.functions.get("test").unwrap();
        let mut args = vec![];
        args.push(("x".to_string(), AzulaType::Bool));
        assert_eq!(function.arguments, args);
        assert_eq!(function.returns, AzulaType::Int);
    }

    #[test]
    fn test_codegen_consts() {
        let mut codegen = Codegen::new("test", Statement::Root(vec![]));

        // Integer
        let mut func = Function::new();
        codegen.codegen_expr(
            ExpressionNode {
                expression: Expression::Integer(5),
                typed: AzulaType::Int,
                span: Span { start: 0, end: 1 },
            },
            &mut func,
            true,
        );
        assert_eq!(
            func.blocks[0].1.instructions,
            vec![Instruction::ConstInt(5, 0)]
        );

        // True
        let mut func = Function::new();
        codegen.codegen_expr(
            ExpressionNode {
                expression: Expression::Boolean(true),
                typed: AzulaType::Int,
                span: Span { start: 0, end: 1 },
            },
            &mut func,
            true,
        );
        assert_eq!(
            func.blocks[0].1.instructions,
            vec![Instruction::ConstTrue(0)]
        );

        // False
        let mut func = Function::new();
        codegen.codegen_expr(
            ExpressionNode {
                expression: Expression::Boolean(false),
                typed: AzulaType::Int,
                span: Span { start: 0, end: 1 },
            },
            &mut func,
            true,
        );
        assert_eq!(
            func.blocks[0].1.instructions,
            vec![Instruction::ConstFalse(0)]
        );
    }

    #[test]
    fn test_codegen_if() {
        let mut codegen = Codegen::new("test", Statement::Root(vec![]));
        let mut func = Function::new();

        codegen.codegen_if(
            Statement::If(
                ExpressionNode {
                    expression: Expression::Boolean(true),
                    typed: AzulaType::Bool,
                    span: Span { start: 0, end: 0 },
                },
                vec![],
                None,
                Span { start: 0, end: 0 },
            ),
            &mut func,
        );

        assert_eq!(func.blocks.len(), 3);
    }

    #[test]
    fn test_codegen_infix() {
        let mut codegen = Codegen::new("test", Statement::Root(vec![]));

        // Addition
        let mut func = Function::new();
        codegen.codegen_expr(
            ExpressionNode {
                expression: Expression::Infix(
                    Rc::new(ExpressionNode {
                        expression: Expression::Integer(10),
                        typed: AzulaType::Int,
                        span: Span { start: 0, end: 1 },
                    }),
                    Operator::Add,
                    Rc::new(ExpressionNode {
                        expression: Expression::Integer(20),
                        typed: AzulaType::Int,
                        span: Span { start: 0, end: 1 },
                    }),
                ),
                typed: AzulaType::Int,
                span: Span { start: 0, end: 1 },
            },
            &mut func,
            true,
        );
        assert_eq!(
            func.blocks[0].1.instructions,
            vec![
                Instruction::ConstInt(10, 0),
                Instruction::ConstInt(20, 1),
                Instruction::Add(Value::Local(0), Value::Local(1), 2)
            ]
        );

        // Subtraction
        let mut func = Function::new();
        codegen.codegen_expr(
            ExpressionNode {
                expression: Expression::Infix(
                    Rc::new(ExpressionNode {
                        expression: Expression::Integer(10),
                        typed: AzulaType::Int,
                        span: Span { start: 0, end: 1 },
                    }),
                    Operator::Sub,
                    Rc::new(ExpressionNode {
                        expression: Expression::Integer(20),
                        typed: AzulaType::Int,
                        span: Span { start: 0, end: 1 },
                    }),
                ),
                typed: AzulaType::Int,
                span: Span { start: 0, end: 1 },
            },
            &mut func,
            true,
        );
        assert_eq!(
            func.blocks[0].1.instructions,
            vec![
                Instruction::ConstInt(10, 0),
                Instruction::ConstInt(20, 1),
                Instruction::Sub(Value::Local(0), Value::Local(1), 2)
            ]
        );

        // Multiplication
        let mut func = Function::new();
        codegen.codegen_expr(
            ExpressionNode {
                expression: Expression::Infix(
                    Rc::new(ExpressionNode {
                        expression: Expression::Integer(10),
                        typed: AzulaType::Int,
                        span: Span { start: 0, end: 1 },
                    }),
                    Operator::Mul,
                    Rc::new(ExpressionNode {
                        expression: Expression::Integer(20),
                        typed: AzulaType::Int,
                        span: Span { start: 0, end: 1 },
                    }),
                ),
                typed: AzulaType::Int,
                span: Span { start: 0, end: 1 },
            },
            &mut func,
            true,
        );
        assert_eq!(
            func.blocks[0].1.instructions,
            vec![
                Instruction::ConstInt(10, 0),
                Instruction::ConstInt(20, 1),
                Instruction::Mul(Value::Local(0), Value::Local(1), 2)
            ]
        );

        // Divide
        let mut func = Function::new();
        codegen.codegen_expr(
            ExpressionNode {
                expression: Expression::Infix(
                    Rc::new(ExpressionNode {
                        expression: Expression::Integer(10),
                        typed: AzulaType::Int,
                        span: Span { start: 0, end: 1 },
                    }),
                    Operator::Div,
                    Rc::new(ExpressionNode {
                        expression: Expression::Integer(20),
                        typed: AzulaType::Int,
                        span: Span { start: 0, end: 1 },
                    }),
                ),
                typed: AzulaType::Int,
                span: Span { start: 0, end: 1 },
            },
            &mut func,
            true,
        );
        assert_eq!(
            func.blocks[0].1.instructions,
            vec![
                Instruction::ConstInt(10, 0),
                Instruction::ConstInt(20, 1),
                Instruction::Div(Value::Local(0), Value::Local(1), 2)
            ]
        );

        // Modulus
        let mut func = Function::new();
        codegen.codegen_expr(
            ExpressionNode {
                expression: Expression::Infix(
                    Rc::new(ExpressionNode {
                        expression: Expression::Integer(10),
                        typed: AzulaType::Int,
                        span: Span { start: 0, end: 1 },
                    }),
                    Operator::Mod,
                    Rc::new(ExpressionNode {
                        expression: Expression::Integer(20),
                        typed: AzulaType::Int,
                        span: Span { start: 0, end: 1 },
                    }),
                ),
                typed: AzulaType::Int,
                span: Span { start: 0, end: 1 },
            },
            &mut func,
            true,
        );
        assert_eq!(
            func.blocks[0].1.instructions,
            vec![
                Instruction::ConstInt(10, 0),
                Instruction::ConstInt(20, 1),
                Instruction::Mod(Value::Local(0), Value::Local(1), 2)
            ]
        );
    }

    #[test]
    fn test_codegen_enum_registration() {
        let root = Statement::Root(vec![Statement::Enum {
            name: "Color",
            variants: vec!["Red", "Green", "Blue"],
            payloads: vec![vec![], vec![], vec![]],
            span: Span { start: 0, end: 1 },
        }]);
        let mut codegen = Codegen::new("test", root);
        codegen.codegen();

        assert!(codegen.module.enums.contains_key("Color"));
        assert_eq!(
            codegen.module.enums["Color"],
            vec!["Red".to_string(), "Green".to_string(), "Blue".to_string()]
        );
    }

    #[test]
    fn test_codegen_enum_variant_is_const_int() {
        let mut codegen = Codegen::new("test", Statement::Root(vec![]));
        codegen
            .module
            .add_enum("Color".to_string(), vec!["Red".to_string(), "Green".to_string(), "Blue".to_string()]);

        let mut func = Function::new();
        codegen.codegen_expr(
            ExpressionNode {
                expression: Expression::NamespaceAccess(
                    Rc::new(ExpressionNode {
                        expression: Expression::Identifier("Color".to_string()),
                        typed: AzulaType::Infer,
                        span: Span { start: 0, end: 5 },
                    }),
                    Rc::new(ExpressionNode {
                        expression: Expression::Identifier("Green".to_string()),
                        typed: AzulaType::Infer,
                        span: Span { start: 7, end: 12 },
                    }),
                ),
                typed: AzulaType::Named("Color".to_string()),
                span: Span { start: 0, end: 12 },
            },
            &mut func,
            true,
        );
        // Green is index 1
        assert_eq!(func.blocks[0].1.instructions, vec![Instruction::ConstInt(1, 0)]);
    }

    #[test]
    fn test_codegen_match_block_count() {
        let mut codegen = Codegen::new("test", Statement::Root(vec![]));
        codegen
            .module
            .add_enum("Color".to_string(), vec!["Red".to_string(), "Green".to_string()]);

        let mut func = Function::new();
        // Manually add scrutinee as a stored variable
        let scrut = func.const_int(0);
        func.variables.insert("__scrut".to_string(), AzulaType::Named("Color".to_string()));
        func.store("__scrut".to_string(), scrut, AzulaType::Named("Color".to_string()));
        let scrut_loaded = func.load("__scrut".to_string(), AzulaType::Named("Color".to_string()));

        let arms = vec![
            (
                MatchPattern::Variant("Color", "Red"),
                ExpressionNode {
                    expression: Expression::Integer(10),
                    typed: AzulaType::Int,
                    span: Span { start: 0, end: 1 },
                },
            ),
            (
                MatchPattern::Variant("Color", "Green"),
                ExpressionNode {
                    expression: Expression::Integer(20),
                    typed: AzulaType::Int,
                    span: Span { start: 0, end: 1 },
                },
            ),
        ];

        codegen.codegen_match(
            Rc::new(ExpressionNode {
                expression: Expression::Identifier("__scrut".to_string()),
                typed: AzulaType::Named("Color".to_string()),
                span: Span { start: 0, end: 1 },
            }),
            arms,
            AzulaType::Int,
            &mut func,
        );

        // entry + match-arm-0-0 + match-next-0-1 + match-arm-0-1 + match-end-0 = 5 blocks
        assert_eq!(func.blocks.len(), 5);
        assert_eq!(func.match_block_index, 1);
    }
}
