use std::collections::HashMap;
use std::error::Error;
use std::ops::Deref;
use std::path::Path;
use std::process::Command;

use azula_codegen::prelude::Backend;
use azula_codegen::prelude::OptimizationLevel;
use azula_ir::prelude::{GlobalValue, Instruction, Module, Value};
use azula_type::prelude::AzulaType;
use inkwell::basic_block::BasicBlock;
use inkwell::module::{Linkage, Module as LLVMModule};
use inkwell::targets::{FileType, InitializationConfig, Target, TargetMachine, TargetTriple};
use inkwell::types::StructType;
use inkwell::types::{BasicMetadataTypeEnum, BasicType, BasicTypeEnum, FunctionType};
use inkwell::values::{BasicMetadataValueEnum, BasicValue, BasicValueEnum, FunctionValue};
use inkwell::{builder::Builder, context::Context};
use inkwell::{AddressSpace, FloatPredicate, IntPredicate};

pub struct LLVMCodegen<'ctx> {
    context: &'ctx Context,
    module: LLVMModule<'ctx>,
    builder: Builder<'ctx>,

    strings: HashMap<usize, BasicValueEnum<'ctx>>,
    string_size: HashMap<usize, usize>,
    globals: HashMap<String, BasicValueEnum<'ctx>>,
    structs: HashMap<String, StructType<'ctx>>,
    enum_names: std::collections::HashSet<String>,
    boxed_enum_names: std::collections::HashSet<String>,

    target: Option<String>,
    opt_level: OptimizationLevel,
}

struct FunctionLocals<'a> {
    registers: HashMap<usize, BasicValueEnum<'a>>,
    variables: HashMap<String, BasicValueEnum<'a>>,

    blocks: HashMap<String, BasicBlock<'a>>,
}

impl<'ctx> Backend<'ctx> for LLVMCodegen<'ctx> {
    fn codegen(
        name: &'ctx str,
        destination: &'ctx str,
        emit: bool,
        target: Option<&String>,
        opt_level: OptimizationLevel,
        module: Module<'ctx>,
    ) -> Result<(), Box<dyn Error>> {
        let context = Context::create();
        let llvm_module = context.create_module(module.name);
        let target = if let Some(val) = target {
            Some(val.clone())
        } else {
            None
        };
        let enum_names: std::collections::HashSet<String> =
            module.enums.keys().cloned().collect();
        let mut codegen = LLVMCodegen {
            context: &context,
            module: llvm_module,
            builder: context.create_builder(),
            strings: HashMap::new(),
            string_size: HashMap::new(),
            globals: HashMap::new(),
            structs: HashMap::new(),
            enum_names,
            boxed_enum_names: module.boxed_enums.clone(),
            target,
            opt_level,
        };

        codegen.generate_structs(&module);

        for (name, extern_func) in &module.extern_functions {
            let args: Vec<_> = extern_func
                .arguments
                .iter()
                .map(|arg| codegen.azula_type_to_llvm_basic_type(arg.clone()).into())
                .collect();
            codegen.module.add_function(
                name,
                codegen.azula_type_to_function_llvm_type_with_varargs(
                    extern_func.returns.clone(),
                    &args,
                    extern_func.varargs,
                ),
                Some(Linkage::External),
            );
        }

        codegen.module.add_function(
            "pow",
            codegen.context.f64_type().fn_type(
                &[
                    codegen.context.f64_type().as_basic_type_enum().into(),
                    codegen.context.f64_type().as_basic_type_enum().into(),
                ],
                false,
            ),
            Some(Linkage::External),
        );
        let mut i = 0;

        for (name, func) in &module.functions {
            let mut linkage = Some(Linkage::Private);
            let mut returns = func.returns.clone();
            if *name == "main" {
                linkage = None;
                // A void `main` still has to give the OS an exit code.
                if returns == AzulaType::Void {
                    returns = AzulaType::SizedSignedInt(32);
                }
            }
            codegen.module.add_function(
                name,
                codegen.azula_type_to_function_llvm_type(
                    returns,
                    &func
                        .arguments
                        .iter()
                        .map(|(_, typ)| codegen.azula_type_to_llvm_basic_type(typ.clone()).into())
                        .collect::<Vec<_>>(),
                ),
                linkage,
            );
        }

        for (name, func) in &module.functions {
            let mut locals = FunctionLocals::new();
            let function = codegen.module.get_function(name).unwrap();

            // Pre-allocate all local variables in the entry block so that
            // match/if result slots (first stored in non-entry blocks) have
            // valid allocas regardless of which branch is taken at runtime.
            let entry_bb = codegen.context.append_basic_block(function, "entry");
            locals.blocks.insert("entry".to_string(), entry_bb);
            codegen.builder.position_at_end(entry_bb);
            for (var_name, var_type) in &func.variables {
                let alloca = codegen
                    .builder
                    .build_alloca(
                        codegen.azula_type_to_llvm_basic_type(var_type.clone()),
                        "alloca",
                    )
                    .unwrap();
                locals.variables.insert(var_name.clone(), alloca.as_basic_value_enum());
            }

            for (name, block) in &func.blocks {
                let basic = if locals.blocks.contains_key(name) {
                    *locals.blocks.get(name).unwrap()
                } else {
                    codegen.context.append_basic_block(function, &name)
                };
                codegen.builder.position_at_end(basic);
                if i == 0 {
                    codegen.store_globals(&module);
                    i += 1;
                }
                for instruction in &block.instructions {
                    codegen.codegen_instruction(instruction.clone(), &function, &mut locals);
                }
            }
        }

        if emit {
            codegen
                .module
                .print_to_file(format!("{}.ll", name))
                .unwrap();
        }

        if let Err(e) = codegen.module.verify() {
            return Err(format!("internal compiler error: invalid LLVM IR generated:\n{}", e.to_string()).into());
        }

        std::fs::create_dir_all(".build")?;
        let base_name = Path::new(name).file_name().unwrap().to_string_lossy();
        let object_file = format!(".build/{}.o", base_name);
        codegen.build_object_file(object_file.clone());

        // Prefer `zig cc` (which makes cross-compiling easy), falling back to the system C compiler.
        let use_zig = Command::new("zig").arg("version").output().is_ok();
        let mut command = if use_zig {
            let mut c = Command::new("zig");
            c.arg("cc");
            c
        } else {
            Command::new("cc")
        };
        command
            .arg("-o")
            .arg(format!("{}{}", destination, name))
            .arg(object_file)
            .arg("-lm");
        if let Some(target) = codegen.target {
            command.arg("-target").arg(target);
        }
        let status = command.status()?;
        if !status.success() {
            return Err("linking failed".into());
        }

        Ok(())
    }
}

