// Copyright (c) The Diem Core Contributors
// Copyright (c) The Move Contributors
// SPDX-License-Identifier: Apache-2.0

//! This module contains verification of usage of dependencies for modules and scripts.
use crate::VerifierConfig;
use move_binary_format::{
    access::{ModuleAccess, ScriptAccess},
    binary_views::BinaryIndexedView,
    errors::{verification_error, Location, PartialVMError, PartialVMResult, VMResult},
    file_format::{
        Bytecode, CodeOffset, CompiledModule, CompiledScript, FunctionAttribute,
        FunctionDefinitionIndex, FunctionHandleIndex, ModuleHandleIndex, SignatureToken,
        StructHandleIndex, StructTypeParameter, TableIndex, Visibility,
    },
    file_format_common::VERSION_5,
    safe_unwrap,
    views::FunctionHandleView,
    IndexKind,
};
use move_core_types::{
    ability::AbilitySet, identifier::Identifier, language_storage::ModuleId, vm_status::StatusCode,
};
use std::collections::{BTreeMap, BTreeSet};

struct Context<'a, 'b> {
    resolver: BinaryIndexedView<'a>,
    // (Module -> CompiledModule) for (at least) all immediate dependencies
    dependency_map: BTreeMap<ModuleId, &'b CompiledModule>,
    // (Module::StructName -> handle) for all types of all dependencies
    struct_id_to_handle_map: BTreeMap<(ModuleId, Identifier), StructHandleIndex>,
    // (Module::FunctionName -> (handle_idx, def_idx)) for all functions that can ever be
    // called by this module/script in all dependencies
    func_id_to_index_map:
        BTreeMap<(ModuleId, Identifier), (FunctionHandleIndex, FunctionDefinitionIndex)>,
    // (handle -> visibility) for all function handles found in the module being checked
    function_visibilities: BTreeMap<FunctionHandleIndex, Visibility>,
    // all function handles found in the module being checked that are script functions in <V5
    // None if the current module/script >= V5
    script_functions: Option<BTreeSet<FunctionHandleIndex>>,
}

impl<'a, 'b> Context<'a, 'b> {
    fn module(
        module: &'a CompiledModule,
        dependencies: impl IntoIterator<Item = &'b CompiledModule>,
    ) -> Self {
        Self::new(BinaryIndexedView::Module(module), dependencies)
    }

    fn script(
        script: &'a CompiledScript,
        dependencies: impl IntoIterator<Item = &'b CompiledModule>,
    ) -> Self {
        Self::new(BinaryIndexedView::Script(script), dependencies)
    }

