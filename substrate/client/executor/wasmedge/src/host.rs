use crate::util;
use codec::{Decode, Encode};
use log::trace;
use sc_allocator::{AllocationStats, FreeingBumpHeapAllocator};
use sc_executor_common::{
	error::{Result, WasmError},
	sandbox::{self, SupervisorFuncIndex},
	util::MemoryTransfer,
};
use sp_sandbox::env as sandbox_env;
use sp_wasm_interface::{FunctionContext, MemoryId, Pointer, Sandbox, WordSize};
use std::sync::Arc;
use wasmedge_sdk::{types::Val, Executor, FuncRef, Memory, Table, WasmValue};

// The sandbox store is inside of a Option<Box<..>>> so that we can temporarily borrow it.
struct SandboxStore(Option<Box<sandbox::Store<Arc<FuncRef>>>>);

// There are a bunch of `Rc`s within the sandbox store, however we only manipulate
// those within one thread so this should be safe.
unsafe impl Send for SandboxStore {}

/// The state required to construct a InstanceWrapper context. The context only lasts for one host
/// call, whereas the state is maintained for the duration of a Wasm runtime call, which may make
/// many different host calls that must share state.
pub struct HostState {
	sandbox_store: SandboxStore,
	allocator: Box<FreeingBumpHeapAllocator>,
	panic_message: Option<String>,
}

impl HostState {
	/// Constructs a new `HostState`.
	pub fn new(allocator: FreeingBumpHeapAllocator) -> Self {
		HostState {
			sandbox_store: SandboxStore(Some(Box::new(sandbox::Store::new(
				sandbox::SandboxBackend::TryWasmer,
			)))),
			allocator: Box::new(allocator),
			panic_message: None,
		}
	}

	/// Takes the error message out of the host state, leaving a `None` in its place.
	pub fn take_panic_message(&mut self) -> Option<String> {
		self.panic_message.take()
	}

	pub(crate) fn allocation_stats(&self) -> AllocationStats {
		self.allocator.stats()
	}

	pub fn allocator(&mut self) -> &mut FreeingBumpHeapAllocator {
		self.allocator.as_mut()
	}
}

/// A `HostContext` implements `FunctionContext` for making host calls from a WasmEdge
/// runtime. The `HostContext` exists only for the lifetime of the call and borrows state from
/// a longer-living `HostState`.
pub(crate) struct HostContext<'a> {
	memory: Memory,
	table: Option<Table>,
	host_state: &'a mut HostState,
}

impl<'a> HostContext<'a> {
	pub fn new(memory: Memory, table: Option<Table>, host_state: &mut HostState) -> HostContext {
		HostContext { memory, table, host_state }
	}

	fn sandbox_store(&self) -> &sandbox::Store<Arc<FuncRef>> {
		self.host_state
			.sandbox_store
			.0
			.as_ref()
			.expect("sandbox store is only empty when temporarily borrowed")
	}

	fn sandbox_store_mut(&mut self) -> &mut sandbox::Store<Arc<FuncRef>> {
		self.host_state
			.sandbox_store
			.0
			.as_mut()
			.expect("sandbox store is only empty when temporarily borrowed")
	}
}

impl<'a> sp_wasm_interface::FunctionContext for HostContext<'a> {
	fn read_memory_into(
		&self,
		address: Pointer<u8>,
		dest: &mut [u8],
	) -> sp_wasm_interface::Result<()> {
		util::read_memory_into(util::memory_slice(&self.memory), address, dest)
			.map_err(|e| e.to_string())
	}

	fn write_memory(&mut self, address: Pointer<u8>, data: &[u8]) -> sp_wasm_interface::Result<()> {
		util::write_memory_from(util::memory_slice_mut(&mut self.memory), address, data)
			.map_err(|e| e.to_string())
	}

	fn allocate_memory(&mut self, size: WordSize) -> sp_wasm_interface::Result<Pointer<u8>> {
		let memory_slice = unsafe {
			std::slice::from_raw_parts_mut(
				self.memory
					.data_pointer_mut(0, 1)
					.expect("failed to returns the mut data pointer to the Memory."),
				(self.memory.size() * 64 * 1024) as usize,
			)
		};

		self.host_state
			.allocator()
			.allocate(memory_slice, size)
			.map_err(|e| e.to_string())
	}