impl<'a> LLVMCodegen<'a> {
    fn store_globals(&mut self, module: &Module<'a>) {
        for (i, str) in module.strings.clone().into_iter().enumerate() {
            let ptr = self
                .builder
                .build_global_string_ptr(str.as_str(), "string")
                .unwrap()
                .as_basic_value_enum();

            self.strings.insert(i, ptr);
            self.string_size.insert(i, str.len());
        }

        for (name, val) in &module.global_values {
            let ptr = match val {
                GlobalValue::Int(i) => {
                    let val = self.module.add_global(
                        self.context.i64_type(),
                        Some(AddressSpace::default()),
                        &name,
                    );

                    val.set_initializer(
                        &self
                            .context
                            .i64_type()
                            .const_int(*i as u64, true),
                    );

                    val.as_basic_value_enum()
                }
                GlobalValue::Float(f) => {
                    let val = self.module.add_global(
                        self.context.f64_type(),
                        Some(AddressSpace::default()),
                        &name,
                    );

                    val.set_initializer(
                        &self
                            .context
                            .f64_type()
                            .const_float((*f).try_into().unwrap()),
                    );

                    val.as_basic_value_enum()
                }
                GlobalValue::Bool(b) => {
                    let val = self.module.add_global(
                        self.context.bool_type(),
                        Some(AddressSpace::default()),
                        &name,
                    );

                    val.set_initializer(
                        &self
                            .context
                            .bool_type()
                            .const_int((*b).try_into().unwrap(), false),
                    );

                    val.as_basic_value_enum()
                }
                GlobalValue::String(s) => *self.strings.get(&s).unwrap(),
                GlobalValue::Array(_) => todo!(),
            };

            self.globals.insert(name.clone(), ptr);
        }
    }

    fn generate_structs(&mut self, module: &Module<'a>) {
        for (i, _) in &module.structs {
            let struc = self.context.opaque_struct_type(i);
            self.structs.insert(i.to_string(), struc);
        }

        for (i, str) in &module.structs {
            let args: Vec<_> = str
                .attributes
                .iter()
                .map(|(arg, _)| self.azula_type_to_llvm_basic_type(arg.clone()))
                .collect();

            self.structs
                .get(&i.to_string())
                .unwrap()
                .set_body(&args, false);
        }
    }