    fn new(
        resolver: BinaryIndexedView<'a>,
        dependencies: impl IntoIterator<Item = &'b CompiledModule>,
    ) -> Self {
        let self_module = resolver.self_id();
        let self_module_idx = resolver.self_handle_idx();
        let empty_defs = &vec![];
        let self_function_defs = match &resolver {
            BinaryIndexedView::Module(m) => m.function_defs(),
            BinaryIndexedView::Script(_) => empty_defs,
        };
        let dependency_map = dependencies
            .into_iter()
            .filter(|d| Some(d.self_id()) != self_module)
            .map(|d| (d.self_id(), d))
            .collect();

        let script_functions = if resolver.version() < VERSION_5 {
            Some(BTreeSet::new())
        } else {
            None
        };
        let mut context = Self {
            resolver,
            dependency_map,
            struct_id_to_handle_map: BTreeMap::new(),
            func_id_to_index_map: BTreeMap::new(),
            function_visibilities: BTreeMap::new(),
            script_functions,
        };

        let mut dependency_visibilities = BTreeMap::new();
        for (module_id, module) in &context.dependency_map {
            let friend_module_ids: BTreeSet<_> = module.immediate_friends().into_iter().collect();

            // Module::StructName -> def handle idx
            for struct_def in module.struct_defs() {
                let struct_handle = module.struct_handle_at(struct_def.struct_handle);
                let struct_name = module.identifier_at(struct_handle.name);
                context.struct_id_to_handle_map.insert(
                    (module_id.clone(), struct_name.to_owned()),
                    struct_def.struct_handle,
                );
            }
            // Module::FuncName -> def handle idx
            for (idx, func_def) in module.function_defs().iter().enumerate() {
                let func_handle = module.function_handle_at(func_def.function);
                let func_name = module.identifier_at(func_handle.name);
                dependency_visibilities.insert(
                    (module_id.clone(), func_name.to_owned()),
                    (func_def.visibility, func_def.is_entry),
                );
                let may_be_called = match func_def.visibility {
                    Visibility::Public => true,
                    Visibility::Friend => self_module
                        .as_ref()
                        .is_some_and(|self_id| friend_module_ids.contains(self_id)),
                    Visibility::Private => false,
                };
                if may_be_called {
                    context.func_id_to_index_map.insert(
                        (module_id.clone(), func_name.to_owned()),
                        (
                            func_def.function,
                            FunctionDefinitionIndex(idx as TableIndex),
                        ),
                    );
                }
            }
        }

        for function_def in self_function_defs {
            context
                .function_visibilities
                .insert(function_def.function, function_def.visibility);
            if function_def.is_entry {
                context
                    .script_functions
                    .as_mut()
                    .map(|s| s.insert(function_def.function));
            }
        }
        for (idx, function_handle) in context.resolver.function_handles().iter().enumerate() {
            if Some(function_handle.module) == self_module_idx {
                continue;
            }
            let dep_module_id = context
                .resolver
                .module_id_for_handle(context.resolver.module_handle_at(function_handle.module));
            let function_name = context.resolver.identifier_at(function_handle.name);
            let dep_file_format_version =
                context.dependency_map.get(&dep_module_id).unwrap().version;
            let dep_function = (dep_module_id, function_name.to_owned());
            let (visibility, is_entry) = match dependency_visibilities.get(&dep_function) {
                // The visibility does not need to be set here. If the function does not
                // link, it will be reported by verify_imported_functions
                None => continue,
                Some(vis_entry) => *vis_entry,
            };
            let fhandle_idx = FunctionHandleIndex(idx as TableIndex);
            context
                .function_visibilities
                .insert(fhandle_idx, visibility);
            if dep_file_format_version < VERSION_5 && is_entry {
                context
                    .script_functions
                    .as_mut()
                    .map(|s| s.insert(fhandle_idx));
            }
        }

        context
    }
}

pub fn verify_module<'a>(
    config: &VerifierConfig,
    module: &CompiledModule,
    dependencies: impl IntoIterator<Item = &'a CompiledModule>,
) -> VMResult<()> {
    if config.verify_nothing() {
        return Ok(());
    }
    verify_module_impl(module, dependencies)
        .map_err(|e| e.finish(Location::Module(module.self_id())))
}

fn verify_module_impl<'a>(
    module: &CompiledModule,
    dependencies: impl IntoIterator<Item = &'a CompiledModule>,
) -> PartialVMResult<()> {
    let context = &Context::module(module, dependencies);

    verify_imported_modules(context)?;
    verify_imported_structs(context)?;
    verify_imported_functions(context)?;
    verify_all_script_visibility_usage(context)
}

pub fn verify_script<'a>(
    config: &VerifierConfig,
    script: &CompiledScript,
    dependencies: impl IntoIterator<Item = &'a CompiledModule>,
) -> VMResult<()> {
    if config.verify_nothing() {
        return Ok(());
    }
    verify_script_impl(script, dependencies).map_err(|e| e.finish(Location::Script))
}

pub fn verify_script_impl<'a>(
    script: &CompiledScript,
    dependencies: impl IntoIterator<Item = &'a CompiledModule>,
) -> PartialVMResult<()> {
    let context = &Context::script(script, dependencies);

    verify_imported_modules(context)?;
    verify_imported_structs(context)?;
    verify_imported_functions(context)?;
    verify_all_script_visibility_usage(context)
}

