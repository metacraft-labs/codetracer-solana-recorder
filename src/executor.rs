//! SBF VM execution with register tracing.
//!
//! Loads a compiled SBF ELF, executes it through the `solana-sbpf` VM with
//! register tracing enabled, and returns the register trace data in the
//! format expected by `recorder::record_from_traces`.

use std::sync::Arc;

use eyre::{Result, eyre};
use solana_sbpf::{
    ebpf,
    elf::Executable,
    memory_region::{MemoryMapping, MemoryRegion},
    program::BuiltinProgram,
    vm::{Config, ContextObject, EbpfVm, ExecutionMode},
};

use crate::syscalls::{HasSyscallState, SyscallState};

/// Context object for standalone SBF execution.
///
/// Provides:
///   * A compute budget that decrements on each instruction (used by
///     ``ContextObject::consume`` / ``get_remaining``).
///   * The active ``MemoryMapping`` for the running VM -- exposed via
///     ``active_mapping_ptr`` so the interpreter can resolve virtual
///     addresses, and via [`crate::syscalls`] handlers so they can
///     read/write the program's memory.
///   * The recorder's ``SyscallState`` -- captures each syscall the
///     program invokes so the trace surface matches the on-chain
///     ``InvokeContext``.  See [`crate::syscalls`] for the
///     registered handler set.
///
/// The struct is ``pub`` (not ``pub(crate)``) because the
/// ``declare_builtin_function!`` macro lives in the
/// ``codetracer_solana_recorder::syscalls`` module and the macro's
/// generated impl block needs name resolution on the concrete type.
pub struct RecorderContext {
    remaining: u64,
    pub memory_mapping: MemoryMapping,
    syscall_state: SyscallState,
}

impl RecorderContext {
    fn new(compute_budget: u64, config: &Config, heap_start: u64, heap_len: u64) -> Self {
        let memory_mapping =
            MemoryMapping::new(vec![], config, solana_sbpf::program::SBPFVersion::Reserved)
                .expect("empty memory mapping should not fail");
        Self {
            remaining: compute_budget,
            memory_mapping,
            syscall_state: SyscallState::new(heap_start, heap_len),
        }
    }

    /// Public constructor used by [`crate::syscalls`] unit tests so
    /// they can drive the VM through the same setup the executor
    /// performs without going through ``execute_with_tracing`` (which
    /// expects an ELF; the syscall tests use assembled SBPF directly).
    #[doc(hidden)]
    pub fn new_for_tests(
        compute_budget: u64,
        config: &Config,
        heap_start: u64,
        heap_len: u64,
    ) -> Self {
        Self::new(compute_budget, config, heap_start, heap_len)
    }
}

impl ContextObject for RecorderContext {
    fn consume(&mut self, amount: u64) {
        self.remaining = self.remaining.saturating_sub(amount);
    }

    fn get_remaining(&self) -> u64 {
        self.remaining
    }

    fn active_mapping_ptr(&mut self) -> std::ptr::NonNull<MemoryMapping> {
        std::ptr::NonNull::from(&mut self.memory_mapping)
    }
}

impl HasSyscallState for RecorderContext {
    fn syscall_state(&self) -> &SyscallState {
        &self.syscall_state
    }
}