    fn codegen_instruction(
        &self,
        instruction: Instruction<'a>,
        func: &FunctionValue<'a>,
        locals: &mut FunctionLocals<'a>,
    ) {
        match instruction {
            Instruction::Load(name, dest, typ) => {
                let alloca = locals.variables.get(&name).unwrap();
                let element_type = self.azula_type_to_llvm_basic_type(typ);
                let value = self
                    .builder
                    .build_load(element_type, alloca.into_pointer_value(), "load")
                    .unwrap();

                locals.store(dest, value);
            }
            Instruction::LoadGlobal(name, dest, typ) => {
                let alloca = self.globals.get(&name).unwrap();
                let element_type = self.azula_type_to_llvm_basic_type(typ);
                let value = self
                    .builder
                    .build_load(element_type, alloca.into_pointer_value(), "load")
                    .unwrap();

                locals.store(dest, value);
            }
            Instruction::LoadArg(arg, dest, _) => {
                locals.store(dest, func.get_params()[arg]);
            }
            Instruction::Store(name, val, typ) => {
                let value = match val {
                    Value::Local(val) => locals.load(val),
                    Value::LiteralInteger(n) => self
                        .context
                        .i64_type()
                        .const_int(n as u64, false)
                        .as_basic_value_enum(),
                    Value::LiteralBoolean(b) => self
                        .context
                        .bool_type()
                        .const_int(b as u64, false)
                        .as_basic_value_enum(),
                    Value::Global(y) => {
                        let alloca = if locals.variables.contains_key(&name) {
                            locals.variables.get(&name).unwrap().into_pointer_value()
                        } else {
                            let alloca = self
                                .builder
                                .build_alloca(self.azula_type_to_llvm_basic_type(typ), "alloca")
                                .unwrap();
                            locals.variables.insert(name, alloca.as_basic_value_enum());

                            alloca
                        };
                        self.builder
                            .build_store(alloca, *self.strings.get(&y).unwrap())
                            .unwrap();
                        return;
                    }
                };

                let target_llvm = self.azula_type_to_llvm_basic_type(typ.clone());
                let value = self.coerce_int_width(value, target_llvm);

                let alloca = if locals.variables.contains_key(&name) {
                    locals.variables.get(&name).unwrap().into_pointer_value()
                } else {
                    let alloca = self
                        .builder
                        .build_alloca(target_llvm, "alloca")
                        .unwrap();
                    locals.variables.insert(name, alloca.as_basic_value_enum());

                    alloca
                };
                self.builder.build_store(alloca, value).unwrap();
            }
            Instruction::ConstInt(val, dest) => {
                locals.registers.insert(
                    dest,
                    self.context
                        .i64_type()
                        .const_int(val as u64, false)
                        .as_basic_value_enum(),
                );
            }
            Instruction::ConstFloat(val, dest) => {
                locals.registers.insert(
                    dest,
                    self.context
                        .f64_type()
                        .const_float(val)
                        .as_basic_value_enum(),
                );
            }
            Instruction::ConstTrue(dest) => {
                locals.registers.insert(
                    dest,
                    self.context
                        .bool_type()
                        .const_int(1 as u64, false)
                        .as_basic_value_enum(),
                );
            }
            Instruction::ConstFalse(dest) => {
                locals.registers.insert(
                    dest,
                    self.context
                        .bool_type()
                        .const_int(0 as u64, false)
                        .as_basic_value_enum(),
                );
            }
            Instruction::ConstNull(dest) => {
                locals.registers.insert(
                    dest,
                    self.context
                        .ptr_type(AddressSpace::default())
                        .const_null()
                        .as_basic_value_enum(),
                );
            }
            Instruction::Add(..) => self.codegen_add(instruction, locals),
            Instruction::Sub(..) => self.codegen_sub(instruction, locals),
            Instruction::Mul(..) => self.codegen_mul(instruction, locals),
            Instruction::Div(..) => self.codegen_div(instruction, locals),
            Instruction::Mod(..) => self.codegen_mod(instruction, locals),
            Instruction::Pow(..) => self.codegen_pow(instruction, locals),
            Instruction::Return(val) => {
                let ret_basic_type: Option<BasicTypeEnum<'a>> = func.get_type().get_return_type();
                match val {
                    None => match ret_basic_type {
                        Some(BasicTypeEnum::IntType(t)) => {
                            self.builder.build_return(Some(&t.const_zero())).unwrap();
                        }
                        _ => {
                            self.builder.build_return(None).unwrap();
                        }
                    },
                    Some(Value::Local(val)) => {
                        let mut ret_val = *locals.registers.get(&val).clone().unwrap();
                        if let Some(target) = ret_basic_type {
                            ret_val = self.coerce_int_width(ret_val, target);
                        }
                        self.builder.build_return(Some(&ret_val)).unwrap();
                    }
                    Some(Value::Global(val)) => {
                        self.builder
                            .build_return(Some(self.strings.get(&val).clone().unwrap()))
                            .unwrap();
                    }
                    _ => unreachable!(),
                }
            },
            Instruction::Unreachable => {
                self.builder.build_unreachable().unwrap();
            }
            Instruction::FunctionCall(name, args, dest) => {
                // compiler intrinsics lowered to LLVM directly
                if name == "ptr_add" {
                    let ptr = locals.load(value_to_local(args[0].clone())).into_pointer_value();
                    let offset = locals.load(value_to_local(args[1].clone())).into_int_value();
                    let gep = unsafe {
                        self.builder
                            .build_gep(self.context.i8_type(), ptr, &[offset], "ptr_add")
                            .unwrap()
                    };
                    locals.store(dest, gep.as_basic_value_enum());
                    return;
                }
                if name == "ptr_read_int" {
                    let ptr = locals.load(value_to_local(args[0].clone())).into_pointer_value();
                    let val = self.builder.build_load(self.context.i64_type(), ptr, "ptr_read_int").unwrap();
                    locals.store(dest, val);
                    return;
                }
                if name == "ptr_write_int" {
                    let ptr = locals.load(value_to_local(args[0].clone())).into_pointer_value();
                    let val = locals.load(value_to_local(args[1].clone())).into_int_value();
                    self.builder.build_store(ptr, val).unwrap();
                    return;
                }
                if name == "ptr_read_str" {
                    let ptr = locals.load(value_to_local(args[0].clone())).into_pointer_value();
                    let ptr_type = self.context.ptr_type(inkwell::AddressSpace::default());
                    let val = self.builder.build_load(ptr_type, ptr, "ptr_read_str").unwrap();
                    locals.store(dest, val);
                    return;
                }
                if name == "ptr_write_str" {
                    let ptr = locals.load(value_to_local(args[0].clone())).into_pointer_value();
                    let val = locals.load(value_to_local(args[1].clone())).into_pointer_value();
                    self.builder.build_store(ptr, val).unwrap();
                    return;
                }

                let converted_args: Vec<BasicMetadataValueEnum> = args
                    .iter()
                    .map(|arg| match arg {
                        Value::Global(i) => {
                            (*self.strings.get(i).unwrap()).into()
                        }
                        Value::Local(..) => locals.load(value_to_local(arg.clone())).into(),
                        Value::LiteralInteger(n) => self
                            .context
                            .i64_type()
                            .const_int(*n as u64, false)
                            .as_basic_value_enum()
                            .into(),
                        Value::LiteralBoolean(b) => self
                            .context
                            .bool_type()
                            .const_int(*b as u64, false)
                            .as_basic_value_enum()
                            .into(),
                    })
                    .collect();

                let callee = self
                    .module
                    .get_function(&name)
                    .unwrap_or_else(|| panic!("Unknown function {}", name));
                let param_types = callee.get_type().get_param_types();
                let converted_args: Vec<BasicMetadataValueEnum> = converted_args
                    .into_iter()
                    .enumerate()
                    .map(|(i, arg)| match (BasicValueEnum::try_from(arg), param_types.get(i)) {
                        (Ok(value), Some(param)) => match BasicTypeEnum::try_from(*param) {
                            Ok(param) => self.coerce_int_width(value, param).into(),
                            Err(_) => arg,
                        },
                        // C default argument promotions for variadic arguments
                        (Ok(BasicValueEnum::IntValue(iv)), None) if iv.get_type().get_bit_width() < 32 => {
                            let i32t = self.context.i32_type();
                            if iv.get_type().get_bit_width() == 1 {
                                self.builder.build_int_z_extend(iv, i32t, "promote").unwrap().into()
                            } else {
                                self.builder.build_int_s_extend(iv, i32t, "promote").unwrap().into()
                            }
                        }
                        (Ok(BasicValueEnum::FloatValue(fv)), None)
                            if fv.get_type() != self.context.f64_type() =>
                        {
                            self.builder
                                .build_float_ext(fv, self.context.f64_type(), "promote")
                                .unwrap()
                                .into()
                        }
                        _ => arg,
                    })
                    .collect();

                let result = self
                    .builder
                    .build_call(callee, &converted_args, "call")
                    .unwrap();

                let result = result.try_as_basic_value();

                if result.is_basic() {
                    locals.store(dest, result.unwrap_basic().as_basic_value_enum());
                }
            }
            Instruction::Jcond(cond, true_block_name, end_block_name) => {
                let local = locals.load(value_to_local(cond)).into_int_value();

                let true_block = if let Some(b) = locals.blocks.get(&true_block_name) {
                    *b
                } else {
                    let b = self.context.append_basic_block(*func, true_block_name.as_str());
                    locals.blocks.insert(true_block_name.clone(), b);
                    b
                };
                let end_block = if let Some(b) = locals.blocks.get(&end_block_name) {
                    *b
                } else {
                    let b = self.context.append_basic_block(*func, end_block_name.as_str());
                    locals.blocks.insert(end_block_name.clone(), b);
                    b
                };

                self.builder
                    .build_conditional_branch(local, true_block, end_block)
                    .unwrap();
            }
            Instruction::Or(val1, val2, dest) => {
                let local1 = locals.load(value_to_local(val1)).into_int_value();
                let local2 = locals.load(value_to_local(val2)).into_int_value();
                let value = self.builder.build_or(local1, local2, "or").unwrap();

                locals.store(dest, value.as_basic_value_enum());
            }
            Instruction::And(val1, val2, dest) => {
                let local1 = locals.load(value_to_local(val1)).into_int_value();
                let local2 = locals.load(value_to_local(val2)).into_int_value();
                let value = self.builder.build_and(local1, local2, "and").unwrap();

                locals.store(dest, value.as_basic_value_enum());
            }
            Instruction::Eq(val1, val2, dest) => {
                let local1 = locals.load(value_to_local(val1));
                let local2 = locals.load(value_to_local(val2));
            let (local1, local2) = self.harmonize(local1, local2);

                let value = match local1.get_type() {
                    BasicTypeEnum::FloatType(_) => self
                        .builder
                        .build_float_compare(
                            FloatPredicate::OEQ,
                            local1.into_float_value(),
                            local2.into_float_value(),
                            "eq",
                        )
                        .unwrap()
                        .as_basic_value_enum(),
                    BasicTypeEnum::IntType(_) => self
                        .builder
                        .build_int_compare(
                            IntPredicate::EQ,
                            local1.into_int_value(),
                            local2.into_int_value(),
                            "eq",
                        )
                        .unwrap()
                        .as_basic_value_enum(),
                    BasicTypeEnum::PointerType(_) => {
                        let i64t = self.context.i64_type();
                        let l = self.builder.build_ptr_to_int(local1.into_pointer_value(), i64t, "ptoi").unwrap();
                        let r = self.builder.build_ptr_to_int(local2.into_pointer_value(), i64t, "ptoi").unwrap();
                        self.builder.build_int_compare(IntPredicate::EQ, l, r, "eq").unwrap().as_basic_value_enum()
                    }
                    _ => unreachable!(),
                };

                locals.store(dest, value.as_basic_value_enum());
            }
            Instruction::Neq(val1, val2, dest) => {
                let local1 = locals.load(value_to_local(val1));
                let local2 = locals.load(value_to_local(val2));
            let (local1, local2) = self.harmonize(local1, local2);

                let value = match local1.get_type() {
                    BasicTypeEnum::FloatType(_) => self
                        .builder
                        .build_float_compare(
                            FloatPredicate::ONE,
                            local1.into_float_value(),
                            local2.into_float_value(),
                            "neq",
                        )
                        .unwrap()
                        .as_basic_value_enum(),
                    BasicTypeEnum::IntType(_) => self
                        .builder
                        .build_int_compare(
                            IntPredicate::NE,
                            local1.into_int_value(),
                            local2.into_int_value(),
                            "neq",
                        )
                        .unwrap()
                        .as_basic_value_enum(),
                    BasicTypeEnum::PointerType(_) => {
                        let i64t = self.context.i64_type();
                        let l = self.builder.build_ptr_to_int(local1.into_pointer_value(), i64t, "ptoi").unwrap();
                        let r = self.builder.build_ptr_to_int(local2.into_pointer_value(), i64t, "ptoi").unwrap();
                        self.builder.build_int_compare(IntPredicate::NE, l, r, "neq").unwrap().as_basic_value_enum()
                    }
                    _ => unreachable!(),
                };

                locals.store(dest, value.as_basic_value_enum());
            }
            Instruction::Jump(val) => {
                if let Some(block) = locals.blocks.get(&val) {
                    self.builder.build_unconditional_branch(*block).unwrap();
                } else {
                    let jump_block =
                        self.context.append_basic_block(*func, val.clone().as_str());
                    locals.blocks.insert(val.clone(), jump_block);
                    self.builder
                        .build_unconditional_branch(jump_block)
                        .unwrap();

                    self.builder.position_at_end(jump_block);
                }
            }
            Instruction::Gt(val1, val2, dest) => {
                let local1 = locals.load(value_to_local(val1));
                let local2 = locals.load(value_to_local(val2));
            let (local1, local2) = self.harmonize(local1, local2);

                let value = match local1.get_type() {
                    BasicTypeEnum::FloatType(_) => self
                        .builder
                        .build_float_compare(
                            FloatPredicate::OGT,
                            local1.into_float_value(),
                            local2.into_float_value(),
                            "gt",
                        )
                        .unwrap()
                        .as_basic_value_enum(),
                    BasicTypeEnum::IntType(_) => self
                        .builder
                        .build_int_compare(
                            IntPredicate::SGT,
                            local1.into_int_value(),
                            local2.into_int_value(),
                            "gt",
                        )
                        .unwrap()
                        .as_basic_value_enum(),
                    _ => unreachable!(),
                };

                locals.store(dest, value.as_basic_value_enum());
            }
            Instruction::Gte(val1, val2, dest) => {
                let local1 = locals.load(value_to_local(val1));
                let local2 = locals.load(value_to_local(val2));
            let (local1, local2) = self.harmonize(local1, local2);

                let value = match local1.get_type() {
                    BasicTypeEnum::FloatType(_) => self
                        .builder
                        .build_float_compare(
                            FloatPredicate::OGE,
                            local1.into_float_value(),
                            local2.into_float_value(),
                            "gte",
                        )
                        .unwrap()
                        .as_basic_value_enum(),
                    BasicTypeEnum::IntType(_) => self
                        .builder
                        .build_int_compare(
                            IntPredicate::SGE,
                            local1.into_int_value(),
                            local2.into_int_value(),
                            "gte",
                        )
                        .unwrap()
                        .as_basic_value_enum(),
                    _ => unreachable!(),
                };

                locals.store(dest, value.as_basic_value_enum());
            }
            Instruction::Lt(val1, val2, dest) => {
                let local1 = locals.load(value_to_local(val1));
                let local2 = locals.load(value_to_local(val2));
            let (local1, local2) = self.harmonize(local1, local2);

                let value = match local1.get_type() {
                    BasicTypeEnum::FloatType(_) => self
                        .builder
                        .build_float_compare(
                            FloatPredicate::OLT,
                            local1.into_float_value(),
                            local2.into_float_value(),
                            "lt",
                        )
                        .unwrap()
                        .as_basic_value_enum(),
                    BasicTypeEnum::IntType(_) => self
                        .builder
                        .build_int_compare(
                            IntPredicate::SLT,
                            local1.into_int_value(),
                            local2.into_int_value(),
                            "lt",
                        )
                        .unwrap()
                        .as_basic_value_enum(),
                    _ => unreachable!("{:?}", local1),
                };

                locals.store(dest, value.as_basic_value_enum());
            }
            Instruction::Lte(val1, val2, dest) => {
                let local1 = locals.load(value_to_local(val1));
                let local2 = locals.load(value_to_local(val2));
            let (local1, local2) = self.harmonize(local1, local2);

                let value = match local1.get_type() {
                    BasicTypeEnum::FloatType(_) => self
                        .builder
                        .build_float_compare(
                            FloatPredicate::OLE,
                            local1.into_float_value(),
                            local2.into_float_value(),
                            "lte",
                        )
                        .unwrap()
                        .as_basic_value_enum(),
                    BasicTypeEnum::IntType(_) => self
                        .builder
                        .build_int_compare(
                            IntPredicate::SLE,
                            local1.into_int_value(),
                            local2.into_int_value(),
                            "lte",
                        )
                        .unwrap()
                        .as_basic_value_enum(),
                    _ => unreachable!(),
                };

                locals.store(dest, value.as_basic_value_enum());
            }
            Instruction::BitAnd(ref val1, ref val2, dest)
            | Instruction::BitOr(ref val1, ref val2, dest)
            | Instruction::BitXor(ref val1, ref val2, dest)
            | Instruction::Shl(ref val1, ref val2, dest)
            | Instruction::Shr(ref val1, ref val2, dest) => {
                let local1 = locals.load(value_to_local(val1.clone()));
                let local2 = locals.load(value_to_local(val2.clone()));
                let (local1, local2) = self.harmonize(local1, local2);
                let (l, r) = (local1.into_int_value(), local2.into_int_value());
                let value = match instruction {
                    Instruction::BitAnd(..) => self.builder.build_and(l, r, "and"),
                    Instruction::BitOr(..) => self.builder.build_or(l, r, "or"),
                    Instruction::BitXor(..) => self.builder.build_xor(l, r, "xor"),
                    Instruction::Shl(..) => self.builder.build_left_shift(l, r, "shl"),
                    _ => self.builder.build_right_shift(l, r, true, "shr"),
                }
                .unwrap();
                locals.store(dest, value.as_basic_value_enum());
            }
            Instruction::SizeOf(typ, dest) => {
                let size = self.azula_type_to_llvm_basic_type(typ).size_of().unwrap();
                locals.store(dest, size.as_basic_value_enum());
            }
            Instruction::ElementPtr(array, index, dest, elem_type) => {
                let array_ptr = locals.load(value_to_local(array)).into_pointer_value();
                let index_val = locals.load(value_to_local(index)).into_int_value();
                let elem_llvm_type = self.azula_type_to_llvm_basic_type(elem_type);
                let gep = unsafe {
                    self.builder
                        .build_gep(elem_llvm_type, array_ptr, &[index_val], "element_ptr")
                        .unwrap()
                };
                locals.store(dest, gep.as_basic_value_enum());
            }
            Instruction::Not(val, dest) => {
                let local = locals.load(value_to_local(val)).into_int_value();
                let value = self.builder.build_not(local, "not").unwrap();

                locals.store(dest, value.as_basic_value_enum());
            }
            Instruction::Pointer(val, dest) => {
                let alloca = locals.variables.get(&val).unwrap().clone();

                locals.store(dest, alloca.as_basic_value_enum());
            }
            Instruction::CreateArray(typ, size, dest) => {
                let array = self
                    .builder
                    .build_array_malloc(
                        self.azula_type_to_llvm_basic_type(typ.clone()),
                        self.context.i32_type().const_int(size as u64, false),
                        "array",
                    )
                    .unwrap();

                locals.store(dest, array.as_basic_value_enum());
            }
            Instruction::StoreElement(array, index, value, elem_type) => {
                let array_ptr = locals.load(value_to_local(array)).into_pointer_value();
                let index_val = locals.load(value_to_local(index)).into_int_value();

                let val = match value {
                    Value::Local(..) => locals.load(value_to_local(value)),
                    Value::Global(pos) => *self.strings.get(&pos).unwrap(),
                    Value::LiteralInteger(n) => self
                        .context
                        .i64_type()
                        .const_int(n as u64, false)
                        .as_basic_value_enum(),
                    Value::LiteralBoolean(b) => self
                        .context
                        .bool_type()
                        .const_int(b as u64, false)
                        .as_basic_value_enum(),
                };

                let elem_llvm_type = self.azula_type_to_llvm_basic_type(elem_type);
                let val = self.coerce_int_width(val, elem_llvm_type);
                let gep = unsafe {
                    self.builder
                        .build_gep(elem_llvm_type, array_ptr, &[index_val], "gep")
                        .unwrap()
                };
                self.builder.build_store(gep, val).unwrap();
            }
            Instruction::AccessElement(array, index, dest, elem_type) => {
                let array_ptr = locals.load(value_to_local(array)).into_pointer_value();
                let index_val = locals.load(value_to_local(index)).into_int_value();
                let elem_llvm_type = self.azula_type_to_llvm_basic_type(elem_type);
                let gep = unsafe {
                    self.builder
                        .build_gep(elem_llvm_type, array_ptr, &[index_val], "gep")
                        .unwrap()
                };
                let loaded = self.builder.build_load(elem_llvm_type, gep, "load").unwrap();
                locals.store(dest, loaded);
            }
            Instruction::StoreStructMember(struc, index, val, struct_name) => {
                let val = match val {
                    Value::Local(ptr) => locals.load(ptr),
                    Value::LiteralInteger(n) => self
                        .context
                        .i64_type()
                        .const_int(n as u64, false)
                        .as_basic_value_enum(),
                    Value::LiteralBoolean(b) => self
                        .context
                        .bool_type()
                        .const_int(b as u64, false)
                        .as_basic_value_enum(),
                    Value::Global(v) => *self.strings.get(&v).unwrap(),
                };

                let struc_val = locals.load(value_to_local(struc.clone()));

                if struc_val.is_struct_value() {
                    let updated = self
                        .builder
                        .build_insert_value(
                            struc_val.into_struct_value(),
                            val,
                            index as u32,
                            "val",
                        )
                        .unwrap();

                    locals.store(value_to_local(struc), updated.as_basic_value_enum());
                } else if struc_val.is_pointer_value() {
                    let llvm_struct_type = *self.structs.get(&struct_name).unwrap();
                    let field_type = llvm_struct_type.get_field_type_at_index(index as u32).unwrap();
                    let val = self.coerce_int_width(val, field_type);
                    let gep = unsafe {
                        self.builder
                            .build_struct_gep(
                                llvm_struct_type,
                                struc_val.into_pointer_value(),
                                index as u32,
                                "gep",
                            )
                            .unwrap()
                    };
                    self.builder.build_store(gep, val).unwrap();
                }
            }
            Instruction::CreateStruct(struc, values, dest) => {
                let struc_type = self.structs.get(&struc).unwrap();

                let vals: Vec<_> = values
                    .iter()
                    .map(|val| match val {
                        Value::Local(ptr) => locals.load(*ptr),
                        Value::LiteralInteger(n) => self
                            .context
                            .i64_type()
                            .const_int(*n as u64, false)
                            .as_basic_value_enum(),
                        Value::LiteralBoolean(b) => self
                            .context
                            .bool_type()
                            .const_int(*b as u64, false)
                            .as_basic_value_enum(),
                        Value::Global(v) => *self.strings.get(&v).unwrap(),
                    })
                    .collect();

                let val = struc_type.const_named_struct(&[]);

                let mut agg = val.as_basic_value_enum().into_struct_value();
                for (index, arg) in vals.iter().enumerate() {
                    let field_type = struc_type.get_field_type_at_index(index as u32).unwrap();
                    let arg = &self.coerce_int_width(*arg, field_type);
                    agg = self
                        .builder
                        .build_insert_value(agg, *arg, index as u32, "insert")
                        .unwrap()
                        .as_basic_value_enum()
                        .into_struct_value();
                }

                locals.store(dest, agg.as_basic_value_enum());
            }
            Instruction::AccessStructMember(struc, index, dest, resolve, struct_name) => {
                let struc_val = locals.load(value_to_local(struc));

                if struc_val.is_struct_value() {
                    let val = self
                        .builder
                        .build_extract_value(struc_val.into_struct_value(), index as u32, "val")
                        .unwrap();

                    locals.store(dest, val.as_basic_value_enum());
                } else if struc_val.is_pointer_value() {
                    let llvm_struct_type = *self.structs.get(&struct_name).unwrap();
                    let gep = unsafe {
                        self.builder
                            .build_struct_gep(
                                llvm_struct_type,
                                struc_val.into_pointer_value(),
                                index as u32,
                                "gep",
                            )
                            .unwrap()
                    };
                    if resolve {
                        let field_type = llvm_struct_type.get_field_type_at_index(index as u32).unwrap();
                        let loaded = self.builder.build_load(field_type, gep, "load").unwrap();
                        locals.store(dest, loaded);
                    } else {
                        // resolve=false: return the field address (GEP pointer) for &field semantics
                        locals.store(dest, gep.as_basic_value_enum());
                    }
                }
            }
            Instruction::Cast(val, target_type, dest) => {
                let src = locals.load(value_to_local(val));
                let target_llvm = self.azula_type_to_llvm_basic_type(target_type);
                let result = match (src, target_llvm) {
                    (BasicValueEnum::IntValue(iv), BasicTypeEnum::IntType(it)) => {
                        let src_bits = iv.get_type().get_bit_width();
                        let dst_bits = it.get_bit_width();
                        if dst_bits > src_bits && src_bits == 1 {
                            self.builder.build_int_z_extend(iv, it, "zext").unwrap().as_basic_value_enum()
                        } else if dst_bits > src_bits {
                            self.builder.build_int_s_extend(iv, it, "sext").unwrap().as_basic_value_enum()
                        } else if dst_bits < src_bits {
                            self.builder.build_int_truncate(iv, it, "trunc").unwrap().as_basic_value_enum()
                        } else {
                            iv.as_basic_value_enum()
                        }
                    }
                    (BasicValueEnum::IntValue(iv), BasicTypeEnum::FloatType(ft)) => {
                        self.builder.build_signed_int_to_float(iv, ft, "itof").unwrap().as_basic_value_enum()
                    }
                    (BasicValueEnum::FloatValue(fv), BasicTypeEnum::IntType(it)) => {
                        self.builder.build_float_to_signed_int(fv, it, "ftoi").unwrap().as_basic_value_enum()
                    }
                    (BasicValueEnum::IntValue(iv), BasicTypeEnum::PointerType(pt)) => {
                        self.builder.build_int_to_ptr(iv, pt, "itop").unwrap().as_basic_value_enum()
                    }
                    (BasicValueEnum::PointerValue(pv), BasicTypeEnum::IntType(it)) => {
                        self.builder.build_ptr_to_int(pv, it, "ptoi").unwrap().as_basic_value_enum()
                    }
                    (other, _) => other,
                };
                locals.store(dest, result);
            }
        };
    }