	fn deallocate_memory(&mut self, ptr: Pointer<u8>) -> sp_wasm_interface::Result<()> {
		let memory_slice = unsafe {
			std::slice::from_raw_parts_mut(
				self.memory
					.data_pointer_mut(0, 1)
					.expect("failed to returns the mut data pointer to the Memory."),
				(self.memory.size() * 64 * 1024) as usize,
			)
		};

		self.host_state
			.allocator()
			.deallocate(memory_slice, ptr)
			.map_err(|e| e.to_string())
	}

	fn sandbox(&mut self) -> &mut dyn Sandbox {
		self
	}

	fn register_panic_error_message(&mut self, message: &str) {
		self.host_state.panic_message = Some(message.to_owned());
	}
}

impl<'a> Sandbox for HostContext<'a> {
	fn memory_get(
		&mut self,
		memory_id: MemoryId,
		offset: WordSize,
		buf_ptr: Pointer<u8>,
		buf_len: WordSize,
	) -> sp_wasm_interface::Result<u32> {
		let sandboxed_memory = self.sandbox_store().memory(memory_id).map_err(|e| e.to_string())?;

		let len = buf_len as usize;

		let buffer = match sandboxed_memory.read(Pointer::new(offset as u32), len) {
			Err(_) => return Ok(sandbox_env::ERR_OUT_OF_BOUNDS),
			Ok(buffer) => buffer,
		};

		if util::write_memory_from(util::memory_slice_mut(&mut self.memory), buf_ptr, &buffer)
			.is_err()
		{
			return Ok(sandbox_env::ERR_OUT_OF_BOUNDS)
		}

		Ok(sandbox_env::ERR_OK)
	}

	fn memory_set(
		&mut self,
		memory_id: MemoryId,
		offset: WordSize,
		val_ptr: Pointer<u8>,
		val_len: WordSize,
	) -> sp_wasm_interface::Result<u32> {
		let sandboxed_memory = self.sandbox_store().memory(memory_id).map_err(|e| e.to_string())?;

		let len = val_len as usize;

		let buffer = match util::read_memory(util::memory_slice(&self.memory), val_ptr, len) {
			Err(_) => return Ok(sandbox_env::ERR_OUT_OF_BOUNDS),
			Ok(buffer) => buffer,
		};

		if sandboxed_memory.write_from(Pointer::new(offset as u32), &buffer).is_err() {
			return Ok(sandbox_env::ERR_OUT_OF_BOUNDS)
		}

		Ok(sandbox_env::ERR_OK)
	}

	fn memory_teardown(&mut self, memory_id: MemoryId) -> sp_wasm_interface::Result<()> {
		self.sandbox_store_mut().memory_teardown(memory_id).map_err(|e| e.to_string())
	}

	fn memory_new(&mut self, initial: u32, maximum: u32) -> sp_wasm_interface::Result<u32> {
		self.sandbox_store_mut().new_memory(initial, maximum).map_err(|e| e.to_string())
	}

	fn invoke(
		&mut self,
		instance_id: u32,
		export_name: &str,
		mut args: &[u8],
		return_val: Pointer<u8>,
		return_val_len: u32,
		state: u32,
	) -> sp_wasm_interface::Result<u32> {
		trace!(target: "sp-sandbox", "invoke, instance_idx={}", instance_id);

		// Deserialize arguments and convert them into wasmi types.
		let args = Vec::<sp_wasm_interface::Value>::decode(&mut args)
			.map_err(|_| "Can't decode serialized arguments for the invocation")?
			.into_iter()
			.collect::<Vec<_>>();

		let instance = self.sandbox_store().instance(instance_id).map_err(|e| e.to_string())?;

		let dispatch_thunk =
			self.sandbox_store().dispatch_thunk(instance_id).map_err(|e| e.to_string())?;

		let result = instance.invoke(
			export_name,
			&args,
			state,
			&mut SandboxContext { host_context: self, dispatch_thunk },
		);

		match result {
			Ok(None) => Ok(sandbox_env::ERR_OK),
			Ok(Some(val)) => {
				// Serialize return value and write it back into the memory.
				sp_wasm_interface::ReturnValue::Value(val.into()).using_encoded(|val| {
					if val.len() > return_val_len as usize {
						return Err("Return value buffer is too small".into())
					}
					<HostContext as FunctionContext>::write_memory(self, return_val, val)
						.map_err(|_| "can't write return value")?;
					Ok(sandbox_env::ERR_OK)
				})
			},
			Err(_) => Ok(sandbox_env::ERR_EXECUTION),
		}
	}

