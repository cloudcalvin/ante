use std::collections::HashMap;
use std::path::Path;

use crate::args::Args;
use crate::cache::{DefinitionInfoId, DefinitionKind, ModuleCache};
use crate::lexer::token::IntegerKind;
use crate::parser::ast::{self, Ast};
use crate::types::typed::Typed;
use crate::types::{FunctionType, PrimitiveType, Type, TypeBinding, TypeConstructor, TypeInfoBody};
use crate::util::{fmap, trustme};
use cranelift::codegen::ir::immediates::Offset32;
use cranelift::codegen::ir::{types as cranelift_types, Function};
use cranelift::codegen::verify_function;
use cranelift::frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift::prelude::isa::{CallConv, TargetFrontendConfig};
use cranelift::prelude::{
    settings, AbiParam, EntityRef, ExtFuncData, ExternalName, InstBuilder, MemFlags, Signature,
    Value as CraneliftValue,
};
use cranelift_module::{DataContext, FuncId, Linkage, Module};

use super::module::DynModule;
use super::Codegen;

pub const BOXED_TYPE: cranelift_types::Type = cranelift_types::I64;

// TODO: Make this a threadsafe queue so we can compile functions in parallel
type FunctionQueue<'ast, 'c> = Vec<(FunctionRef<'ast, 'c>, Signature, FuncId)>;

enum FunctionRef<'a, 'c> {
    Lambda(&'a ast::Lambda<'c>),
    TypeConstructor { tag: &'a Option<u8>, typ: Type },
}

impl<'a, 'c> FunctionRef<'a, 'c> {
    fn get_type(&self) -> &Type {
        match self {
            FunctionRef::Lambda(lambda) => lambda.get_type().unwrap(),
            FunctionRef::TypeConstructor { typ, .. } => typ,
        }
    }
}

pub struct Context<'ast, 'c> {
    pub cache: &'ast mut ModuleCache<'c>,
    pub definitions: HashMap<DefinitionInfoId, Value>,
    module: DynModule,
    unique_id: u32,

    data_context: DataContext,

    pub current_function_name: Option<String>,
    pub current_function_parameters: Vec<CraneliftValue>,

    alloc_fn: FuncId,
    pub frontend_config: TargetFrontendConfig,

    function_queue: FunctionQueue<'ast, 'c>,
}

#[allow(unused)]
#[derive(Debug, Clone)]
pub enum Value {
    Normal(CraneliftValue),
    Function(ExtFuncData),
    Variable(Variable),
}

pub enum FunctionValue {
    Direct(ExtFuncData),
    Indirect(CraneliftValue), // function pointer
}

impl Value {
    /// Convert the value into a CraneliftValue
    pub fn eval<'local, 'c>(self, builder: &mut FunctionBuilder) -> CraneliftValue {
        match self {
            Value::Normal(value) => value,
            Value::Variable(variable) => builder.use_var(variable),
            Value::Function(data) => {
                let function_ref = builder.import_function(data);
                builder.ins().func_addr(BOXED_TYPE, function_ref)
            },
        }
    }

    pub fn eval_function<'local, 'ast, 'c>(self) -> FunctionValue {
        match self {
            Value::Function(data) => FunctionValue::Direct(data),
            Value::Normal(value) => FunctionValue::Indirect(value),
            other => unreachable!("Expected a function value, got: {:?}", other),
        }
    }
}

fn declare_malloc_function(module: &mut dyn Module) -> FuncId {
    let mut signature = Signature::new(CallConv::SystemV);
    // malloc doesn't really take a reference but we give it one anyway
    // to avoid having to convert between our boxed values. This is incorrect
    // if we compile on 32-bit platforms.
    signature.params.push(AbiParam::new(BOXED_TYPE));
    signature.returns.push(AbiParam::new(BOXED_TYPE));
    module
        .declare_function("malloc", Linkage::Import, &signature)
        .unwrap()
}

enum FunctionOrGlobal {
    Function(Signature),
    Global(cranelift_types::Type),
}