    fn codegen_add(&self, instruction: Instruction<'a>, locals: &mut FunctionLocals<'a>) {
        if let Instruction::Add(val1, val2, dest) = instruction {
            let local1 = locals.load(value_to_local(val1));
            let local2 = locals.load(value_to_local(val2));
            let (local1, local2) = self.harmonize(local1, local2);

            let value = match local1.get_type() {
                BasicTypeEnum::FloatType(_) => self
                    .builder
                    .build_float_add(
                        local1.into_float_value(),
                        local2.into_float_value(),
                        "add",
                    )
                    .unwrap()
                    .as_basic_value_enum(),
                BasicTypeEnum::IntType(_) => self
                    .builder
                    .build_int_add(local1.into_int_value(), local2.into_int_value(), "add")
                    .unwrap()
                    .as_basic_value_enum(),
                _ => unreachable!(),
            };

            locals.store(dest, value);
        }
    }

    fn codegen_sub(&self, instruction: Instruction<'a>, locals: &mut FunctionLocals<'a>) {
        if let Instruction::Sub(val1, val2, dest) = instruction {
            let local1 = locals.load(value_to_local(val1));
            let local2 = locals.load(value_to_local(val2));
            let (local1, local2) = self.harmonize(local1, local2);

            let value = match local1.get_type() {
                BasicTypeEnum::FloatType(_) => self
                    .builder
                    .build_float_sub(
                        local1.into_float_value(),
                        local2.into_float_value(),
                        "sub",
                    )
                    .unwrap()
                    .as_basic_value_enum(),
                BasicTypeEnum::IntType(_) => self
                    .builder
                    .build_int_sub(local1.into_int_value(), local2.into_int_value(), "sub")
                    .unwrap()
                    .as_basic_value_enum(),
                _ => unreachable!(),
            };

            locals.store(dest, value.as_basic_value_enum());
        }
    }

