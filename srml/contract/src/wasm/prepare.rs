// Copyright 2018-2019 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate. If not, see <http://www.gnu.org/licenses/>.

//! This module takes care of loading, checking and preprocessing of a
//! wasm module before execution. It also extracts some essential information
//! from a module.

use crate::wasm::env_def::ImportSatisfyCheck;
use crate::wasm::PrefabWasmModule;
use crate::{Schedule, Trait};

use parity_wasm::elements::{self, Internal, External, MemoryType, Type};
use pwasm_utils;
use pwasm_utils::rules;
use rstd::prelude::*;
use runtime_primitives::traits::As;

struct ContractModule<'a, Gas: 'a> {
	// An `Option` is used here for loaning (`take()`-ing) the module.
	// Invariant: Can't be `None` (i.e. on enter and on exit from the function
	// the value *must* be `Some`).
	module: Option<elements::Module>,
	schedule: &'a Schedule<Gas>,
}

impl<'a, Gas: 'a + As<u32> + Clone> ContractModule<'a, Gas> {
	fn new(
		original_code: &[u8],
		schedule: &'a Schedule<Gas>,
	) -> Result<ContractModule<'a, Gas>, &'static str> {
		let module =
			elements::deserialize_buffer(original_code).map_err(|_| "can't decode wasm code")?;
		Ok(ContractModule {
			module: Some(module),
			schedule,
		})
	}

	/// Ensures that module doesn't declare internal memories.
	///
	/// In this runtime we only allow wasm module to import memory from the environment.
	/// Memory section contains declarations of internal linear memories, so if we find one
	/// we reject such a module.
	fn ensure_no_internal_memory(&self) -> Result<(), &'static str> {
		let module = self
			.module
			.as_ref()
			.expect("On entry to the function `module` can't be None; qed");
		if module
			.memory_section()
			.map_or(false, |ms| ms.entries().len() > 0)
		{
			return Err("module declares internal memory");
		}
		Ok(())
	}

	fn inject_gas_metering(&mut self) -> Result<(), &'static str> {
		let gas_rules =
			rules::Set::new(
				self.schedule.regular_op_cost.clone().as_(),
				Default::default(),
			)
			.with_grow_cost(self.schedule.grow_mem_cost.clone().as_())
			.with_forbidden_floats();

		let module = self
			.module
			.take()
			.expect("On entry to the function `module` can't be `None`; qed");

		let contract_module = pwasm_utils::inject_gas_counter(module, &gas_rules)
			.map_err(|_| "gas instrumentation failed")?;

		self.module = Some(contract_module);
		Ok(())
	}

	fn inject_stack_height_metering(&mut self) -> Result<(), &'static str> {
		let module = self
			.module
			.take()
			.expect("On entry to the function `module` can't be `None`; qed");

		let contract_module =
			pwasm_utils::stack_height::inject_limiter(module, self.schedule.max_stack_height)
				.map_err(|_| "stack height instrumentation failed")?;

		self.module = Some(contract_module);
		Ok(())
	}

	/// Check that the module has required exported functions. For now
	/// these are just entrypoints:
	///
	/// - 'call'
	/// - 'deploy'
	fn scan_exports(&self) -> Result<(), &'static str> {
		let mut deploy_found = false;
		let mut call_found = false;

		let module = self
			.module
			.as_ref()
			.expect("On entry to the function `module` can't be `None`; qed");

		let types = module.type_section().map(|ts| ts.types()).unwrap_or(&[]);
		let export_entries = module
			.export_section()
			.map(|is| is.entries())
			.unwrap_or(&[]);
		let func_entries = module
			.function_section()
			.map(|fs| fs.entries())
			.unwrap_or(&[]);

		// Function index space consists of imported function following by
		// declared functions. Calculate the total number of imported functions so
		// we can use it to convert indexes from function space to declared function space.
		let fn_space_offset = module
			.import_section()
			.map(|is| is.entries())
			.unwrap_or(&[])
			.iter()
			.filter(|entry| {
				match *entry.external() {
					External::Function(_) => true,
					_ => false,
				}
			})
			.count();

		for export in export_entries {
			match export.field() {
				"call" => call_found = true,
				"deploy" => deploy_found = true,
				_ => continue,
			}

			// Then check the export kind. "call" and "deploy" are
			// functions.
			let fn_idx = match export.internal() {
				Internal::Function(ref fn_idx) => *fn_idx,
				_ => return Err("expected a function"),
			};

			// convert index from function index space to declared index space.
			let fn_idx = match fn_idx.checked_sub(fn_space_offset as u32) {
				Some(fn_idx) => fn_idx,
				None => {
					// Underflow here means fn_idx points to imported function which we don't allow!
					return Err("entry point points to an imported function");
				}
			};

			// Then check the signature.
			// Both "call" and "deploy" has a () -> () function type.
			let func_ty_idx = func_entries.get(fn_idx as usize)
				.ok_or_else(|| "export refers to non-existent function")?
				.type_ref();
			let Type::Function(ref func_ty) = types
				.get(func_ty_idx as usize)
				.ok_or_else(|| "function has a non-existent type")?;
			if !(func_ty.params().is_empty() && func_ty.return_type().is_none()) {
				return Err("entry point has wrong signature");
			}
		}

		if !deploy_found {
			return Err("deploy function isn't exported");
		}
		if !call_found {
			return Err("call function isn't exported");
		}

		Ok(())
	}

	/// Scan an import section if any.
	///
	/// This accomplishes two tasks:
	///
	/// - checks any imported function against defined host functions set, incl.
	///   their signatures.
	/// - if there is a memory import, returns it's descriptor
	fn scan_imports<C: ImportSatisfyCheck>(&self) -> Result<Option<&MemoryType>, &'static str> {
		let module = self
			.module
			.as_ref()
			.expect("On entry to the function `module` can't be `None`; qed");

		let types = module.type_section().map(|ts| ts.types()).unwrap_or(&[]);
		let import_entries = module
			.import_section()
			.map(|is| is.entries())
			.unwrap_or(&[]);

		let mut imported_mem_type = None;

		for import in import_entries {
			if import.module() != "env" {
				// This import tries to import something from non-"env" module,
				// but all imports are located in "env" at the moment.
				return Err("module has imports from a non-'env' namespace");
			}

			let type_idx = match import.external() {
				&External::Function(ref type_idx) => type_idx,
				&External::Memory(ref memory_type) => {
					imported_mem_type = Some(memory_type);
					continue;
				}
				_ => continue,
			};

			let Type::Function(ref func_ty) = types
				.get(*type_idx as usize)
				.ok_or_else(|| "validation: import entry points to a non-existent type")?;

			// We disallow importing `ext_println` unless debug features are enabled,
			// which should only be allowed on a dev chain
			if !self.schedule.enable_println && import.field().as_bytes() == b"ext_println" {
				return Err("module imports `ext_println` but debug features disabled");
			}

			// We disallow importing `gas` function here since it is treated as implementation detail.
			if import.field().as_bytes() == b"gas"
				|| !C::can_satisfy(import.field().as_bytes(), func_ty)
			{
				return Err("module imports a non-existent function");
			}
		}
		Ok(imported_mem_type)
	}

	fn into_wasm_code(mut self) -> Result<Vec<u8>, &'static str> {
		elements::serialize(
			self.module
				.take()
				.expect("On entry to the function `module` can't be `None`; qed"),
		)
		.map_err(|_| "error serializing instrumented module")
	}
}