impl<'local, 'c> Context<'local, 'c> {
    fn new(
        output_path: &Path, use_jit: bool, cache: &'local mut ModuleCache<'c>,
    ) -> (Self, FunctionBuilderContext) {
        let builder_context = FunctionBuilderContext::new();
        let (mut module, frontend_config) =
            DynModule::new(output_path.to_string_lossy().into_owned(), use_jit);
        let alloc_fn = declare_malloc_function(&mut module);

        (
            Context {
                cache,
                definitions: HashMap::new(),
                module,
                unique_id: 1, // alloc_fn is id 0
                alloc_fn,
                frontend_config,
                data_context: DataContext::new(),
                function_queue: vec![],
                current_function_name: None,
                current_function_parameters: vec![],
            },
            builder_context,
        )
    }

    pub fn codegen_all(
        path: &Path, ast: &'local Ast<'c>, cache: &'local mut ModuleCache<'c>, args: &Args,
    ) {
        let output_path = path.with_extension("");
        let (mut context, mut builder_context) = Context::new(&output_path, !args.build, cache);
        let mut module_context = context.module.make_context();

        let main = context.codegen_main(ast, &mut builder_context, &mut module_context, args);

        // Then codegen any functions used by main and so forth
        while let Some((function, signature, id)) = context.function_queue.pop() {
            context.codegen_function(
                function,
                &mut builder_context,
                &mut module_context,
                signature,
                id,
                args,
            );
        }

        context.module.finish(main, &output_path);
    }