    fn codegen_mul(&self, instruction: Instruction<'a>, locals: &mut FunctionLocals<'a>) {
        if let Instruction::Mul(val1, val2, dest) = instruction {
            let local1 = locals.load(value_to_local(val1));
            let local2 = locals.load(value_to_local(val2));
            let (local1, local2) = self.harmonize(local1, local2);

            let value = match local1.get_type() {
                BasicTypeEnum::FloatType(_) => self
                    .builder
                    .build_float_mul(
                        local1.into_float_value(),
                        local2.into_float_value(),
                        "mul",
                    )
                    .unwrap()
                    .as_basic_value_enum(),
                BasicTypeEnum::IntType(_) => self
                    .builder
                    .build_int_mul(local1.into_int_value(), local2.into_int_value(), "mul")
                    .unwrap()
                    .as_basic_value_enum(),
                _ => unreachable!(),
            };

            locals.store(dest, value.as_basic_value_enum());
        }
    }

    fn codegen_div(&self, instruction: Instruction<'a>, locals: &mut FunctionLocals<'a>) {
        if let Instruction::Div(val1, val2, dest) = instruction {
            let local1 = locals.load(value_to_local(val1));
            let local2 = locals.load(value_to_local(val2));
            let (local1, local2) = self.harmonize(local1, local2);

            let value = match local1.get_type() {
                BasicTypeEnum::FloatType(_) => self
                    .builder
                    .build_float_div(
                        local1.into_float_value(),
                        local2.into_float_value(),
                        "div",
                    )
                    .unwrap()
                    .as_basic_value_enum(),
                BasicTypeEnum::IntType(_) => self
                    .builder
                    .build_int_signed_div(
                        local1.into_int_value(),
                        local2.into_int_value(),
                        "div",
                    )
                    .unwrap()
                    .as_basic_value_enum(),
                _ => unreachable!(),
            };

            locals.store(dest, value.as_basic_value_enum());
        }
    }