/// Execute a compiled SBF ELF file with register tracing enabled.
///
/// Returns the raw register trace data (binary format: 96 bytes per row,
/// 12 × u64 little-endian) that can be fed to `record_from_traces`.
///
/// # Arguments
///
/// * `elf_data` - Raw bytes of the compiled SBF ELF (.so) file
/// * `compute_budget` - Maximum compute units for execution
///
/// # Returns
///
/// The register trace as raw bytes (each row = 12 × u64 = 96 bytes).
pub fn execute_with_tracing(elf_data: &[u8], compute_budget: u64) -> Result<Vec<u8>> {
    let config = Config {
        enable_register_tracing: true,
        enable_instruction_meter: true,
        ..Config::default()
    };

    // Loader with the recorder's Solana syscall set registered.  See
    // [`crate::syscalls`] for the canonical list (sol_log_, sol_panic_,
    // sol_memcpy_, sol_alloc_free_ etc.).  Without these handlers a
    // program's first ``CALL_IMM <sol_syscall>`` aborts the VM with
    // ``EbpfError::UnsupportedInstruction`` -- which is what blocked
    // the cross-repo WDIO smoke test at run 27546133542: the entrypoint
    // !() macro calls ``msg!("result: {}", ..)`` which lowers to
    // ``sol_log_``, the unregistered hash aborted the VM before
    // ``process_instruction``'s ``let sum_val = a + b;`` line ever
    // executed, and the smoke test's ``finds sum_val in local
    // variables`` assertion failed because no PC ever mapped to
    // ``solana_flow_test.rs:27``.
    let mut loader = BuiltinProgram::<RecorderContext>::new_loader(config.clone());
    loader = crate::syscalls::register(loader)
        .map_err(|e| eyre!("failed to register syscalls: {:?}", e))?;
    let loader = Arc::new(loader);

    // Load and verify the ELF executable.
    let executable = Executable::<RecorderContext>::load(elf_data, loader.clone())
        .map_err(|e| eyre!("failed to load ELF: {:?}", e))?;

    executable
        .verify::<solana_sbpf::verifier::RequisiteVerifier>()
        .map_err(|e| eyre!("ELF verification failed: {:?}", e))?;

    // Set up stack, heap, and input memory regions.
    let stack_size = config.stack_size();
    let mut stack = vec![0u8; stack_size];
    let heap_size = 32 * 1024; // 32 KB heap
    let mut heap = vec![0u8; heap_size];

    // Solana SBF programs compiled via ``entrypoint!`` expect ``r1`` to
    // point at a serialized input region (per the BPFLoader ABI):
    //
    //   | num_accounts: u64 (LE)        |
    //   | (account_metas + data...)     |
    //   | instruction_data_len: u64 (LE)|
    //   | instruction_data (bytes)      |
    //   | program_id: 32 bytes          |
    //
    // Without that, the entrypoint's first ``LD`` from ``[r1+0]`` hits
    // an ``AccessViolation(Load, 0, 8, "unknown")`` at address 0 and
    // execution aborts after ~7 instructions (observed against
    // ``test_programs.so`` in the cross-repo smoke test).  Provide a
    // minimal valid input -- zero accounts, zero instruction data,
    // all-zero program id, total 48 bytes -- so the entrypoint
    // deserialises cleanly and reaches the body of
    // ``process_instruction``.
    let mut input = vec![0u8; 48]; // num=0, data_len=0, program_id=32 zero bytes

    let sbpf_version = executable.get_sbpf_version();
    let regions = vec![
        MemoryRegion::new_writable(&mut stack, ebpf::MM_STACK_START),
        MemoryRegion::new_writable(&mut heap, ebpf::MM_HEAP_START),
        MemoryRegion::new_writable(&mut input, ebpf::MM_INPUT_START),
    ];

    let mut context = RecorderContext::new(
        compute_budget,
        &config,
        ebpf::MM_HEAP_START,
        heap_size as u64,
    );
    context.memory_mapping = MemoryMapping::new(regions, &config, sbpf_version)
        .map_err(|e| eyre!("failed to create memory mapping: {:?}", e))?;

    // Create and execute the VM.
    let mut vm = EbpfVm::new(loader, sbpf_version, &mut context, stack_size);
    // Point r1 at the input region per Solana entrypoint ABI.
    vm.registers[1] = ebpf::MM_INPUT_START;

    let mut mode = ExecutionMode::Interpreted;
    let mut call_frames = vec![solana_sbpf::vm::CallFrame::default(); config.max_call_depth];
    let (_insn_count, result) = vm.execute_program(&executable, &mut mode, &mut call_frames);

    eprintln!("Execution result: {:?}", result);
    eprintln!(
        "Register trace: {} entries ({} bytes)",
        vm.register_trace.len(),
        vm.register_trace.len() * 96
    );

    // Convert register trace entries to raw bytes.
    // Each entry is [u64; 12] = 96 bytes (little-endian).
    let mut regs_data = Vec::with_capacity(vm.register_trace.len() * 96);
    for entry in &vm.register_trace {
        for &reg_val in entry.iter() {
            regs_data.extend_from_slice(&reg_val.to_le_bytes());
        }
    }

    Ok(regs_data)
}