fn verify_imported_modules(context: &Context) -> PartialVMResult<()> {
    let self_module = context.resolver.self_handle_idx();
    for (idx, module_handle) in context.resolver.module_handles().iter().enumerate() {
        let module_id = context.resolver.module_id_for_handle(module_handle);
        if Some(ModuleHandleIndex(idx as u16)) != self_module
            && !context.dependency_map.contains_key(&module_id)
        {
            return Err(verification_error(
                StatusCode::MISSING_DEPENDENCY,
                IndexKind::ModuleHandle,
                idx as TableIndex,
            ));
        }
    }
    Ok(())
}

fn verify_imported_structs(context: &Context) -> PartialVMResult<()> {
    let self_module = context.resolver.self_handle_idx();
    for (idx, struct_handle) in context.resolver.struct_handles().iter().enumerate() {
        if Some(struct_handle.module) == self_module {
            continue;
        }
        let owner_module_id = context
            .resolver
            .module_id_for_handle(context.resolver.module_handle_at(struct_handle.module));
        // TODO: remove unwrap
        let owner_module = safe_unwrap!(context.dependency_map.get(&owner_module_id));
        let struct_name = context.resolver.identifier_at(struct_handle.name);
        match context
            .struct_id_to_handle_map
            .get(&(owner_module_id, struct_name.to_owned()))
        {
            Some(def_idx) => {
                let def_handle = owner_module.struct_handle_at(*def_idx);
                if !compatible_struct_abilities(struct_handle.abilities, def_handle.abilities)
                    || !compatible_struct_type_parameters(
                        &struct_handle.type_parameters,
                        &def_handle.type_parameters,
                    )
                {
                    return Err(verification_error(
                        StatusCode::TYPE_MISMATCH,
                        IndexKind::StructHandle,
                        idx as TableIndex,
                    )
                    .with_message("imported struct mismatches expectation"));
                }
            },
            None => {
                return Err(verification_error(
                    StatusCode::LOOKUP_FAILED,
                    IndexKind::StructHandle,
                    idx as TableIndex,
                ));
            },
        }
    }
    Ok(())
}

fn verify_imported_functions(context: &Context) -> PartialVMResult<()> {
    let self_module = context.resolver.self_handle_idx();
    for (idx, function_handle) in context.resolver.function_handles().iter().enumerate() {
        if Some(function_handle.module) == self_module {
            continue;
        }
        let owner_module_id = context
            .resolver
            .module_id_for_handle(context.resolver.module_handle_at(function_handle.module));
        let function_name = context.resolver.identifier_at(function_handle.name);
        let owner_module = safe_unwrap!(context.dependency_map.get(&owner_module_id));
        match context
            .func_id_to_index_map
            .get(&(owner_module_id.clone(), function_name.to_owned()))
        {
            Some((owner_handle_idx, owner_def_idx)) => {
                let def_handle = owner_module.function_handle_at(*owner_handle_idx);
                // compatible type parameter constraints
                if !compatible_fun_type_parameters(
                    &function_handle.type_parameters,
                    &def_handle.type_parameters,
                ) {
                    return Err(verification_error(
                        StatusCode::TYPE_MISMATCH,
                        IndexKind::FunctionHandle,
                        idx as TableIndex,
                    )
                    .with_message("imported function mismatches expectation"));
                }
                // same parameters
                let handle_params = context.resolver.signature_at(function_handle.parameters);
                let def_params = match context.dependency_map.get(&owner_module_id) {
                    Some(module) => module.signature_at(def_handle.parameters),
                    None => {
                        return Err(verification_error(
                            StatusCode::LOOKUP_FAILED,
                            IndexKind::FunctionHandle,
                            idx as TableIndex,
                        ))
                    },
                };

                compare_cross_module_signatures(
                    context,
                    &handle_params.0,
                    &def_params.0,
                    owner_module,
                )
                .map_err(|e| e.at_index(IndexKind::FunctionHandle, idx as TableIndex))?;

                // same return_
                let handle_return = context.resolver.signature_at(function_handle.return_);
                let def_return = match context.dependency_map.get(&owner_module_id) {
                    Some(module) => module.signature_at(def_handle.return_),
                    None => {
                        return Err(verification_error(
                            StatusCode::LOOKUP_FAILED,
                            IndexKind::FunctionHandle,
                            idx as TableIndex,
                        ))
                    },
                };

                compare_cross_module_signatures(
                    context,
                    &handle_return.0,
                    &def_return.0,
                    owner_module,
                )
                .map_err(|e| e.at_index(IndexKind::FunctionHandle, idx as TableIndex))?;

                // Compatible attributes.
                let mut def_attrs = def_handle.attributes.as_slice();
                let handle_attrs = function_handle.attributes.as_slice();
                if !handle_attrs.is_empty() && def_attrs.is_empty() {
                    // This is a function with no attributes, which can come from that
                    // it's compiled for < Move 2.2. Synthesize the
                    // `persistent` attribute from Public visibility, which we find
                    // in the definition.
                    if owner_module.function_def_at(*owner_def_idx).visibility == Visibility::Public
                    {
                        def_attrs = &[FunctionAttribute::Persistent]
                    }
                }
                if !FunctionAttribute::is_compatible_with(handle_attrs, def_attrs) {
                    let def_view = FunctionHandleView::new(*owner_module, def_handle);
                    return Err(verification_error(
                        StatusCode::LINKER_ERROR,
                        IndexKind::FunctionHandle,
                        idx as TableIndex,
                    )
                    .with_message(format!(
                        "imported function `{}` missing expected attributes",
                        def_view.name()
                    )));
                }
            },
            None => {
                return Err(verification_error(
                    StatusCode::LOOKUP_FAILED,
                    IndexKind::FunctionHandle,
                    idx as TableIndex,
                ));
            },
        }
    }
    Ok(())
}