    fn codegen_mod(&self, instruction: Instruction<'a>, locals: &mut FunctionLocals<'a>) {
        if let Instruction::Mod(val1, val2, dest) = instruction {
            let local1 = locals.load(value_to_local(val1));
            let local2 = locals.load(value_to_local(val2));
            let (local1, local2) = self.harmonize(local1, local2);

            let value = match local1.get_type() {
                BasicTypeEnum::FloatType(_) => self
                    .builder
                    .build_float_rem(
                        local1.into_float_value(),
                        local2.into_float_value(),
                        "mod",
                    )
                    .unwrap()
                    .as_basic_value_enum(),
                BasicTypeEnum::IntType(_) => self
                    .builder
                    .build_int_signed_rem(
                        local1.into_int_value(),
                        local2.into_int_value(),
                        "mod",
                    )
                    .unwrap()
                    .as_basic_value_enum(),
                _ => unreachable!(),
            };

            locals.store(dest, value.as_basic_value_enum());
        }
    }

    fn codegen_pow(&self, instruction: Instruction<'a>, locals: &mut FunctionLocals<'a>) {
        if let Instruction::Pow(val1, val2, dest) = instruction {
            let local1 = locals.load(value_to_local(val1));
            let local2 = locals.load(value_to_local(val2));
            let (local1, local2) = self.harmonize(local1, local2);

            let result = self
                .builder
                .build_call(
                    self.module.get_function("pow").unwrap(),
                    &[local1.into(), local2.into()],
                    "power",
                )
                .unwrap();

            locals.store(dest, result.try_as_basic_value().unwrap_basic());
        }
    }

