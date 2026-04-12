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

/// Minimal context object for standalone SBF execution.
///
/// Provides a compute budget and memory mapping. Does not implement
/// Solana runtime features (accounts, syscalls) — suitable for pure
/// computational programs.
struct RecorderContext {
    remaining: u64,
    memory_mapping: MemoryMapping,
}

impl RecorderContext {
    fn new(compute_budget: u64, config: &Config) -> Self {
        let memory_mapping =
            MemoryMapping::new(vec![], config, solana_sbpf::program::SBPFVersion::Reserved)
                .expect("empty memory mapping should not fail");
        Self {
            remaining: compute_budget,
            memory_mapping,
        }
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

    // Create a minimal loader. For programs that reference Solana syscalls,
    // ELF loading may fail with unresolved symbols. That's expected — those
    // programs need the full Solana runtime (Mollusk/LiteSVM) instead.
    let loader = Arc::new(BuiltinProgram::<RecorderContext>::new_loader(config.clone()));

    // Load and verify the ELF executable.
    let executable = Executable::<RecorderContext>::load(elf_data, loader.clone())
        .map_err(|e| eyre!("failed to load ELF: {:?}", e))?;

    executable
        .verify::<solana_sbpf::verifier::RequisiteVerifier>()
        .map_err(|e| eyre!("ELF verification failed: {:?}", e))?;

    // Set up stack and heap memory regions.
    let stack_size = config.stack_size();
    let mut stack = vec![0u8; stack_size];
    let heap_size = 32 * 1024; // 32 KB heap
    let mut heap = vec![0u8; heap_size];

    let sbpf_version = executable.get_sbpf_version().clone();
    let regions = vec![
        MemoryRegion::new_writable(&mut stack, ebpf::MM_STACK_START),
        MemoryRegion::new_writable(&mut heap, ebpf::MM_HEAP_START),
    ];

    let mut context = RecorderContext::new(compute_budget, &config);
    context.memory_mapping = MemoryMapping::new(regions, &config, sbpf_version)
        .map_err(|e| eyre!("failed to create memory mapping: {:?}", e))?;

    // Create and execute the VM.
    let mut vm = EbpfVm::new(loader, sbpf_version, &mut context, stack_size);

    let mut mode = ExecutionMode::Interpreted;
    let mut call_frames =
        vec![solana_sbpf::vm::CallFrame::default(); config.max_call_depth];
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