/// Loads the given module given in `original_code`, performs some checks on it and
/// does some preprocessing.
///
/// The checks are:
///
/// - module doesn't define an internal memory instance,
/// - imported memory (if any) doesn't reserve more memory than permitted by the `schedule`,
/// - all imported functions from the external environment matches defined by `env` module,
///
/// The preprocessing includes injecting code for gas metering and metering the height of stack.
pub fn prepare_contract<T: Trait, C: ImportSatisfyCheck>(
	original_code: &[u8],
	schedule: &Schedule<T::Gas>,
) -> Result<PrefabWasmModule, &'static str> {
	let mut contract_module = ContractModule::new(original_code, schedule)?;
	contract_module.scan_exports()?;
	contract_module.ensure_no_internal_memory()?;

	struct MemoryDefinition {
		initial: u32,
		maximum: u32,
	}

	let memory_def = if let Some(memory_type) = contract_module.scan_imports::<C>()? {
		// Inspect the module to extract the initial and maximum page count.
		let limits = memory_type.limits();
		match (limits.initial(), limits.maximum()) {
			(initial, Some(maximum)) if initial > maximum => {
				return Err(
					"Requested initial number of pages should not exceed the requested maximum",
				);
			}
			(_, Some(maximum)) if maximum > schedule.max_memory_pages => {
				return Err("Maximum number of pages should not exceed the configured maximum.");
			}
			(initial, Some(maximum)) => MemoryDefinition { initial, maximum },
			(_, None) => {
				// Maximum number of pages should be always declared.
				// This isn't a hard requirement and can be treated as a maxiumum set
				// to configured maximum.
				return Err("Maximum number of pages should be always declared.");
			}
		}
	} else {
		// If none memory imported then just crate an empty placeholder.
		// Any access to it will lead to out of bounds trap.
		MemoryDefinition {
			initial: 0,
			maximum: 0,
		}
	};

	contract_module.inject_gas_metering()?;
	contract_module.inject_stack_height_metering()?;

	Ok(PrefabWasmModule {
		schedule_version: schedule.version,
		initial: memory_def.initial,
		maximum: memory_def.maximum,
		_reserved: None,
		code: contract_module.into_wasm_code()?,
	})
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::tests::Test;
	use crate::exec::Ext;
	use std::fmt;
	use wabt;
	use assert_matches::assert_matches;

	impl fmt::Debug for PrefabWasmModule {
		fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
			write!(f, "PreparedContract {{ .. }}")
		}
	}

	// Define test environment for tests. We need ImportSatisfyCheck
	// implementation from it. So actual implementations doesn't matter.
	define_env!(TestEnv, <E: Ext>,
		panic(_ctx) => { unreachable!(); },

		// gas is an implementation defined function and a contract can't import it.
		gas(_ctx, _amount: u32) => { unreachable!(); },

		nop(_ctx, _unused: u64) => { unreachable!(); },

		ext_println(_ctx, _ptr: u32, _len: u32) => { unreachable!(); },
	);

	macro_rules! prepare_test {
		($name:ident, $wat:expr, $($expected:tt)*) => {
			#[test]
			fn $name() {
				let wasm = wabt::Wat2Wasm::new().validate(false).convert($wat).unwrap();
				let schedule = Schedule::<u64>::default();
				let r = prepare_contract::<Test, TestEnv>(wasm.as_ref(), &schedule);
				assert_matches!(r, $($expected)*);
			}
		};
	}

	prepare_test!(no_floats,
		r#"
		(module
			(func (export "call")
				(drop
					(f32.add
						(f32.const 0)
						(f32.const 1)
					)
				)
			)
			(func (export "deploy"))
		)"#,
		Err("gas instrumentation failed")
	);

	mod memories {
		use super::*;

		// Tests below assumes that maximum page number is configured to a certain number.
		#[test]
		fn assume_memory_size() {
			assert_eq!(Schedule::<u64>::default().max_memory_pages, 16);
		}

		prepare_test!(memory_with_one_page,
			r#"
			(module
				(import "env" "memory" (memory 1 1))

				(func (export "call"))
				(func (export "deploy"))
			)
			"#,
			Ok(_)
		);

		prepare_test!(internal_memory_declaration,
			r#"
			(module
				(memory 1 1)

				(func (export "call"))
				(func (export "deploy"))
			)
			"#,
			Err("module declares internal memory")
		);

		prepare_test!(no_memory_import,
			r#"
			(module
				;; no memory imported

				(func (export "call"))
				(func (export "deploy"))
			)"#,
			Ok(_)
		);

		prepare_test!(initial_exceeds_maximum,
			r#"
			(module
				(import "env" "memory" (memory 16 1))

				(func (export "call"))
				(func (export "deploy"))
			)
			"#,
			Err("Requested initial number of pages should not exceed the requested maximum")
		);

		prepare_test!(no_maximum,
			r#"
			(module
				(import "env" "memory" (memory 1))

				(func (export "call"))
				(func (export "deploy"))
			)
			"#,
			Err("Maximum number of pages should be always declared.")
		);

		prepare_test!(requested_maximum_exceeds_configured_maximum,
			r#"
			(module
				(import "env" "memory" (memory 1 17))

				(func (export "call"))
				(func (export "deploy"))
			)
			"#,
			Err("Maximum number of pages should not exceed the configured maximum.")
		);
	}

	mod imports {
		use super::*;

		prepare_test!(can_import_legit_function,
			r#"
			(module
				(import "env" "nop" (func (param i64)))

				(func (export "call"))
				(func (export "deploy"))
			)
			"#,
			Ok(_)
		);

		// even though gas is defined the contract can't import it since
		// it is an implementation defined.
		prepare_test!(can_not_import_gas_function,
			r#"
			(module
				(import "env" "gas" (func (param i32)))

				(func (export "call"))
				(func (export "deploy"))
			)
			"#,
			Err("module imports a non-existent function")
		);

		// nothing can be imported from non-"env" module for now.
		prepare_test!(non_env_import,
			r#"
			(module
				(import "another_module" "memory" (memory 1 1))

				(func (export "call"))
				(func (export "deploy"))
			)
			"#,
			Err("module has imports from a non-'env' namespace")
		);

		// wrong signature
		prepare_test!(wrong_signature,
			r#"
			(module
				(import "env" "gas" (func (param i64)))

				(func (export "call"))
				(func (export "deploy"))
			)
			"#,
			Err("module imports a non-existent function")
		);

		prepare_test!(unknown_func_name,
			r#"
			(module
				(import "env" "unknown_func" (func))

				(func (export "call"))
				(func (export "deploy"))
			)
			"#,
			Err("module imports a non-existent function")
		);

		prepare_test!(ext_println_debug_disabled,
			r#"
			(module
				(import "env" "ext_println" (func $ext_println (param i32 i32)))

				(func (export "call"))
				(func (export "deploy"))
			)
			"#,
			Err("module imports `ext_println` but debug features disabled")
		);

		#[test]
		fn ext_println_debug_enabled() {
			let wasm = wabt::Wat2Wasm::new().validate(false).convert(
				r#"
				(module
					(import "env" "ext_println" (func $ext_println (param i32 i32)))

					(func (export "call"))
					(func (export "deploy"))
				)
				"#
			).unwrap();
			let mut schedule = Schedule::<u64>::default();
			schedule.enable_println = true;
			let r = prepare_contract::<Test, TestEnv>(wasm.as_ref(), &schedule);
			assert_matches!(r, Ok(_));
		}
	}

	mod entrypoints {
		use super::*;

		prepare_test!(it_works,
			r#"
			(module
				(func (export "call"))
				(func (export "deploy"))
			)
			"#,
			Ok(_)
		);

		prepare_test!(omit_deploy,
			r#"
			(module
				(func (export "call"))
			)
			"#,
			Err("deploy function isn't exported")
		);

		prepare_test!(omit_call,
			r#"
			(module
				(func (export "deploy"))
			)
			"#,
			Err("call function isn't exported")
		);

		// Try to use imported function as an entry point.
		prepare_test!(try_sneak_export_as_entrypoint,
			r#"
			(module
				(import "env" "panic" (func))

				(func (export "deploy"))

				(export "call" (func 0))
			)
			"#,
			Err("entry point points to an imported function")
		);

		// Try to use imported function as an entry point.
		prepare_test!(try_sneak_export_as_global,
			r#"
			(module
				(func (export "deploy"))
				(global (export "call") i32 (i32.const 0))
			)
			"#,
			Err("expected a function")
		);

		prepare_test!(wrong_signature,
			r#"
			(module
				(func (export "deploy"))
				(func (export "call") (param i32))
			)
			"#,
			Err("entry point has wrong signature")
		);
	}
}