    fn build_object_file(&self, dest: String) {
        let target_machine = self.create_machine(self.target.clone()).unwrap();

        target_machine
            .write_to_file(&self.module, FileType::Object, Path::new(&dest))
            .unwrap();
    }

    fn create_machine(&self, name: Option<String>) -> Option<TargetMachine> {
        if let Some(target) = name {
            let triple = TargetTriple::create(&target);

            self.module.set_triple(&triple);
            Target::initialize_all(&InitializationConfig::default());
            let target = Target::from_triple(&triple).unwrap();
            let mut opt_level = inkwell::OptimizationLevel::Default;
            if self.opt_level == OptimizationLevel::Aggressive {
                opt_level = inkwell::OptimizationLevel::Aggressive;
            }
            return target.create_target_machine(
                &triple,
                "",
                "",
                opt_level,
                inkwell::targets::RelocMode::PIC,
                inkwell::targets::CodeModel::Default,
            );
        }

        let triple = TargetMachine::get_default_triple();
        let cpu = TargetMachine::get_host_cpu_name().to_string();
        let features = TargetMachine::get_host_cpu_features().to_string();

        self.module.set_triple(&triple);
        Target::initialize_native(&InitializationConfig::default()).unwrap();
        let target = Target::from_triple(&triple).unwrap();
        let mut opt_level = inkwell::OptimizationLevel::Default;
        if self.opt_level == OptimizationLevel::Aggressive {
            opt_level = inkwell::OptimizationLevel::Aggressive;
        }
        target.create_target_machine(
            &triple,
            &cpu,
            &features,
            opt_level,
            inkwell::targets::RelocMode::PIC,
            inkwell::targets::CodeModel::Default,
        )
    }

