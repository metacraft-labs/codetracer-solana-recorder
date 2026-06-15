//! Solana on-chain syscall handlers for the standalone recorder VM.
//!
//! The on-chain Solana runtime (agave/programs/bpf_loader/src/syscalls/)
//! exposes a fixed set of syscalls -- ``sol_log_``, ``sol_panic_``,
//! ``sol_memcpy_`` and friends -- that any compiled SBF program may
//! invoke via ``CALL_IMM <hash>``.  The recorder runs the program in a
//! standalone ``solana_sbpf::vm::EbpfVm`` outside the on-chain runtime,
//! so those handlers don't exist by default: any ``CALL_IMM`` to an
//! unregistered hash aborts the VM with ``EbpfError::UnsupportedInstruction``.
//!
//! That manifested concretely against the cross-repo WDIO smoke test
//! (run 27546133542): the entrypoint!()-emitted ``entrypoint`` function
//! deserialises the input region, then calls ``msg!("result: {}", ..)``
//! which lowers to ``sol_log_(ptr, len)``.  Without the handler, the
//! interpreter halted before ``process_instruction``'s ``let sum_val =
//! a + b;`` line ever executed -- so the recorder's PC->source map
//! never connected to lines 25..32 of ``solana_flow_test.rs`` and the
//! DAP server returned only the raw ``r0..r10`` registers when the
//! smoke test queried locals at the (effectively unreached) source
//! position.
//!
//! This module registers the minimum syscall set a typical Solana
//! program touches during entrypoint deserialisation and ``msg!``
//! logging.  Each handler routes the call through the recorder's
//! ``ContextObject`` (so the syscall name appears in the recorder
//! tracer when wired up) and performs the on-chain-equivalent side
//! effect:
//!   * Logging syscalls read the program's memory via the VM's
//!     ``MemoryMapping`` and capture the payload for later display.
//!   * ``sol_panic_`` halts execution with a typed error so the
//!     trace stops cleanly instead of running off the rails.
//!   * Memory syscalls (``sol_memcpy_`` etc.) perform the requested
//!     operation through the same ``MemoryMapping`` so subsequent
//!     program logic sees the expected effect.
//!   * ``sol_alloc_free_`` is provided for completeness with a
//!     bump-allocator semantic identical to the on-chain
//!     ``InvokeContext::allocator`` -- though most modern Solana
//!     programs use the program-side ``BumpAllocator`` (custom_heap_
//!     default!()) and never call this syscall.
//!
//! All handlers also push a record onto the context's syscall log so
//! tests can assert which syscalls a program ended up invoking; the
//! log doubles as the diagnostic surface we'd otherwise need a
//! recorder-internal counter for.

use std::cell::RefCell;

use solana_sbpf::{
    declare_builtin_function,
    elf::ElfError,
    error::EbpfError,
    memory_region::{AccessType, MemoryMapping},
    program::{BuiltinFunctionDefinition, BuiltinProgram},
    vm::ContextObject,
};

/// Per-VM state for syscall execution.
///
/// Stored inside the recorder's ``ContextObject`` so each syscall can:
///   * Read & write the program's virtual memory through
///     ``memory_mapping`` (the on-chain runtime keeps this in the
///     ``InvokeContext``; we mirror the layout so the syscall
///     implementations are line-for-line translations).
///   * Append a ``SyscallLogEntry`` to ``log`` -- the recorder's
///     unit tests and the diagnostic ``Execution result:`` printout
///     consume this to figure out *which* syscalls a program touched.
///   * Track the bump-allocator position for the legacy
///     ``sol_alloc_free_`` syscall.
pub struct SyscallState {
    /// Captured syscall invocations (name + payload summary) in the
    /// order they were called.  Use [`SyscallState::log_text`] to
    /// surface them.
    pub log: RefCell<Vec<SyscallLogEntry>>,
    /// Bump-allocator pointer for legacy ``sol_alloc_free_``.  The
    /// on-chain runtime initialises this to the *top* of the heap
    /// region and grows downward; we mirror that so a program that
    /// alternates between the program-side ``BumpAllocator`` and the
    /// syscall path doesn't collide with itself.
    pub bump_pos: RefCell<u64>,
    /// Heap base / length -- copied from the recorder's
    /// ``ebpf::MM_HEAP_START`` region so syscall handlers don't have
    /// to look up the region by index every call.
    pub heap_start: u64,
    pub heap_len: u64,
}