// The local view must be a subset of (or equal to) the defined set of abilities. Conceptually, the
// local view can be more constrained than the defined one. Removing abilities locally does nothing
// but limit the local usage.
// (Note this works because there are no negative constraints, i.e. you cannot constrain a type
// parameter with the absence of an ability)
fn compatible_struct_abilities(
    local_struct_abilities_declaration: AbilitySet,
    defined_struct_abilities: AbilitySet,
) -> bool {
    local_struct_abilities_declaration.is_subset(defined_struct_abilities)
}

// - The number of type parameters must be the same
// - Each pair of parameters must satisfy [`compatible_type_parameter_constraints`]
fn compatible_fun_type_parameters(
    local_type_parameters_declaration: &[AbilitySet],
    defined_type_parameters: &[AbilitySet],
) -> bool {
    local_type_parameters_declaration.len() == defined_type_parameters.len()
        && local_type_parameters_declaration
            .iter()
            .zip(defined_type_parameters)
            .all(
                |(
                    local_type_parameter_constraints_declaration,
                    defined_type_parameter_constraints,
                )| {
                    compatible_type_parameter_constraints(
                        *local_type_parameter_constraints_declaration,
                        *defined_type_parameter_constraints,
                    )
                },
            )
}

// - The number of type parameters must be the same
// - Each pair of parameters must satisfy [`compatible_type_parameter_constraints`] and [`compatible_type_parameter_phantom_decl`]
fn compatible_struct_type_parameters(
    local_type_parameters_declaration: &[StructTypeParameter],
    defined_type_parameters: &[StructTypeParameter],
) -> bool {
    local_type_parameters_declaration.len() == defined_type_parameters.len()
        && local_type_parameters_declaration
            .iter()
            .zip(defined_type_parameters)
            .all(
                |(local_type_parameter_declaration, defined_type_parameter)| {
                    compatible_type_parameter_phantom_decl(
                        local_type_parameter_declaration,
                        defined_type_parameter,
                    ) && compatible_type_parameter_constraints(
                        local_type_parameter_declaration.constraints,
                        defined_type_parameter.constraints,
                    )
                },
            )
}