    fn azula_type_to_llvm_basic_type(&self, t: AzulaType<'a>) -> BasicTypeEnum<'a> {
        match t {
            AzulaType::Int => self.context.i64_type().as_basic_type_enum(),
            AzulaType::SizedSignedInt(size) => match size {
                8 => self.context.i8_type().as_basic_type_enum(),
                16 => self.context.i16_type().as_basic_type_enum(),
                32 => self.context.i32_type().as_basic_type_enum(),
                64 => self.context.i64_type().as_basic_type_enum(),
                _ => unreachable!(),
            },
            AzulaType::SizedUnsignedInt(size) => match size {
                8 => self.context.i8_type().as_basic_type_enum(),
                16 => self.context.i16_type().as_basic_type_enum(),
                32 => self.context.i32_type().as_basic_type_enum(),
                64 => self.context.i64_type().as_basic_type_enum(),
                _ => unreachable!(),
            },
            AzulaType::Str => self.context.ptr_type(AddressSpace::default()).as_basic_type_enum(),
            AzulaType::Float => self.context.f64_type().as_basic_type_enum(),
            AzulaType::SizedFloat(size) => match size {
                16 => self.context.f16_type().as_basic_type_enum(),
                32 => self.context.f32_type().as_basic_type_enum(),
                64 => self.context.f64_type().as_basic_type_enum(),
                _ => unreachable!(),
            },
            AzulaType::Bool => self.context.bool_type().as_basic_type_enum(),
            AzulaType::Void => todo!(),
            AzulaType::Pointer(_) => {
                // LLVM 18 uses opaque pointers — all pointer types are just `ptr`
                self.context
                    .ptr_type(AddressSpace::default())
                    .as_basic_type_enum()
            }
            AzulaType::Infer => unreachable!(),
            AzulaType::Named(name) => {
                if self.boxed_enum_names.contains(&name) {
                    self.context.ptr_type(AddressSpace::default()).as_basic_type_enum()
                } else if self.enum_names.contains(&name) {
                    self.context.i64_type().as_basic_type_enum()
                } else {
                    self.structs
                        .get(&name.to_string())
                        .unwrap()
                        .as_basic_type_enum()
                }
            }
            AzulaType::UnknownType(_) => todo!(),
            AzulaType::Generic(..) => unreachable!("generic types are instantiated by the typechecker"),
            AzulaType::Array(_, _) => {
                // Arrays are heap-allocated; represented as opaque pointers in LLVM 18
                self.context
                    .ptr_type(AddressSpace::default())
                    .as_basic_type_enum()
            }
        }
    }

    /// Sign-extend the narrower of two integer operands so both have the same width.
    fn harmonize<'v>(
        &self,
        a: BasicValueEnum<'v>,
        b: BasicValueEnum<'v>,
    ) -> (BasicValueEnum<'v>, BasicValueEnum<'v>)
    where
        'a: 'v,
    {
        if let (BasicValueEnum::IntValue(x), BasicValueEnum::IntValue(y)) = (a, b) {
            let (xw, yw) = (x.get_type().get_bit_width(), y.get_type().get_bit_width());
            if xw < yw {
                return (self.coerce_int_width(a, y.get_type().as_basic_type_enum()), b);
            } else if yw < xw {
                return (a, self.coerce_int_width(b, x.get_type().as_basic_type_enum()));
            }
        }
        (a, b)
    }

    fn coerce_int_width<'v>(&self, val: BasicValueEnum<'v>, target: BasicTypeEnum<'v>) -> BasicValueEnum<'v>
    where 'a: 'v,
    {
        if let (BasicValueEnum::IntValue(iv), BasicTypeEnum::IntType(it)) = (val, target) {
            let src = iv.get_type().get_bit_width();
            let dst = it.get_bit_width();
            if dst > src && src == 1 {
                return self.builder.build_int_z_extend(iv, it, "zext").unwrap().as_basic_value_enum();
            } else if dst > src {
                return self.builder.build_int_s_extend(iv, it, "sext").unwrap().as_basic_value_enum();
            } else if dst < src {
                return self.builder.build_int_truncate(iv, it, "trunc").unwrap().as_basic_value_enum();
            }
        }
        val
    }

    fn azula_type_to_function_llvm_type(
        &self,
        t: AzulaType<'a>,
        args: &[BasicMetadataTypeEnum<'a>],
    ) -> FunctionType<'a> {
        self.azula_type_to_function_llvm_type_with_varargs(t, args, false)
    }

    fn azula_type_to_function_llvm_type_with_varargs(
        &self,
        t: AzulaType<'a>,
        args: &[BasicMetadataTypeEnum<'a>],
        varargs: bool,
    ) -> FunctionType<'a> {
        match t {
            AzulaType::Void => self.context.void_type().fn_type(args, varargs),
            _ => self.azula_type_to_llvm_basic_type(t).fn_type(args, varargs),
        }
    }
}

impl<'ctx> FunctionLocals<'ctx> {
    pub fn new() -> Self {
        Self {
            registers: HashMap::new(),
            variables: HashMap::new(),
            blocks: HashMap::new(),
        }
    }

    pub fn store(&mut self, dest: usize, value: BasicValueEnum<'ctx>) {
        self.registers.insert(dest, value);
    }

    pub fn load(&self, dest: usize) -> BasicValueEnum<'ctx> {
        self.registers.get(&dest).unwrap().clone()
    }
}

fn value_to_local(value: Value) -> usize {
    match value {
        Value::LiteralInteger(_) => unreachable!(),
        Value::LiteralBoolean(_) => unreachable!(),
        Value::Local(val) => val,
        Value::Global(_) => unreachable!(),
    }
}