/// Single entry in the syscall log.  Captures only what's cheap to
/// surface; payloads that require VM-side memory reads are
/// best-effort stringified (utf8 lossy for ``sol_log_``, hex for
/// binary blobs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyscallLogEntry {
    pub name: String,
    pub payload: String,
}

impl SyscallState {
    pub fn new(heap_start: u64, heap_len: u64) -> Self {
        Self {
            log: RefCell::new(Vec::new()),
            // Bump pointer grows downward from the heap top -- matches
            // ``solana_program_entrypoint::BumpAllocator``'s convention.
            bump_pos: RefCell::new(heap_start.saturating_add(heap_len)),
            heap_start,
            heap_len,
        }
    }

    /// Snapshot of the syscall log as a vector of ``name(payload)``
    /// strings -- used by unit tests and the executor's diagnostic
    /// printout.
    pub fn log_text(&self) -> Vec<String> {
        self.log
            .borrow()
            .iter()
            .map(|e| format!("{}({})", e.name, e.payload))
            .collect()
    }

    /// Push an entry without locking the caller into ``RefCell`` borrow
    /// rules.  All syscall handlers in this module use this helper.
    fn push(&self, name: &str, payload: String) {
        self.log.borrow_mut().push(SyscallLogEntry {
            name: name.to_string(),
            payload,
        });
    }
}