    pub fn codegen_eval<T: Codegen<'c>>(
        &mut self, ast: &'local T, builder: &mut FunctionBuilder,
    ) -> CraneliftValue {
        ast.codegen(self, builder).eval(builder)
    }

    fn codegen_function(
        &mut self, function: FunctionRef<'local, 'c>, context: &mut FunctionBuilderContext,
        module_context: &mut cranelift::codegen::Context, signature: Signature,
        function_id: FuncId, args: &Args,
    ) {
        module_context.func =
            Function::with_name_signature(ExternalName::user(0, function_id.as_u32()), signature);
        let mut builder = FunctionBuilder::new(&mut module_context.func, context);

        let entry = builder.create_block();
        builder.switch_to_block(entry);
        builder.seal_block(entry);

        let body = self.codegen_function_inner(function, &mut builder);
        self.create_return(body, &mut builder);

        builder.append_block_params_for_function_params(entry);
        builder.finalize();

        if args.show_ir {
            println!("{}", module_context.func.display());
        }

        let flags = settings::Flags::new(settings::builder());
        if let Err(errors) = verify_function(&module_context.func, &flags) {
            panic!("{}", errors);
        }

        self.module
            .define_function(function_id, module_context)
            .unwrap();
        module_context.clear();
    }

    pub fn next_unique_id(&mut self) -> u32 {
        self.unique_id += 1;
        self.unique_id
    }

    fn codegen_main(
        &mut self, ast: &'local Ast<'c>, builder_context: &mut FunctionBuilderContext,
        module_context: &mut cranelift::codegen::Context, args: &Args,
    ) -> FuncId {
        let func = &mut module_context.func;
        func.signature
            .returns
            .push(AbiParam::new(cranelift_types::I32));

        let main_id = self
            .module
            .declare_function("main", Linkage::Export, &func.signature)
            .unwrap();
        let mut builder = FunctionBuilder::new(func, builder_context);
        let entry = builder.create_block();

        builder.switch_to_block(entry);
        builder.seal_block(entry);

        ast.codegen(self, &mut builder);

        let zero = builder.ins().iconst(cranelift_types::I32, 0);
        self.create_return(Value::Normal(zero), &mut builder);

        builder.finalize();

        let flags = settings::Flags::new(settings::builder());
        let func = &module_context.func;
        let res = verify_function(&func, &flags);

        if args.show_ir {
            println!("{}", func.display());
        }

        if let Err(errors) = res {
            panic!("{}", errors);
        }

        self.module
            .define_function(main_id, module_context)
            .unwrap();
        module_context.clear();
        main_id
    }

    fn codegen_function_inner(
        &mut self, function: FunctionRef<'local, 'c>, builder: &mut FunctionBuilder,
    ) -> Value {
        match function {
            FunctionRef::Lambda(lambda) => self.codegen_lambda(lambda, builder),
            FunctionRef::TypeConstructor { tag, typ } => {
                self.codegen_type_constructor_function(tag, &typ, builder)
            },
        }
    }

    fn codegen_lambda(
        &mut self, lambda: &'local ast::Lambda<'c>, builder: &mut FunctionBuilder,
    ) -> Value {
        // TODO Parameter binding
        for _parameter in &lambda.args {
            let x = Variable::new(self.next_unique_id() as usize);
            builder.declare_var(x, BOXED_TYPE);
        }

        lambda.body.codegen(self, builder)
    }

    fn codegen_type_constructor(
        &mut self, tag: &'local Option<u8>, typ: &Type, name: &str, builder: &mut FunctionBuilder,
    ) -> Value {
        match typ {
            Type::Function(_) => {
                self.add_type_constructor_to_queue(tag, typ.clone(), name, builder)
            },
            Type::TypeVariable(id) => {
                match &self.cache.type_bindings[id.0] {
                    TypeBinding::Bound(binding) => {
                        // TODO: Can we remove the cloning here?
                        let binding = binding.clone();
                        self.codegen_type_constructor(tag, &binding, name, builder)
                    },
                    TypeBinding::Unbound(_, _) => unreachable!(),
                }
            },
            Type::TypeApplication(typ, _args) => {
                self.codegen_type_constructor(tag, typ, name, builder)
            },
            Type::ForAll(_, typ) => self.codegen_type_constructor(tag, typ, name, builder),
            Type::UserDefinedType(_) => {
                // This type constructor is not a function type, it is just a single tag value then
                // TODO: What do we do for nullary struct values?
                Value::Normal(builder.ins().iconst(BOXED_TYPE, tag.unwrap_or(0) as i64))
            },
            Type::Primitive(_) => unreachable!(),
            Type::Ref(_) => unreachable!(),
        }
    }

    fn codegen_type_constructor_function(
        &mut self, tag: &Option<u8>, typ: &Type, builder: &mut FunctionBuilder,
    ) -> Value {
        let f = match typ {
            Type::Function(f) => f,
            _ => unreachable!(),
        };

        let mut params = Vec::with_capacity(f.parameters.len() + 1);
        if let Some(tag) = tag {
            params.push(builder.ins().iconst(BOXED_TYPE, *tag as i64));
        }

        for _ in &f.parameters {
            let param = Variable::new(self.next_unique_id() as usize);
            builder.declare_var(param, BOXED_TYPE);
            params.push(builder.use_var(param));
        }

        Value::Normal(self.alloc(&params, builder))
    }

    fn resolve_type(&mut self, typ: &Type) -> Type {
        match typ {
            Type::Primitive(p) => Type::Primitive(*p),
            Type::Function(f) => {
                let f = FunctionType {
                    parameters: fmap(&f.parameters, |parameter| self.resolve_type(parameter)),
                    return_type: Box::new(self.resolve_type(f.return_type.as_ref())),
                    environment: Box::new(self.resolve_type(f.environment.as_ref())),
                    is_varargs: f.is_varargs,
                };
                Type::Function(f)
            },
            Type::TypeVariable(id) => match &self.cache.type_bindings[id.0] {
                TypeBinding::Bound(t) => {
                    let t = t.clone();
                    self.resolve_type(&t)
                },
                // Default to unit
                TypeBinding::Unbound(_, _) => Type::Primitive(PrimitiveType::UnitType),
            },
            Type::UserDefinedType(id) => Type::UserDefinedType(*id),
            Type::TypeApplication(c, args) => Type::TypeApplication(
                Box::new(self.resolve_type(c)),
                fmap(args, |arg| self.resolve_type(arg)),
            ),
            Type::Ref(id) => Type::Ref(*id),
            Type::ForAll(_vars, typ) => self.resolve_type(typ.as_ref()),
        }
    }

    fn convert_type(&mut self, _typ: &Type) -> cranelift_types::Type {
        BOXED_TYPE
    }

    pub fn convert_signature(&mut self, typ: &Type) -> Signature {
        let typ = self.resolve_type(typ);
        let mut sig = Signature::new(CallConv::Fast);

        match typ {
            Type::Function(f) => {
                for parameter in &f.parameters {
                    let cranelift_type = self.convert_type(parameter);
                    sig.params.push(AbiParam::new(cranelift_type));
                }

                let cranelift_type = self.convert_type(f.return_type.as_ref());
                sig.returns.push(AbiParam::new(cranelift_type));
                sig
            },
            _ => unreachable!(
                "called convert_signature with type {}",
                typ.display(self.cache)
            ),
        }
    }

    pub fn unboxed_integer_type(&mut self, kind: &IntegerKind) -> cranelift_types::Type {
        match kind {
            IntegerKind::Unknown => unreachable!("Unknown IntegerKind encountered during codegen"),
            IntegerKind::Inferred(id) => self.convert_type(&Type::TypeVariable(*id)),
            IntegerKind::I8 | IntegerKind::U8 => cranelift_types::I8,
            IntegerKind::I16 | IntegerKind::U16 => cranelift_types::I16,
            IntegerKind::I32 | IntegerKind::U32 => cranelift_types::I32,
            IntegerKind::I64 | IntegerKind::Isz | IntegerKind::U64 | IntegerKind::Usz => {
                cranelift_types::I64
            },
        }
    }

    pub fn codegen_definition(
        &mut self, id: DefinitionInfoId, builder: &mut FunctionBuilder,
    ) -> Value {
        let definition = &mut self.cache.definition_infos[id.0];
        let definition = trustme::extend_lifetime(definition);

        let value = match &definition.definition {
            Some(DefinitionKind::Definition(definition)) => definition.codegen(self, builder),
            Some(DefinitionKind::Extern(annotation)) => self.codegen_extern(*annotation, builder),
            Some(DefinitionKind::TypeConstructor { name, tag }) => {
                self.codegen_type_constructor(tag, definition.typ.as_ref().unwrap(), name, builder)
            },
            Some(DefinitionKind::TraitDefinition(definition)) => {
                unreachable!("No trait impl for trait {}", definition)
            },
            Some(DefinitionKind::Parameter) => unreachable!(
                "Parameter definitions should already be codegen'd, {}, id = {}",
                definition.name, id.0
            ),
            Some(DefinitionKind::MatchPattern) => unreachable!(
                "Pattern definitions should already be codegen'd, {}, id = {}",
                definition.name, id.0
            ),
            None => unreachable!("Variable {} has no definition", id.0),
        };

        self.definitions.insert(id, value.clone());
        value
    }

    pub fn create_return(&mut self, value: Value, builder: &mut FunctionBuilder) {
        // TODO: Check for pre-existing branch instruction
        let value = value.eval(builder);
        builder.ins().return_(&[value]);
    }

    fn add_function_to_queue(
        &mut self, function: FunctionRef<'local, 'c>, name: &str, builder: &mut FunctionBuilder,
    ) -> Value {
        let signature = self.convert_signature(function.get_type());
        let function_id = self
            .module
            .declare_function(name, Linkage::Export, &signature)
            .unwrap();
        self.function_queue
            .push((function, signature.clone(), function_id));

        let signature = builder.import_signature(signature);

        Value::Function(ExtFuncData {
            name: ExternalName::user(0, function_id.as_u32()),
            signature,
            // Using 'true' here gives an unimplemented error on aarch64
            colocated: false,
        })
    }

    pub fn add_lambda_to_queue(
        &mut self, lambda: &'local ast::Lambda<'c>, name: &str, builder: &mut FunctionBuilder,
    ) -> Value {
        self.add_function_to_queue(FunctionRef::Lambda(lambda), name, builder)
    }

    pub fn add_type_constructor_to_queue(
        &mut self, tag: &'local Option<u8>, typ: Type, name: &str, builder: &mut FunctionBuilder,
    ) -> Value {
        self.add_function_to_queue(FunctionRef::TypeConstructor { tag, typ }, name, builder)
    }

    pub fn unit_value(&mut self, builder: &mut FunctionBuilder) -> Value {
        Value::Normal(builder.ins().iconst(BOXED_TYPE, 0))
    }

    /// Boxes a value at runtime.
    ///
    /// This expects all `values` to be boxed types and thus
    /// the total size of the allocation will be sizeof(usize) * values.len()
    ///
    /// This will be called very often as the cranelift backend will perform
    /// boxing instead of monomorphisation to handle generics.
    fn alloc(
        &mut self, values: &[CraneliftValue], builder: &mut FunctionBuilder,
    ) -> CraneliftValue {
        let function_ref = self
            .module
            .declare_func_in_func(self.alloc_fn, builder.func);

        let size = self.pointer_size() as i64 * values.len() as i64;
        let size = builder.ins().iconst(BOXED_TYPE, size);

        let call = builder.ins().call(function_ref, &[size]);
        let results = builder.inst_results(call);
        assert_eq!(results.len(), 1);
        let allocated = results[0];

        for (i, value) in values.into_iter().enumerate() {
            let offset = self.pointer_size() * i as i32;
            builder
                .ins()
                .store(MemFlags::new(), *value, allocated, offset);
        }

        allocated
    }

    /// Binds the given pattern to the given value, recursively filling in
    /// any definitions in the pattern to the corresponding value.
    ///
    /// Like all values in this IR, `value` is expected to be boxed, so
    /// we must unbox the value and cast it at each step as we unwrap it.
    pub fn bind_pattern(
        &mut self, pattern: &Ast, value: CraneliftValue, builder: &mut FunctionBuilder,
    ) {
        match pattern {
            Ast::Literal(_) => (), // Nothing to do
            Ast::Variable(variable) => {
                let id = variable.definition.unwrap();

                // Unlike monomorphisation in the llvm pass, we should never expect to
                // invalidate previous work by binding the same definition to a new value.
                if let Some(old_value) = self.definitions.insert(id, Value::Normal(value)) {
                    unreachable!(
                        "bind_pattern tried to bind to {}, but it was already bound to {:?}",
                        pattern, old_value
                    );
                }
            },
            // This should be an irrefutable pattern (struct/tuple), arbitrary patterns
            // are handled only when compiling decision trees.
            Ast::FunctionCall(call) => {
                let offsets = self.field_offsets(call.typ.as_ref().unwrap());
                assert_eq!(offsets.len(), call.args.len());

                for (arg_pattern, arg_offset) in call.args.iter().zip(offsets) {
                    let flags = MemFlags::new();
                    let arg_value = builder.ins().load(BOXED_TYPE, flags, value, arg_offset);
                    self.bind_pattern(arg_pattern, arg_value, builder);
                }
            },
            Ast::TypeAnnotation(annotation) => self.bind_pattern(&annotation.lhs, value, builder),
            _ => unreachable!("Invalid pattern given to bind_pattern: {}", pattern),
        }
    }

    /// Returns a Vec of byte offsets of each field of this type.
    fn field_offsets(&self, struct_type: &Type) -> Vec<Offset32> {
        match struct_type {
            Type::Primitive(_) => unreachable!(),
            Type::Function(_) => unreachable!(),
            Type::TypeVariable(id) => match &self.cache.type_bindings[id.0] {
                TypeBinding::Bound(binding) => self.field_offsets(binding),
                TypeBinding::Unbound(..) => unreachable!(),
            },
            Type::Ref(_) => unreachable!(),
            Type::ForAll(_, _) => unreachable!(),
            Type::UserDefinedType(id) => {
                let type_info = &self.cache.type_infos[id.0];
                match &type_info.body {
                    TypeInfoBody::Union(_) => unreachable!(),
                    TypeInfoBody::Unknown => unreachable!(),
                    TypeInfoBody::Alias(alias) => self.field_offsets(alias),
                    TypeInfoBody::Struct(fields) => {
                        let mut offset = 0;
                        fmap(fields, |field| {
                            let field_offset = offset;
                            offset += self.size_of_unboxed_type(&field.field_type);
                            Offset32::new(field_offset)
                        })
                    },
                }
            },

            // This is much simpler than the equivalent monomorphised version
            // since we do not have to keep track of type arguments thanks to
            // uniform representation.
            Type::TypeApplication(base_type, _) => self.field_offsets(base_type),
        }
    }

    /// Returns the size of the given type in bytes.
    ///
    /// The type is considered to be shallowly-unboxed.
    /// That is, the outermost type will be unboxed but any
    /// fields contained within will still be boxed.
    pub fn size_of_unboxed_type(&self, field_type: &Type) -> i32 {
        match field_type {
            Type::Primitive(primitive) => self.size_of_primitive(primitive),
            Type::Function(_) => self.pointer_size(),
            Type::TypeVariable(id) => {
                match &self.cache.type_bindings[id.0] {
                    TypeBinding::Bound(binding) => self.size_of_unboxed_type(binding),
                    // Default to i32. TODO: Re-evaluate this. We could default to unit instead.
                    TypeBinding::Unbound(..) => std::mem::size_of::<i32>() as i32,
                }
            },
            Type::UserDefinedType(id) => {
                let type_info = &self.cache.type_infos[id.0];
                match &type_info.body {
                    TypeInfoBody::Unknown => unreachable!(),
                    TypeInfoBody::Alias(alias) => self.size_of_unboxed_type(alias),
                    // All fields are boxed
                    TypeInfoBody::Struct(fields) => fields.len() as i32 * self.pointer_size(),
                    TypeInfoBody::Union(variants) => self.size_of_union(variants),
                }
            },
            Type::TypeApplication(base_type, _) => self.size_of_unboxed_type(base_type),
            Type::Ref(_) => self.pointer_size(),
            Type::ForAll(_, typ) => self.size_of_unboxed_type(typ),
        }
    }

    fn size_of_primitive(&self, primitive: &PrimitiveType) -> i32 {
        match primitive {
            PrimitiveType::IntegerType(kind) => {
                match kind {
                    IntegerKind::Unknown => unreachable!(),
                    IntegerKind::Inferred(id) => {
                        match &self.cache.type_bindings[id.0] {
                            TypeBinding::Bound(binding) => self.size_of_unboxed_type(binding),
                            // Default to i32
                            TypeBinding::Unbound(..) => std::mem::size_of::<i32>() as i32,
                        }
                    },
                    IntegerKind::I8 | IntegerKind::U8 => 1,
                    IntegerKind::I16 | IntegerKind::U16 => 2,
                    IntegerKind::I32 | IntegerKind::U32 => 4,
                    IntegerKind::I64 | IntegerKind::U64 => 8,
                    IntegerKind::Isz | IntegerKind::Usz => self.pointer_size(),
                }
            },
            PrimitiveType::FloatType => 8,
            PrimitiveType::CharType => 1,
            PrimitiveType::BooleanType => 1,
            PrimitiveType::UnitType => 1,
            PrimitiveType::Ptr => self.pointer_size(),
        }
    }

    /// Returns the size of a sum type in bytes.
    /// This should match the size of its largest variant + an extra byte for the tag
    fn size_of_union(&self, variants: &[TypeConstructor]) -> i32 {
        variants
            .iter()
            .map(|variant| variant.args.len() as i32 * self.pointer_size() + 1)
            .max()
            .unwrap_or(1)
    }

    /// Returns the size of a pointer in bytes.
    /// TODO: Adjust based on target platform
    fn pointer_size(&self) -> i32 {
        std::mem::size_of::<*const u8>() as i32
    }

    fn codegen_extern(
        &mut self, annotation: &ast::TypeAnnotation, builder: &mut FunctionBuilder,
    ) -> Value {
        let name = match annotation.lhs.as_ref() {
            Ast::Variable(variable) => variable.to_string(),
            other => unimplemented!(
                "Extern declarations for '{}' patterns are unimplemented",
                other
            ),
        };

        match self.convert_extern_signature(annotation.typ.as_ref().unwrap()) {
            FunctionOrGlobal::Global(_typ) => {
                todo!("Extern globals")
            },
            FunctionOrGlobal::Function(signature) => {
                let id = self
                    .module
                    .declare_function(&name, Linkage::Import, &signature)
                    .unwrap();
                let signature = builder.import_signature(signature);

                Value::Function(ExtFuncData {
                    name: ExternalName::user(0, id.as_u32()),
                    signature,
                    colocated: false,
                })
            },
        }
    }

    fn convert_extern_signature(&self, typ: &Type) -> FunctionOrGlobal {
        match typ {
            Type::TypeVariable(id) => match &self.cache.type_bindings[id.0] {
                TypeBinding::Bound(t) => self.convert_extern_signature(t),
                TypeBinding::Unbound(_, _) => {
                    // Technically valid, but very questionable if a user declares an
                    // extern global with an unbound type variable type
                    FunctionOrGlobal::Global(BOXED_TYPE)
                },
            },
            Type::Function(f) => {
                let mut signature = Signature::new(CallConv::SystemV);
                for parameter in &f.parameters {
                    let t = self.convert_extern_type(parameter);
                    signature.params.push(AbiParam::new(t));
                }
                let ret = self.convert_extern_type(f.return_type.as_ref());
                signature.returns.push(AbiParam::new(ret));
                FunctionOrGlobal::Function(signature)
            },
            Type::ForAll(_vars, typ) => self.convert_extern_signature(typ.as_ref()),
            other => FunctionOrGlobal::Global(self.convert_extern_type(other)),
        }
    }

    /// Convert the type of an extern value to a cranelift type.
    ///
    /// Note that this is currently separate from convert_type and convert_signature
    /// because we need to error if any externs are declared that use C structs or
    /// other types that would be incompatible with our "box everything" approach.
    fn convert_extern_type(&self, typ: &Type) -> cranelift_types::Type {
        match typ {
            Type::Primitive(_) => BOXED_TYPE,
            Type::Function(_) => BOXED_TYPE,
            Type::TypeVariable(id) => match &self.cache.type_bindings[id.0] {
                TypeBinding::Bound(t) => self.convert_extern_type(t),
                TypeBinding::Unbound(_, _) => BOXED_TYPE,
            },
            Type::UserDefinedType(_id) => {
                unimplemented!()
            },
            Type::TypeApplication(c, _args) => {
                // TODO: check if args cause c to be larger than BOXED_TYPE
                self.convert_extern_type(c.as_ref())
            },
            Type::Ref(_) => BOXED_TYPE,
            Type::ForAll(_vars, typ) => self.convert_extern_type(typ.as_ref()),
        }
    }

    /// Declare a string global value and get a reference to it
    fn c_string_value(&mut self, value: &str, builder: &mut FunctionBuilder) -> CraneliftValue {
        let mut value = value.to_owned();
        assert!(!value.ends_with('\0'));
        value.push('\0');

        let value = value.into_bytes().into_boxed_slice();
        self.data_context.define(value);

        let name = format!("string{}", self.next_unique_id());
        let data_id = self
            .module
            .declare_data(&name, Linkage::Local, true, false)
            .unwrap();

        self.module
            .define_data(data_id, &self.data_context)
            .unwrap();
        self.data_context.clear();
        let global = self.module.declare_data_in_func(data_id, builder.func);

        builder.ins().symbol_value(BOXED_TYPE, global)
    }

    pub fn string_value(&mut self, value: &str, builder: &mut FunctionBuilder) -> CraneliftValue {
        let c_string = self.c_string_value(value, builder);
        let length = builder.ins().iconst(BOXED_TYPE, value.len() as i64);
        self.alloc(&[c_string, length], builder)
    }

    pub fn get_field_index(&self, field_name: &str, typ: &Type) -> u32 {
        match typ {
            Type::UserDefinedType(id) => self.cache.type_infos[id.0]
                .find_field(field_name)
                .map(|(i, _)| i)
                .unwrap(),
            Type::TypeVariable(id) => match &self.cache.type_bindings[id.0] {
                TypeBinding::Bound(binding) => self.get_field_index(field_name, binding),
                TypeBinding::Unbound(..) => unreachable!("Type variable {} is unbound", id.0),
            },
            _ => {
                unreachable!(
                    "get_field_index called with a type that clearly doesn't have a {} field: {}",
                    field_name,
                    typ.display(self.cache)
                );
            },
        }
    }
}