//  The local view of a type parameter must be a superset of (or equal to) the defined
//  constraints. Conceptually, the local view can be more constrained than the defined one as the
//  local context is only limiting usage, and cannot take advantage of the additional constraints.
fn compatible_type_parameter_constraints(
    local_type_parameter_constraints_declaration: AbilitySet,
    defined_type_parameter_constraints: AbilitySet,
) -> bool {
    defined_type_parameter_constraints.is_subset(local_type_parameter_constraints_declaration)
}

// Adding phantom declarations relaxes the requirements for clients, thus, the local view may
// lack a phantom declaration present in the definition.
fn compatible_type_parameter_phantom_decl(
    local_type_parameter_declaration: &StructTypeParameter,
    defined_type_parameter: &StructTypeParameter,
) -> bool {
    // local_type_parameter_declaration.is_phantom => defined_type_parameter.is_phantom
    !local_type_parameter_declaration.is_phantom || defined_type_parameter.is_phantom
}

fn compare_cross_module_signatures(
    context: &Context,
    handle_sig: &[SignatureToken],
    def_sig: &[SignatureToken],
    def_module: &CompiledModule,
) -> PartialVMResult<()> {
    if handle_sig.len() != def_sig.len() {
        return Err(PartialVMError::new(StatusCode::TYPE_MISMATCH));
    }
    for (handle_type, def_type) in handle_sig.iter().zip(def_sig) {
        compare_types(context, handle_type, def_type, def_module)?;
    }
    Ok(())
}

fn compare_types(
    context: &Context,
    handle_type: &SignatureToken,
    def_type: &SignatureToken,
    def_module: &CompiledModule,
) -> PartialVMResult<()> {
    let result = match (handle_type, def_type) {
        (SignatureToken::Bool, SignatureToken::Bool)
        | (SignatureToken::U8, SignatureToken::U8)
        | (SignatureToken::U16, SignatureToken::U16)
        | (SignatureToken::U32, SignatureToken::U32)
        | (SignatureToken::U64, SignatureToken::U64)
        | (SignatureToken::U128, SignatureToken::U128)
        | (SignatureToken::U256, SignatureToken::U256)
        | (SignatureToken::Address, SignatureToken::Address)
        | (SignatureToken::Signer, SignatureToken::Signer) => Ok(()),
        (SignatureToken::Vector(handle_ty), SignatureToken::Vector(def_ty)) => {
            compare_types(context, handle_ty, def_ty, def_module)
        },
        (
            SignatureToken::Function(handle_args, handle_result, handle_ab),
            SignatureToken::Function(def_args, def_result, def_ab),
        ) => {
            compare_cross_module_signatures(context, handle_args, def_args, def_module)?;
            compare_cross_module_signatures(context, handle_result, def_result, def_module)?;
            if handle_ab == def_ab {
                Ok(())
            } else {
                Err(PartialVMError::new(StatusCode::TYPE_MISMATCH))
            }
        },
        (SignatureToken::Struct(idx1), SignatureToken::Struct(idx2)) => {
            compare_structs(context, *idx1, *idx2, def_module)
        },
        (
            SignatureToken::StructInstantiation(idx1, inst1),
            SignatureToken::StructInstantiation(idx2, inst2),
        ) => {
            compare_structs(context, *idx1, *idx2, def_module)?;
            compare_cross_module_signatures(context, inst1, inst2, def_module)
        },
        (SignatureToken::Reference(ty1), SignatureToken::Reference(ty2))
        | (SignatureToken::MutableReference(ty1), SignatureToken::MutableReference(ty2)) => {
            compare_types(context, ty1, ty2, def_module)
        },
        (SignatureToken::TypeParameter(idx1), SignatureToken::TypeParameter(idx2)) => {
            if idx1 != idx2 {
                Err(PartialVMError::new(StatusCode::TYPE_MISMATCH))
            } else {
                Ok(())
            }
        },
        (SignatureToken::Bool, _)
        | (SignatureToken::U8, _)
        | (SignatureToken::U64, _)
        | (SignatureToken::U128, _)
        | (SignatureToken::Address, _)
        | (SignatureToken::Signer, _)
        | (SignatureToken::Vector(_), _)
        | (SignatureToken::Function(..), _)
        | (SignatureToken::Struct(_), _)
        | (SignatureToken::StructInstantiation(_, _), _)
        | (SignatureToken::Reference(_), _)
        | (SignatureToken::MutableReference(_), _)
        | (SignatureToken::TypeParameter(_), _)
        | (SignatureToken::U16, _)
        | (SignatureToken::U32, _)
        | (SignatureToken::U256, _) => Err(PartialVMError::new(StatusCode::TYPE_MISMATCH)),
    };
    result.map_err(|err| {
        if err.message().is_none() {
            err.with_message("imported type mismatches expectation")
        } else {
            err
        }
    })
}