/// Convenience trait implemented by the recorder's ``ContextObject``
/// (and any test stand-ins) so the generic syscalls in this module
/// can reach the shared syscall state.
pub trait HasSyscallState {
    fn syscall_state(&self) -> &SyscallState;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Read ``len`` bytes from the program's virtual address space.
///
/// The on-chain runtime uses the equivalent of ``translate_slice<u8>``
/// in agave; we go through ``MemoryMapping::map(AccessType::Load, ..)``
/// which performs the same address-range validation and returns the
/// raw host pointer.  On failure (bad address, region overrun) we
/// surface ``EbpfError::AccessViolation`` so the VM halts the
/// program -- mirroring the on-chain semantics where syscalls that
/// can't read their inputs fault the program.
fn read_slice(
    memory_mapping: &MemoryMapping,
    vm_addr: u64,
    len: u64,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    if len == 0 {
        return Ok(Vec::new());
    }
    match memory_mapping.map(AccessType::Load, vm_addr, len) {
        solana_sbpf::error::ProgramResult::Ok(host_addr) => {
            let slice = unsafe { std::slice::from_raw_parts(host_addr as *const u8, len as usize) };
            Ok(slice.to_vec())
        }
        solana_sbpf::error::ProgramResult::Err(e) => Err(Box::new(e)),
    }
}

/// Stringify a byte payload for the syscall log -- utf8 if possible,
/// otherwise a short hex preview.  Used by ``sol_log_`` /
/// ``sol_panic_`` etc. to surface a readable description without
/// dumping arbitrary binary into ``stderr``.
fn stringify_payload(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => {
            let preview: String = bytes.iter().take(32).map(|b| format!("{b:02x}")).collect();
            if bytes.len() > 32 {
                format!("hex:{preview}... ({} bytes)", bytes.len())
            } else {
                format!("hex:{preview}")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Syscall declarations
// ---------------------------------------------------------------------------

declare_builtin_function!(
    /// ``abort()`` — halt the program unconditionally.  On-chain this
    /// is the immediate target of Rust's ``::core::intrinsics::abort``
    /// and the panic handler's last-ditch exit.  We halt with a typed
    /// error so the recorder's diagnostic output names it explicitly
    /// rather than the generic ``UnsupportedInstruction`` (which is
    /// what an *unregistered* abort would surface as).
    SyscallAbort,
    fn rust(
        ctx: &mut crate::executor::RecorderContext,
        _a: u64,
        _b: u64,
        _c: u64,
        _d: u64,
        _e: u64,
    ) -> Result<u64, Box<dyn std::error::Error>> {
        ctx.syscall_state().push("abort", String::new());
        Err(Box::new(EbpfError::SyscallError(Box::new(
            std::io::Error::other("program called abort()"),
        ))))
    }
);

declare_builtin_function!(
    /// ``sol_panic_(file_ptr, file_len, line, column)`` -- read the
    /// panic file path out of the program's memory, log it, halt.
    /// Solana-side this is the target of
    /// ``custom_panic_default!()``'s panic handler.
    SyscallSolPanic,
    fn rust(
        ctx: &mut crate::executor::RecorderContext,
        file_ptr: u64,
        file_len: u64,
        line: u64,
        column: u64,
        _e: u64,
    ) -> Result<u64, Box<dyn std::error::Error>> {
        let file = read_slice(&ctx.memory_mapping, file_ptr, file_len)
            .map(|b| stringify_payload(&b))
            .unwrap_or_else(|_| "<unreadable>".to_string());
        ctx.syscall_state().push(
            "sol_panic_",
            format!("{file}:{line}:{column}"),
        );
        Err(Box::new(EbpfError::SyscallError(Box::new(
            std::io::Error::other(format!("program panicked at {file}:{line}:{column}")),
        ))))
    }
);

declare_builtin_function!(
    /// ``sol_log_(msg_ptr, msg_len)`` -- read the message from the
    /// program's memory, capture it in the syscall log.  This is the
    /// syscall ``msg!(..)`` ultimately lowers to and the one a typical
    /// non-CPI Solana program calls most often.
    SyscallSolLog,
    fn rust(
        ctx: &mut crate::executor::RecorderContext,
        msg_ptr: u64,
        msg_len: u64,
        _c: u64,
        _d: u64,
        _e: u64,
    ) -> Result<u64, Box<dyn std::error::Error>> {
        let bytes = read_slice(&ctx.memory_mapping, msg_ptr, msg_len)?;
        let text = stringify_payload(&bytes);
        ctx.syscall_state().push("sol_log_", text);
        Ok(0)
    }
);

declare_builtin_function!(
    /// ``sol_log_64_(arg1, arg2, arg3, arg4, arg5)`` -- log up to five
    /// u64 values.  Used by ``msg!("0x{:x}", n)`` shortcuts and
    /// debug helpers.
    SyscallSolLog64,
    fn rust(
        ctx: &mut crate::executor::RecorderContext,
        a: u64,
        b: u64,
        c: u64,
        d: u64,
        e: u64,
    ) -> Result<u64, Box<dyn std::error::Error>> {
        ctx.syscall_state().push(
            "sol_log_64_",
            format!("{a:#x} {b:#x} {c:#x} {d:#x} {e:#x}"),
        );
        Ok(0)
    }
);

declare_builtin_function!(
    /// ``sol_log_pubkey(pubkey_ptr)`` -- read 32 bytes from
    /// ``pubkey_ptr`` and log as a base58-ish hex preview.
    SyscallSolLogPubkey,
    fn rust(
        ctx: &mut crate::executor::RecorderContext,
        pubkey_ptr: u64,
        _b: u64,
        _c: u64,
        _d: u64,
        _e: u64,
    ) -> Result<u64, Box<dyn std::error::Error>> {
        let bytes = read_slice(&ctx.memory_mapping, pubkey_ptr, 32)
            .unwrap_or_default();
        ctx.syscall_state().push(
            "sol_log_pubkey",
            stringify_payload(&bytes),
        );
        Ok(0)
    }
);

declare_builtin_function!(
    /// ``sol_log_compute_units_()`` -- log the remaining compute
    /// budget.  The recorder doesn't enforce compute budgets in the
    /// same way as on-chain so we report a placeholder; the call is
    /// still surfaced in the log.
    SyscallSolLogComputeUnits,
    fn rust(
        ctx: &mut crate::executor::RecorderContext,
        _a: u64,
        _b: u64,
        _c: u64,
        _d: u64,
        _e: u64,
    ) -> Result<u64, Box<dyn std::error::Error>> {
        let remaining = ContextObject::get_remaining(ctx);
        ctx.syscall_state().push(
            "sol_log_compute_units_",
            format!("remaining={remaining}"),
        );
        Ok(0)
    }
);

declare_builtin_function!(
    /// ``sol_log_data(slice_ptr, slice_len)`` -- log raw binary data.
    /// Used by programs that call ``sol_log_data!(..)`` directly.
    SyscallSolLogData,
    fn rust(
        ctx: &mut crate::executor::RecorderContext,
        slice_ptr: u64,
        slice_len: u64,
        _c: u64,
        _d: u64,
        _e: u64,
    ) -> Result<u64, Box<dyn std::error::Error>> {
        let bytes = read_slice(&ctx.memory_mapping, slice_ptr, slice_len)
            .unwrap_or_default();
        ctx.syscall_state().push(
            "sol_log_data",
            stringify_payload(&bytes),
        );
        Ok(0)
    }
);

declare_builtin_function!(
    /// ``sol_memcpy_(dst, src, n)`` -- byte copy between two virtual
    /// addresses.  The on-chain runtime asserts non-overlap; we
    /// preserve that semantic since programs that need overlap-safe
    /// copy reach for ``sol_memmove_`` instead.
    SyscallSolMemcpy,
    fn rust(
        ctx: &mut crate::executor::RecorderContext,
        dst_addr: u64,
        src_addr: u64,
        n: u64,
        _d: u64,
        _e: u64,
    ) -> Result<u64, Box<dyn std::error::Error>> {
        ctx.syscall_state().push(
            "sol_memcpy_",
            format!("dst={dst_addr:#x} src={src_addr:#x} n={n}"),
        );
        if n == 0 {
            return Ok(0);
        }
        let src_host = match ctx.memory_mapping.map(AccessType::Load, src_addr, n) {
            solana_sbpf::error::ProgramResult::Ok(a) => a,
            solana_sbpf::error::ProgramResult::Err(e) => return Err(Box::new(e)),
        };
        let dst_host = match ctx.memory_mapping.map(AccessType::Store, dst_addr, n) {
            solana_sbpf::error::ProgramResult::Ok(a) => a,
            solana_sbpf::error::ProgramResult::Err(e) => return Err(Box::new(e)),
        };
        unsafe {
            std::ptr::copy_nonoverlapping(
                src_host as *const u8,
                dst_host as *mut u8,
                n as usize,
            );
        }
        Ok(0)
    }
);

declare_builtin_function!(
    /// ``sol_memmove_(dst, src, n)`` -- overlap-safe byte copy.
    SyscallSolMemmove,
    fn rust(
        ctx: &mut crate::executor::RecorderContext,
        dst_addr: u64,
        src_addr: u64,
        n: u64,
        _d: u64,
        _e: u64,
    ) -> Result<u64, Box<dyn std::error::Error>> {
        ctx.syscall_state().push(
            "sol_memmove_",
            format!("dst={dst_addr:#x} src={src_addr:#x} n={n}"),
        );
        if n == 0 {
            return Ok(0);
        }
        let src_host = match ctx.memory_mapping.map(AccessType::Load, src_addr, n) {
            solana_sbpf::error::ProgramResult::Ok(a) => a,
            solana_sbpf::error::ProgramResult::Err(e) => return Err(Box::new(e)),
        };
        let dst_host = match ctx.memory_mapping.map(AccessType::Store, dst_addr, n) {
            solana_sbpf::error::ProgramResult::Ok(a) => a,
            solana_sbpf::error::ProgramResult::Err(e) => return Err(Box::new(e)),
        };
        unsafe {
            std::ptr::copy(src_host as *const u8, dst_host as *mut u8, n as usize);
        }
        Ok(0)
    }
);

declare_builtin_function!(
    /// ``sol_memset_(dst, val, n)`` -- fill ``n`` bytes at ``dst`` with
    /// the low byte of ``val``.
    SyscallSolMemset,
    fn rust(
        ctx: &mut crate::executor::RecorderContext,
        dst_addr: u64,
        val: u64,
        n: u64,
        _d: u64,
        _e: u64,
    ) -> Result<u64, Box<dyn std::error::Error>> {
        ctx.syscall_state().push(
            "sol_memset_",
            format!("dst={dst_addr:#x} val={val:#x} n={n}"),
        );
        if n == 0 {
            return Ok(0);
        }
        let dst_host = match ctx.memory_mapping.map(AccessType::Store, dst_addr, n) {
            solana_sbpf::error::ProgramResult::Ok(a) => a,
            solana_sbpf::error::ProgramResult::Err(e) => return Err(Box::new(e)),
        };
        unsafe {
            std::ptr::write_bytes(dst_host as *mut u8, val as u8, n as usize);
        }
        Ok(0)
    }
);

declare_builtin_function!(
    /// ``sol_memcmp_(s1, s2, n, result_ptr)`` -- compare ``n`` bytes,
    /// store -1/0/1 at ``result_ptr``.
    SyscallSolMemcmp,
    fn rust(
        ctx: &mut crate::executor::RecorderContext,
        s1_addr: u64,
        s2_addr: u64,
        n: u64,
        result_addr: u64,
        _e: u64,
    ) -> Result<u64, Box<dyn std::error::Error>> {
        ctx.syscall_state().push(
            "sol_memcmp_",
            format!("s1={s1_addr:#x} s2={s2_addr:#x} n={n}"),
        );
        let cmp_val: i32 = if n == 0 {
            0
        } else {
            let s1_host = match ctx.memory_mapping.map(AccessType::Load, s1_addr, n) {
                solana_sbpf::error::ProgramResult::Ok(a) => a,
                solana_sbpf::error::ProgramResult::Err(e) => return Err(Box::new(e)),
            };
            let s2_host = match ctx.memory_mapping.map(AccessType::Load, s2_addr, n) {
                solana_sbpf::error::ProgramResult::Ok(a) => a,
                solana_sbpf::error::ProgramResult::Err(e) => return Err(Box::new(e)),
            };
            let a = unsafe { std::slice::from_raw_parts(s1_host as *const u8, n as usize) };
            let b = unsafe { std::slice::from_raw_parts(s2_host as *const u8, n as usize) };
            match a.cmp(b) {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            }
        };
        let result_host = match ctx
            .memory_mapping
            .map(AccessType::Store, result_addr, 4)
        {
            solana_sbpf::error::ProgramResult::Ok(a) => a,
            solana_sbpf::error::ProgramResult::Err(e) => return Err(Box::new(e)),
        };
        unsafe {
            std::ptr::write_unaligned(result_host as *mut i32, cmp_val);
        }
        Ok(0)
    }
);

declare_builtin_function!(
    /// ``sol_alloc_free_(size, free_addr)`` -- legacy heap allocator
    /// syscall.  Modern Solana programs ship their own
    /// ``BumpAllocator`` via ``custom_heap_default!()`` and never
    /// reach this syscall, but a handler is registered so a program
    /// that *does* use it (or links against a crate that does)
    /// doesn't UnsupportedInstruction.
    ///
    /// Semantics mirror agave's ``BpfAllocator``:
    ///   * ``free_addr != 0`` -> free (no-op for bump alloc), returns 0.
    ///   * ``free_addr == 0`` -> bump-allocate ``size`` bytes from the
    ///     top of the heap, return the virtual address (or 0 on OOM).
    SyscallSolAllocFree,
    fn rust(
        ctx: &mut crate::executor::RecorderContext,
        size: u64,
        free_addr: u64,
        _c: u64,
        _d: u64,
        _e: u64,
    ) -> Result<u64, Box<dyn std::error::Error>> {
        ctx.syscall_state().push(
            "sol_alloc_free_",
            format!("size={size} free={free_addr:#x}"),
        );
        if free_addr != 0 {
            return Ok(0);
        }
        let state = ctx.syscall_state();
        let mut pos = state.bump_pos.borrow_mut();
        // 8-byte alignment matches agave's BpfAllocator default.
        let aligned = pos.saturating_sub(size) & !7u64;
        if aligned < state.heap_start.saturating_add(8) {
            return Ok(0); // OOM
        }
        *pos = aligned;
        Ok(aligned)
    }
);

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register the recorder's syscall handlers on the supplied loader.
///
/// Called once during ``execute_with_tracing``.  Each entry maps the
/// on-chain syscall *name* (the string ``ebpf::hash_symbol_name``
/// hashes to produce the ``CALL_IMM`` operand) to the corresponding
/// handler declared above.  The hash is what the program's compiled
/// ELF embeds in its ``call <imm>`` instructions; matching names
/// means the recorder VM sees the same hashes the on-chain runtime
/// does and dispatches to our handlers.
///
/// Returns the loader for chained use.
pub fn register(
    mut loader: BuiltinProgram<crate::executor::RecorderContext>,
) -> Result<BuiltinProgram<crate::executor::RecorderContext>, ElfError> {
    SyscallAbort::register(&mut loader, "abort")?;
    SyscallSolPanic::register(&mut loader, "sol_panic_")?;
    SyscallSolLog::register(&mut loader, "sol_log_")?;
    SyscallSolLog64::register(&mut loader, "sol_log_64_")?;
    SyscallSolLogPubkey::register(&mut loader, "sol_log_pubkey")?;
    SyscallSolLogComputeUnits::register(&mut loader, "sol_log_compute_units_")?;
    SyscallSolLogData::register(&mut loader, "sol_log_data")?;
    SyscallSolMemcpy::register(&mut loader, "sol_memcpy_")?;
    SyscallSolMemmove::register(&mut loader, "sol_memmove_")?;
    SyscallSolMemset::register(&mut loader, "sol_memset_")?;
    SyscallSolMemcmp::register(&mut loader, "sol_memcmp_")?;
    SyscallSolAllocFree::register(&mut loader, "sol_alloc_free_")?;
    Ok(loader)
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sbpf::{
        assembler::assemble,
        ebpf,
        program::{BuiltinProgram, SBPFVersion},
        vm::Config,
    };
    use std::sync::Arc;

    /// ``register()`` succeeds for the canonical syscall set without
    /// duplicate-name errors (regression test against accidentally
    /// listing the same syscall twice in [`register`]).
    #[test]
    fn register_populates_loader_without_collisions() {
        let config = Config::default();
        let loader = BuiltinProgram::<crate::executor::RecorderContext>::new_loader(config.clone());
        let loader = register(loader).expect("register should not error");
        let registry = loader.get_function_registry();
        // 12 syscalls declared above.
        assert_eq!(
            registry.iter().count(),
            12,
            "register should add exactly the 12 declared syscalls"
        );
        // Spot-check a few names by their hash.
        for name in ["sol_log_", "sol_panic_", "sol_memcpy_"] {
            let key = ebpf::hash_symbol_name(name.as_bytes());
            assert!(
                registry.lookup_by_key(key).is_some(),
                "syscall {name} must be in the registry under its hashed key"
            );
        }
    }

    /// End-to-end: assemble a tiny SBPF program that calls
    /// ``sol_log_``, drive it through the same VM setup
    /// ``execute_with_tracing`` uses (memory regions + interpreter),
    /// and verify (a) the program exits cleanly (Ok), (b) the
    /// syscall handler captured the call in the recorder's syscall
    /// log.
    ///
    /// This is the canonical reproduction of the cross-repo failure
    /// at run 27546133542.  Before [`super::register`] the call would
    /// abort with ``EbpfError::UnsupportedInstruction``; after, the
    /// trace completes and the syscall log shows the call was
    /// dispatched to our handler.
    #[test]
    fn assembled_program_calling_sol_log_runs_through_recorder_loader() {
        use solana_sbpf::{
            ebpf,
            error::ProgramResult,
            memory_region::{MemoryMapping, MemoryRegion},
            verifier::RequisiteVerifier,
            vm::EbpfVm,
        };

        // Hand-assembled SBPFv3: zero the args so ``sol_log_``'s
        // ``len=0`` early-return path skips the memory translation
        // (this keeps the test self-contained — no need to set up a
        // backed buffer in the program's VM memory for the message
        // string).
        let asm = "
            mov64 r1, 0
            mov64 r2, 0
            syscall sol_log_
            exit
        ";

        let config = Config {
            enable_register_tracing: true,
            enable_instruction_meter: true,
            enabled_sbpf_versions: SBPFVersion::V3..=SBPFVersion::V3,
            ..Config::default()
        };
        let loader = super::register(
            BuiltinProgram::<crate::executor::RecorderContext>::new_loader(config.clone()),
        )
        .expect("register should not error");
        let loader = Arc::new(loader);
        let executable = assemble::<crate::executor::RecorderContext>(asm, loader.clone())
            .expect("assemble should succeed");
        executable
            .verify::<RequisiteVerifier>()
            .expect("assembled program verifies");

        // Build the same memory regions ``execute_with_tracing`` uses,
        // so the test exercises identical setup minus the ELF load.
        let stack_size = config.stack_size();
        let mut stack = vec![0u8; stack_size];
        let heap_size = 32 * 1024;
        let mut heap = vec![0u8; heap_size];
        let mut input = vec![0u8; 48];

        let sbpf_version = executable.get_sbpf_version();
        let regions = vec![
            MemoryRegion::new_writable(&mut stack, ebpf::MM_STACK_START),
            MemoryRegion::new_writable(&mut heap, ebpf::MM_HEAP_START),
            MemoryRegion::new_writable(&mut input, ebpf::MM_INPUT_START),
        ];

        let mut context = crate::executor::RecorderContext::new_for_tests(
            1_000_000,
            &config,
            ebpf::MM_HEAP_START,
            heap_size as u64,
        );
        context.memory_mapping = MemoryMapping::new(regions, &config, sbpf_version).unwrap();

        let mut vm = EbpfVm::new(loader.clone(), sbpf_version, &mut context, stack_size);
        let mut mode = solana_sbpf::vm::ExecutionMode::Interpreted;
        let mut call_frames = vec![solana_sbpf::vm::CallFrame::default(); config.max_call_depth];
        let (_insn_count, result) = vm.execute_program(&executable, &mut mode, &mut call_frames);

        match result {
            ProgramResult::Ok(_) => {}
            other => panic!("expected Ok, got {other:?}"),
        }
        let log = context.syscall_state().log_text();
        assert!(
            log.iter().any(|e| e.starts_with("sol_log_(")),
            "syscall log should contain sol_log_ call; saw: {log:?}"
        );
    }
}