	fn instance_teardown(&mut self, instance_id: u32) -> sp_wasm_interface::Result<()> {
		self.sandbox_store_mut()
			.instance_teardown(instance_id)
			.map_err(|e| e.to_string())
	}

	fn instance_new(
		&mut self,
		dispatch_thunk_id: u32,
		wasm: &[u8],
		raw_env_def: &[u8],
		state: u32,
	) -> sp_wasm_interface::Result<u32> {
		// Extract a dispatch thunk from the instance's table by the specified index.
		let dispatch_thunk = Arc::new({
			match self
				.table
				.as_ref()
				.ok_or("Runtime doesn't have a table; sandbox is unavailable")?
				.get(dispatch_thunk_id)
				.map_err(|_| "dispatch_thunk_id is out of bounds")?
			{
				Val::FuncRef(Some(func_ref)) => func_ref,
				_ => return Err(String::from("dispatch_thunk_id should point to actual func")),
			}
		});

		let guest_env = match sandbox::GuestEnvironment::decode(self.sandbox_store(), raw_env_def) {
			Ok(guest_env) => guest_env,
			Err(_) => return Ok(sandbox_env::ERR_MODULE as u32),
		};

		let mut store = self
			.host_state
			.sandbox_store
			.0
			.take()
			.expect("sandbox store is only empty when borrowed");

		// Catch any potential panics so that we can properly restore the sandbox store
		// which we've destructively borrowed.
		let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
			store.instantiate(
				wasm,
				guest_env,
				state,
				&mut SandboxContext { host_context: self, dispatch_thunk: dispatch_thunk.clone() },
			)
		}));

		self.host_state.sandbox_store.0 = Some(store);

		let result = match result {
			Ok(result) => result,
			Err(error) => std::panic::resume_unwind(error),
		};

		let instance_idx_or_err_code = match result {
			Ok(instance) => instance.register(self.sandbox_store_mut(), dispatch_thunk.clone()),
			Err(sandbox::InstantiationError::StartTrapped) => sandbox_env::ERR_EXECUTION,
			Err(_) => sandbox_env::ERR_MODULE,
		};

		Ok(instance_idx_or_err_code as u32)
	}

	fn get_global_val(
		&self,
		instance_idx: u32,
		name: &str,
	) -> sp_wasm_interface::Result<Option<sp_wasm_interface::Value>> {
		self.sandbox_store()
			.instance(instance_idx)
			.map(|i| i.get_global_val(name))
			.map_err(|e| e.to_string())
	}
}

struct SandboxContext<'a, 'b> {
	host_context: &'a mut HostContext<'b>,
	dispatch_thunk: Arc<FuncRef>,
}

impl<'a, 'b> sandbox::SandboxContext for SandboxContext<'a, 'b> {
	fn invoke(
		&mut self,
		invoke_args_ptr: Pointer<u8>,
		invoke_args_len: WordSize,
		state: u32,
		func_idx: SupervisorFuncIndex,
	) -> Result<i64> {
		let mut executor = Executor::new(None, None).map_err(|e| {
			WasmError::Other(format!("fail to create a WasmEdge Executor context: {}", e))
		})?;

		let result = self.dispatch_thunk.call(
			&mut executor,
			vec![
				WasmValue::from_i32(u32::from(invoke_args_ptr) as i32),
				WasmValue::from_i32(invoke_args_len as i32),
				WasmValue::from_i32(state as i32),
				WasmValue::from_i32(usize::from(func_idx) as i32),
			],
		);

		match result {
			Ok(result) => Ok(result[0].to_i64()),
			Err(err) => Err(err.to_string().into()),
		}
	}

	fn supervisor_context(&mut self) -> &mut dyn FunctionContext {
		self.host_context
	}
}