fn compare_structs(
    context: &Context,
    idx1: StructHandleIndex,
    idx2: StructHandleIndex,
    def_module: &CompiledModule,
) -> PartialVMResult<()> {
    // grab ModuleId and struct name for the module being verified
    let struct_handle = context.resolver.struct_handle_at(idx1);
    let module_handle = context.resolver.module_handle_at(struct_handle.module);
    let module_id = context.resolver.module_id_for_handle(module_handle);
    let struct_name = context.resolver.identifier_at(struct_handle.name);

    // grab ModuleId and struct name for the definition
    let def_struct_handle = def_module.struct_handle_at(idx2);
    let def_module_handle = def_module.module_handle_at(def_struct_handle.module);
    let def_module_id = def_module.module_id_for_handle(def_module_handle);
    let def_struct_name = def_module.identifier_at(def_struct_handle.name);

    if module_id != def_module_id || struct_name != def_struct_name {
        Err(PartialVMError::new(StatusCode::TYPE_MISMATCH))
    } else {
        Ok(())
    }
}

fn verify_all_script_visibility_usage(context: &Context) -> PartialVMResult<()> {
    // script visibility deprecated after V5
    let script_functions = match &context.script_functions {
        None => return Ok(()),
        Some(s) => s,
    };
    debug_assert!(context.resolver.version() < VERSION_5);
    match &context.resolver {
        BinaryIndexedView::Module(m) => {
            for (idx, fdef) in m.function_defs().iter().enumerate() {
                let code = match &fdef.code {
                    None => continue,
                    Some(code) => &code.code,
                };
                verify_script_visibility_usage(
                    &context.resolver,
                    script_functions,
                    fdef.is_entry,
                    FunctionDefinitionIndex(idx as TableIndex),
                    code,
                )?
            }
            Ok(())
        },
        BinaryIndexedView::Script(s) => verify_script_visibility_usage(
            &context.resolver,
            script_functions,
            true,
            FunctionDefinitionIndex(0),
            &s.code().code,
        ),
    }
}

fn verify_script_visibility_usage(
    resolver: &BinaryIndexedView,
    script_functions: &BTreeSet<FunctionHandleIndex>,
    current_is_entry: bool,
    fdef_idx: FunctionDefinitionIndex,
    code: &[Bytecode],
) -> PartialVMResult<()> {
    for (idx, instr) in code.iter().enumerate() {
        let idx = idx as CodeOffset;
        let fhandle_idx = match instr {
            Bytecode::Call(fhandle_idx) => fhandle_idx,
            Bytecode::CallGeneric(finst_idx) => {
                &resolver.function_instantiation_at(*finst_idx).handle
            },
            _ => continue,
        };
        match (current_is_entry, script_functions.contains(fhandle_idx)) {
            (true, true) => (),
            (_, true) => {
                return Err(PartialVMError::new(
                    StatusCode::CALLED_SCRIPT_VISIBLE_FROM_NON_SCRIPT_VISIBLE,
                )
                .at_code_offset(fdef_idx, idx)
                .with_message(
                    "script-visible functions can only be called from scripts or other \
                    script-visible functions"
                        .to_string(),
                ));
            },
            _ => (),
        }
    }
    Ok(())
}
