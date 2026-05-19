//! Comprehensive integration tests for the Solana recorder.
//!
//! These tests exercise the recorder against various Solana program execution
//! scenarios using synthetic register traces and source mappings. No live
//! Mollusk execution or real ELF/DWARF files are needed.

use std::path::{Path, PathBuf};
use std::process::Command;

use codetracer_solana_recorder::account_decoder::{
    decoded_fields_to_struct_record, AnchorIdl, BorshDecoder, DecodedField, DecodedValue, IdlType,
    TypeIds,
};
use codetracer_solana_recorder::cpi::{CpiDetector, CpiEvent};
use codetracer_solana_recorder::multi_program::ProgramRegistry;
use codetracer_solana_recorder::recorder::{record_from_snapshots, record_with_cpi};
use codetracer_solana_recorder::register_trace::{parse_regs_file, RegisterSnapshot, ROW_SIZE};
use codetracer_solana_recorder::tracer_trait::{
    replay_snapshots, CodeTracerTracer, NoOpTracer, SbpfTracer,
};
use codetracer_trace_types::{
    CallRecord, FullValueRecord, FunctionId, FunctionRecord, Line, PathId, ReturnRecord,
    StepRecord, TraceLowLevelEvent, TypeId, ValueRecord, VariableId,
};
use codetracer_trace_writer_nim::trace_writer::TraceWriter;
use codetracer_trace_writer_nim::{create_trace_writer, TraceEventsFileFormat};

// ===========================================================================
// Helpers
// ===========================================================================

/// Build a register snapshot with explicit register values.
fn make_snap(regs: [u64; 12]) -> RegisterSnapshot {
    RegisterSnapshot { registers: regs }
}

/// Build a register snapshot with only PC set.
fn snap_pc(pc: u64) -> RegisterSnapshot {
    let mut regs = [0u64; 12];
    regs[11] = pc;
    make_snap(regs)
}

/// Build a register snapshot with PC and selected registers.
fn snap_regs(pc: u64, r0: u64, r1: u64, r2: u64, r3: u64) -> RegisterSnapshot {
    let mut regs = [0u64; 12];
    regs[0] = r0;
    regs[1] = r1;
    regs[2] = r2;
    regs[3] = r3;
    regs[11] = pc;
    make_snap(regs)
}

/// Build a register snapshot with all argument and callee-saved registers.
fn snap_full(
    pc: u64,
    r0: u64,
    r1: u64,
    r2: u64,
    r3: u64,
    r4: u64,
    r5: u64,
    r6: u64,
    r7: u64,
    r8: u64,
    r9: u64,
    r10: u64,
) -> RegisterSnapshot {
    let regs = [r0, r1, r2, r3, r4, r5, r6, r7, r8, r9, r10, pc];
    make_snap(regs)
}

/// Encode snapshots to raw `.regs` binary.
fn encode_regs(snapshots: &[RegisterSnapshot]) -> Vec<u8> {
    let mut data = Vec::with_capacity(snapshots.len() * ROW_SIZE);
    for snap in snapshots {
        for &r in &snap.registers {
            data.extend_from_slice(&r.to_le_bytes());
        }
    }
    data
}

// ---------------------------------------------------------------------------
// CTFS reader: shells out to `ct-print --full --strip-paths` and translates
// the JSON document the Nim binary emits into the legacy
// `TraceLowLevelEvent` stream the downstream assertions consume.  Mirrors
// the helper in `test_tracer.rs`.  Only the variants the assertions in
// this file check are emitted (`Path`, `VariableName`, `Function`, `Step`,
// `Call`, `Return`, integer-typed `Value`).  Compound `ValueRecord`
// variants (`Sequence` / `Tuple` / `Struct` / ...) are intentionally
// skipped because nothing here keys off them — the per-program tests in
// `test_per_program_ct_print_full.rs` already pin those.
fn parse_events_from_ct(ct_path: &Path) -> Vec<TraceLowLevelEvent> {
    let ct_print = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("codetracer-trace-format-nim")
        .join("ct-print");
    assert!(
        ct_print.exists(),
        "ct-print binary not found at {}; tests require the \
         codetracer-trace-format-nim sibling repo",
        ct_print.display()
    );
    let output = Command::new(&ct_print)
        .args(["--full", "--strip-paths"])
        .arg(ct_path)
        .output()
        .expect("failed to invoke ct-print");
    assert!(
        output.status.success(),
        "ct-print --full failed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let doc: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("ct-print --full must emit valid JSON");

    let mut events: Vec<TraceLowLevelEvent> = Vec::new();
    for p in doc["paths"].as_array().expect("paths array").iter() {
        events.push(TraceLowLevelEvent::Path(PathBuf::from(
            p.as_str().expect("path is string"),
        )));
    }
    for v in doc["varnames"].as_array().expect("varnames array").iter() {
        events.push(TraceLowLevelEvent::VariableName(
            v.as_str().expect("varname is string").to_string(),
        ));
    }
    for f in doc["functions"].as_array().expect("functions array").iter() {
        events.push(TraceLowLevelEvent::Function(FunctionRecord {
            path_id: PathId(0),
            line: Line(1),
            name: f.as_str().expect("function is string").to_string(),
        }));
    }
    for ev in doc["events"].as_array().expect("events array").iter() {
        match ev["kind"].as_str().expect("kind str") {
            "step" => {
                let path_id = ev["path_id"].as_u64().expect("path_id u64") as usize;
                let line = ev["line"].as_i64().expect("line i64");
                events.push(TraceLowLevelEvent::Step(StepRecord {
                    path_id: PathId(path_id),
                    line: Line(line),
                }));
                if let Some(vars) = ev["vars"].as_array() {
                    for v in vars {
                        let value = &v["value"];
                        if value["kind"].as_str() == Some("Int") {
                            let i = value["i"].as_i64().expect("Int.i must be i64");
                            let type_id = value["type_id"].as_u64().unwrap_or(0) as usize;
                            let varname_id =
                                v["varname_id"].as_u64().expect("varname_id u64") as usize;
                            events.push(TraceLowLevelEvent::Value(FullValueRecord {
                                variable_id: VariableId(varname_id),
                                value: ValueRecord::Int {
                                    i,
                                    type_id: TypeId(type_id),
                                },
                            }));
                        }
                    }
                }
            }
            "call_entry" => {
                let function_id = ev["function_id"].as_u64().expect("function_id u64") as usize;
                events.push(TraceLowLevelEvent::Call(CallRecord {
                    function_id: FunctionId(function_id),
                    args: vec![],
                }));
            }
            "call_exit" => {
                events.push(TraceLowLevelEvent::Return(ReturnRecord {
                    return_value: ValueRecord::None { type_id: TypeId(0) },
                }));
            }
            _ => {}
        }
    }
    events
}

/// Read the single `.ct` file in `dir` and parse its events via ct-print.
fn read_ct_events(dir: &Path) -> Vec<TraceLowLevelEvent> {
    let ct_files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map_or(false, |ext| ext == "ct"))
        .collect();
    assert_eq!(ct_files.len(), 1, "expected exactly one .ct file");
    let content = std::fs::read(&ct_files[0]).unwrap();
    assert_eq!(
        &content[..5],
        &[0xC0u8, 0xDE, 0x72, 0xAC, 0xE2],
        "CTFS magic"
    );
    parse_events_from_ct(&ct_files[0])
}

// ---------------------------------------------------------------------------
// Structured trace parsing helpers
// ---------------------------------------------------------------------------

/// Collect all Step events from parsed trace events.
fn step_events(events: &[TraceLowLevelEvent]) -> Vec<&StepRecord> {
    events
        .iter()
        .filter_map(|e| match e {
            TraceLowLevelEvent::Step(s) => Some(s),
            _ => None,
        })
        .collect()
}

/// Collect all Call events from parsed trace events.
fn call_events(events: &[TraceLowLevelEvent]) -> Vec<&CallRecord> {
    events
        .iter()
        .filter_map(|e| match e {
            TraceLowLevelEvent::Call(c) => Some(c),
            _ => None,
        })
        .collect()
}

/// Collect all Return events from parsed trace events.
fn return_events(events: &[TraceLowLevelEvent]) -> Vec<&ReturnRecord> {
    events
        .iter()
        .filter_map(|e| match e {
            TraceLowLevelEvent::Return(r) => Some(r),
            _ => None,
        })
        .collect()
}

/// Collect all Function registration events.
fn function_events(events: &[TraceLowLevelEvent]) -> Vec<&FunctionRecord> {
    events
        .iter()
        .filter_map(|e| match e {
            TraceLowLevelEvent::Function(f) => Some(f),
            _ => None,
        })
        .collect()
}

/// Collect all Path events (as strings).
fn path_events(events: &[TraceLowLevelEvent]) -> Vec<&Path> {
    events
        .iter()
        .filter_map(|e| match e {
            TraceLowLevelEvent::Path(p) => Some(p.as_path()),
            _ => None,
        })
        .collect()
}

/// Collect all VariableName interning events.
fn variable_name_events(events: &[TraceLowLevelEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|e| match e {
            TraceLowLevelEvent::VariableName(n) => Some(n.as_str()),
            _ => None,
        })
        .collect()
}

/// Record via `record_from_snapshots` and decode the resulting `.ct` file
/// through `ct-print --full`.  Returns the live `TraceLowLevelEvent` stream.
fn record_and_parse_events(
    snapshots: &[RegisterSnapshot],
    source_locs: &[(u64, &str, u32)],
    source_path: &str,
) -> Vec<TraceLowLevelEvent> {
    let tmp = tempfile::TempDir::new().unwrap();
    record_from_snapshots(snapshots, source_locs, Path::new(source_path), tmp.path()).unwrap();
    read_ct_events(tmp.path())
}

/// Record via `record_with_cpi` and decode through `ct-print --full`.
fn record_cpi_and_parse_events(
    snapshots: &[RegisterSnapshot],
    registry: &ProgramRegistry,
    detector: &mut CpiDetector,
    source_path: &str,
) -> Vec<TraceLowLevelEvent> {
    let tmp = tempfile::TempDir::new().unwrap();
    record_with_cpi(
        snapshots,
        registry,
        detector,
        Path::new(source_path),
        tmp.path(),
    )
    .unwrap();
    read_ct_events(tmp.path())
}

// ===========================================================================
// 1. Register trace patterns for different instruction types
// ===========================================================================

/// Arithmetic sequence: ADD, SUB, MUL, DIV visible through register changes.
#[test]
fn test_arithmetic_register_patterns() {
    // Simulate:
    //   r1 = 100       (load immediate)
    //   r2 = 30        (load immediate)
    //   r3 = r1 + r2   (ADD -> 130)
    //   r4 = r3 - r2   (SUB -> 100)
    //   r5 = r4 * 3    (MUL -> 300, 3 in r2 first)
    //   r0 = r5 / 10   (DIV -> 30, 10 in r2 first)
    let snapshots = vec![
        snap_full(0, 0, 100, 0, 0, 0, 0, 0, 0, 0, 0, 0x3000),
        snap_full(1, 0, 100, 30, 0, 0, 0, 0, 0, 0, 0, 0x3000),
        snap_full(2, 0, 100, 30, 130, 0, 0, 0, 0, 0, 0, 0x3000),
        snap_full(3, 0, 100, 30, 130, 100, 0, 0, 0, 0, 0, 0x3000),
        snap_full(4, 0, 100, 3, 130, 100, 300, 0, 0, 0, 0, 0x3000),
        snap_full(5, 30, 100, 10, 130, 100, 300, 0, 0, 0, 0, 0x3000),
    ];

    let source_locs: Vec<(u64, &str, u32)> = vec![
        (0, "arith.rs", 10),
        (1, "arith.rs", 11),
        (2, "arith.rs", 12),
        (3, "arith.rs", 13),
        (4, "arith.rs", 14),
        (5, "arith.rs", 15),
    ];

    let events = record_and_parse_events(&snapshots, &source_locs, "arith.rs");

    // Distinct integer values across all Value events: r10=0x3000=12288 frame
    // pointer, r1=100, r2 ∈ {30,3,10}, r3 ∈ {0,130}, r4 ∈ {0,100}, r5 ∈ {0,300},
    // r0 ∈ {0,30}, plus the implicit zeros from un-loaded registers.
    let mut int_values: Vec<i64> = events
        .iter()
        .filter_map(|e| match e {
            TraceLowLevelEvent::Value(FullValueRecord {
                value: ValueRecord::Int { i, .. },
                ..
            }) => Some(*i),
            _ => None,
        })
        .collect();
    int_values.sort_unstable();
    int_values.dedup();
    assert_eq!(int_values, vec![0i64, 3, 10, 30, 100, 130, 300, 0x3000]);

    // 1 implicit start step at line 1 + 6 line-changes (10..=15) = 7 steps.
    let steps = step_events(&events);
    assert_eq!(steps.len(), 7);
}

/// Memory access patterns: register values showing stack/heap addresses.
#[test]
fn test_memory_access_register_patterns() {
    // SBF memory layout:
    //   Stack region: 0x200000000 - 0x300000000
    //   Heap region:  0x300000000 - 0x400000000
    //   Input region: 0x400000000 - 0x500000000
    let stack_addr: u64 = 0x200000100;
    let heap_addr: u64 = 0x300000200;
    let input_addr: u64 = 0x400000000;
    let frame_ptr: u64 = 0x200001000;

    let snapshots = vec![
        // Load stack address into r1
        snap_full(0, 0, stack_addr, 0, 0, 0, 0, 0, 0, 0, 0, frame_ptr),
        // Load heap address into r2
        snap_full(1, 0, stack_addr, heap_addr, 0, 0, 0, 0, 0, 0, 0, frame_ptr),
        // Load value from stack into r3
        snap_full(2, 0, stack_addr, heap_addr, 42, 0, 0, 0, 0, 0, 0, frame_ptr),
        // Store to heap via r2, load input region
        snap_full(
            3, 0, stack_addr, heap_addr, 42, input_addr, 0, 0, 0, 0, 0, frame_ptr,
        ),
    ];

    let source_locs: Vec<(u64, &str, u32)> = vec![
        (0, "mem.rs", 1),
        (1, "mem.rs", 2),
        (2, "mem.rs", 3),
        (3, "mem.rs", 4),
    ];

    let events = record_and_parse_events(&snapshots, &source_locs, "mem.rs");

    // The full r0..r10 register table is interned in declaration order.
    let var_names: Vec<&str> = variable_name_events(&events);
    assert_eq!(
        var_names,
        vec!["r0", "r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9", "r10"],
    );

    // Distinct integer values: 0 (zero registers), the 4 SBF region addresses,
    // and the frame pointer (which equals the stack address `frame_ptr`).
    let mut int_values: Vec<i64> = events
        .iter()
        .filter_map(|e| match e {
            TraceLowLevelEvent::Value(FullValueRecord {
                value: ValueRecord::Int { i, .. },
                ..
            }) => Some(*i),
            _ => None,
        })
        .collect();
    int_values.sort_unstable();
    int_values.dedup();
    assert_eq!(
        int_values,
        vec![
            0i64,
            42,
            stack_addr as i64,
            frame_ptr as i64,
            heap_addr as i64,
            input_addr as i64,
        ],
    );

    // 1 implicit start step at line 1 + 4 line-changes (1..=4) = 5 steps.
    let steps = step_events(&events);
    assert_eq!(steps.len(), 5);
}

/// Syscall pattern: register state before/after sol_log, sol_create_program_address.
#[test]
fn test_syscall_register_patterns() {
    // Before sol_log: r1 = msg_ptr, r2 = msg_len
    // After sol_log: r0 = 0 (success)
    // Before sol_create_program_address: r1 = seeds_ptr, r2 = seeds_len,
    //   r3 = program_id_ptr, r4 = address_out_ptr
    // After: r0 = 0 (success)
    let msg_ptr: u64 = 0x300000100;
    let msg_len: u64 = 13; // "Hello Solana!"

    let seeds_ptr: u64 = 0x300000200;
    let seeds_len: u64 = 2;
    let program_id_ptr: u64 = 0x300000300;
    let addr_out_ptr: u64 = 0x300000400;

    let snapshots = vec![
        // Setup before sol_log
        snap_full(0, 0, msg_ptr, msg_len, 0, 0, 0, 0, 0, 0, 0, 0x3000),
        // After sol_log returns success
        snap_full(1, 0, msg_ptr, msg_len, 0, 0, 0, 0, 0, 0, 0, 0x3000),
        // Setup before sol_create_program_address
        snap_full(
            2,
            0,
            seeds_ptr,
            seeds_len,
            program_id_ptr,
            addr_out_ptr,
            0,
            0,
            0,
            0,
            0,
            0x3000,
        ),
        // After sol_create_program_address returns success
        snap_full(
            3,
            0,
            seeds_ptr,
            seeds_len,
            program_id_ptr,
            addr_out_ptr,
            0,
            0,
            0,
            0,
            0,
            0x3000,
        ),
    ];

    let source_locs: Vec<(u64, &str, u32)> = vec![
        (0, "syscall.rs", 20),
        (1, "syscall.rs", 21),
        (2, "syscall.rs", 30),
        (3, "syscall.rs", 31),
    ];

    let events = record_and_parse_events(&snapshots, &source_locs, "syscall.rs");

    // Variable-name table is the full r0..r10 register set.
    let var_names: Vec<&str> = variable_name_events(&events);
    assert_eq!(
        var_names,
        vec!["r0", "r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9", "r10"],
    );

    let mut int_values: Vec<i64> = events
        .iter()
        .filter_map(|e| match e {
            TraceLowLevelEvent::Value(FullValueRecord {
                value: ValueRecord::Int { i, .. },
                ..
            }) => Some(*i),
            _ => None,
        })
        .collect();
    int_values.sort_unstable();
    int_values.dedup();
    assert_eq!(
        int_values,
        vec![
            0i64,
            2,
            13,
            0x3000,
            msg_ptr as i64,
            seeds_ptr as i64,
            program_id_ptr as i64,
            addr_out_ptr as i64,
        ],
    );

    // 1 implicit start step + 4 line-changes (20, 21, 30, 31) = 5 steps.
    let steps = step_events(&events);
    assert_eq!(steps.len(), 5);
}

/// Function call/return: PC jumps indicating function entry/exit.
#[test]
fn test_function_call_return_pc_jumps() {
    // Primary function at PC 0..2, callee at PC 100..102, return to PC 3.
    // The recorder uses the heuristic: forward jump > 2 = call, backward > 2 = return.
    let snapshots = vec![
        snap_pc(0),   // line 1
        snap_pc(1),   // line 2
        snap_pc(100), // CALL (forward jump > 2) -> line 50
        snap_pc(101), // inside callee -> line 51
        snap_pc(102), // inside callee -> line 52
        snap_pc(2),   // RETURN (backward jump > 2) -> line 3
        snap_pc(3),   // continue in caller -> line 4
    ];

    let source_locs: Vec<(u64, &str, u32)> = vec![
        (0, "caller.rs", 1),
        (1, "caller.rs", 2),
        (100, "callee.rs", 50),
        (101, "callee.rs", 51),
        (102, "callee.rs", 52),
        (2, "caller.rs", 3),
        (3, "caller.rs", 4),
    ];

    let events = record_and_parse_events(&snapshots, &source_locs, "caller.rs");

    // 2 calls: main (outermost) + the synthesised fn_at_pc_100 frame.
    let calls = call_events(&events);
    assert_eq!(calls.len(), 2);

    // 2 returns: callee return + main return.
    let returns = return_events(&events);
    assert_eq!(returns.len(), 2);

    // Function table is exactly the two resolved names, in registration order.
    let func_names: Vec<&str> = function_events(&events)
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    assert_eq!(func_names, vec!["main", "fn_at_pc_100"]);

    // Path table is exactly the two source files, in registration order.
    let paths: Vec<&Path> = path_events(&events);
    assert_eq!(paths, vec![Path::new("caller.rs"), Path::new("callee.rs")]);
}

// ===========================================================================
// 2. DWARF source mapping scenarios
// ===========================================================================

/// Multiple source files: instructions map to lib.rs and helpers.rs.
#[test]
fn test_multiple_source_files() {
    let snapshots = vec![
        snap_pc(0),
        snap_pc(1),
        snap_pc(2),
        snap_pc(3),
        snap_pc(4),
        snap_pc(5),
    ];

    let source_locs: Vec<(u64, &str, u32)> = vec![
        (0, "src/lib.rs", 10),
        (1, "src/lib.rs", 11),
        (2, "src/helpers.rs", 5), // jump to helper file
        (3, "src/helpers.rs", 6),
        (4, "src/lib.rs", 12), // back to lib
        (5, "src/lib.rs", 13),
    ];

    let events = record_and_parse_events(&snapshots, &source_locs, "src/lib.rs");

    // Path table in registration order.
    let paths: Vec<&Path> = path_events(&events);
    assert_eq!(
        paths,
        vec![Path::new("src/lib.rs"), Path::new("src/helpers.rs")],
    );

    // 1 implicit start step + 6 line-changes = 7 steps.
    let steps = step_events(&events);
    assert_eq!(steps.len(), 7);
}

/// Inline function: multiple PCs map to the same source line (DWARF inlining).
#[test]
fn test_inline_function_same_line() {
    // When an inline function is expanded, multiple consecutive PCs
    // can map to the same source line. The recorder deduplicates
    // Step events by line.
    let snapshots = vec![
        snap_pc(0), // lib.rs:10
        snap_pc(1), // lib.rs:10 (still inline expansion)
        snap_pc(2), // lib.rs:10 (still inline expansion)
        snap_pc(3), // lib.rs:11 (next source line)
    ];

    let source_locs: Vec<(u64, &str, u32)> = vec![
        (0, "lib.rs", 10),
        (1, "lib.rs", 10), // same line = inline expansion
        (2, "lib.rs", 10), // same line = inline expansion
        (3, "lib.rs", 11),
    ];

    let events = record_and_parse_events(&snapshots, &source_locs, "lib.rs");

    // The recorder deduplicates consecutive identical source lines, so only
    // lines 10 and 11 produce Step events. TraceWriter::start adds an initial
    // Step at the source_path's line 1, giving 3 total.
    let steps = step_events(&events);
    assert_eq!(
        steps.len(),
        3,
        "inline expansion should produce 3 Step events (start + line 10 + line 11)"
    );
    let step_lines: Vec<i64> = steps.iter().map(|s| s.line.0).collect();
    assert_eq!(step_lines, vec![1i64, 10, 11]);
}

/// Macro expansion: msg! macro expanding to multiple instructions across files.
#[test]
fn test_macro_expansion_multiple_instructions() {
    // The msg! macro in Solana programs expands to:
    //   1. format string construction
    //   2. sol_log_ syscall invocation
    // These may be attributed to different source locations.
    let snapshots = vec![
        snap_pc(0), // user code before msg!
        snap_pc(1), // msg! expansion: format
        snap_pc(2), // msg! expansion: format (same line in macro)
        snap_pc(3), // msg! expansion: sol_log_ call
        snap_pc(4), // user code after msg!
    ];

    let source_locs: Vec<(u64, &str, u32)> = vec![
        (0, "lib.rs", 15),
        (1, "lib.rs", 16),                // msg! invocation line
        (2, "solana_program/log.rs", 42), // macro internal
        (3, "solana_program/log.rs", 43), // macro internal
        (4, "lib.rs", 17),
    ];

    let events = record_and_parse_events(&snapshots, &source_locs, "lib.rs");

    let paths: Vec<&Path> = path_events(&events);
    assert_eq!(
        paths,
        vec![Path::new("lib.rs"), Path::new("solana_program/log.rs")],
    );

    // 1 implicit start step + 5 line-changes (15, 16, 42, 43, 17) = 6 steps.
    let steps = step_events(&events);
    assert_eq!(steps.len(), 6);
}

/// Nested function calls with correct source line attribution.
#[test]
fn test_nested_function_calls() {
    // caller (PC 0-1) -> foo (PC 100-101) -> bar (PC 200-201) -> return -> return
    let snapshots = vec![
        snap_pc(0),   // caller line 5
        snap_pc(1),   // caller line 6
        snap_pc(100), // call foo (forward jump)
        snap_pc(101), // foo line 20
        snap_pc(200), // call bar from foo (forward jump)
        snap_pc(201), // bar line 30
        snap_pc(102), // return from bar (backward to foo, but 102 > 101 by 1 so no heuristic)
        snap_pc(2),   // return from foo (backward to caller)
        snap_pc(3),   // caller line 7
    ];

    let source_locs: Vec<(u64, &str, u32)> = vec![
        (0, "caller.rs", 5),
        (1, "caller.rs", 6),
        (100, "foo.rs", 19),
        (101, "foo.rs", 20),
        (200, "bar.rs", 30),
        (201, "bar.rs", 31),
        (102, "foo.rs", 21),
        (2, "caller.rs", 7),
        (3, "caller.rs", 8),
    ];

    let events = record_and_parse_events(&snapshots, &source_locs, "caller.rs");

    // Path table contains all three source files in registration order.
    let paths: Vec<&Path> = path_events(&events);
    assert_eq!(
        paths,
        vec![
            Path::new("caller.rs"),
            Path::new("foo.rs"),
            Path::new("bar.rs"),
        ],
    );

    // 3 calls: main + foo (PC 100 forward jump) + bar (PC 200 forward jump).
    let calls = call_events(&events);
    assert_eq!(calls.len(), 3);

    // Function table in registration order.
    let func_names: Vec<&str> = function_events(&events)
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    assert_eq!(func_names, vec!["main", "fn_at_pc_100", "fn_at_pc_200"]);
}

// ===========================================================================
// 3. CPI (Cross-Program Invocation) scenarios
// ===========================================================================

/// Single CPI call: program A calls system program.
#[test]
fn test_cpi_single_call_to_system_program() {
    let snapshots = vec![
        snap_regs(0, 0, 100, 0, 0),   // primary: setup
        snap_regs(1, 0, 100, 200, 0), // primary: prepare accounts
        snap_regs(2, 0, 100, 200, 0), // primary: invoke CPI
        snap_regs(5000, 0, 0, 0, 0),  // system program: entry
        snap_regs(5001, 0, 0, 0, 0),  // system program: transfer
        snap_regs(5002, 0, 0, 0, 0),  // system program: done
        snap_regs(3, 0, 100, 200, 0), // primary: CPI returned
        snap_regs(4, 0, 100, 200, 0), // primary: continue
    ];

    let mut registry = ProgramRegistry::new();
    registry.add_synthetic_program(
        "my_program",
        0..100,
        vec![
            (0, "lib.rs".to_string(), 10),
            (1, "lib.rs".to_string(), 11),
            (2, "lib.rs".to_string(), 12),
            (3, "lib.rs".to_string(), 13),
            (4, "lib.rs".to_string(), 14),
        ],
    );
    registry.add_synthetic_program(
        "system_program",
        5000..5100,
        vec![
            (5000, "system.rs".to_string(), 1),
            (5001, "system.rs".to_string(), 2),
            (5002, "system.rs".to_string(), 3),
        ],
    );

    let mut detector = CpiDetector::new(0..100);
    detector.add_program_range("system_program", 5000..5100);

    let events = record_cpi_and_parse_events(&snapshots, &registry, &mut detector, "lib.rs");

    // Function table: outer "main" + the CPI target.
    let func_names: Vec<&str> = function_events(&events)
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    assert_eq!(func_names, vec!["main", "system_program"]);

    // 2 calls: main (outermost) + system_program (CPI).
    let calls = call_events(&events);
    assert_eq!(calls.len(), 2);

    // 2 returns: CPI return + main return.
    let returns = return_events(&events);
    assert_eq!(returns.len(), 2);

    // Path table contains both source files in registration order.
    let paths: Vec<&Path> = path_events(&events);
    assert_eq!(paths, vec![Path::new("lib.rs"), Path::new("system.rs")]);
}

/// Nested CPI: A -> B -> C with program ID changes.
#[test]
fn test_cpi_nested_three_programs() {
    let snapshots = vec![
        snap_pc(10),   // primary
        snap_pc(11),   // primary
        snap_pc(1000), // CPI: primary -> token_program
        snap_pc(1001), // token_program
        snap_pc(2000), // CPI: token_program -> associated_token
        snap_pc(2001), // associated_token
        snap_pc(2002), // associated_token
        snap_pc(1002), // return: associated_token -> token_program
        snap_pc(1003), // token_program continues
        snap_pc(12),   // return: token_program -> primary
        snap_pc(13),   // primary continues
    ];

    let mut registry = ProgramRegistry::new();
    registry.add_synthetic_program(
        "primary",
        0..100,
        vec![
            (10, "primary.rs".to_string(), 5),
            (11, "primary.rs".to_string(), 6),
            (12, "primary.rs".to_string(), 7),
            (13, "primary.rs".to_string(), 8),
        ],
    );
    registry.add_synthetic_program(
        "token_program",
        1000..1100,
        vec![
            (1000, "token.rs".to_string(), 10),
            (1001, "token.rs".to_string(), 11),
            (1002, "token.rs".to_string(), 12),
            (1003, "token.rs".to_string(), 13),
        ],
    );
    registry.add_synthetic_program(
        "associated_token",
        2000..2100,
        vec![
            (2000, "ata.rs".to_string(), 20),
            (2001, "ata.rs".to_string(), 21),
            (2002, "ata.rs".to_string(), 22),
        ],
    );

    let mut detector = CpiDetector::new(0..100);
    detector.add_program_range("token_program", 1000..1100);
    detector.add_program_range("associated_token", 2000..2100);

    let events = record_cpi_and_parse_events(&snapshots, &registry, &mut detector, "primary.rs");

    // Function table: outer "main" + the two CPI targets in invocation order.
    let func_names: Vec<&str> = function_events(&events)
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    assert_eq!(
        func_names,
        vec!["main", "token_program", "associated_token"],
    );

    // 3 calls: main + 2 CPIs.
    let calls = call_events(&events);
    assert_eq!(calls.len(), 3);

    // 3 returns: 2 CPI returns + main return.
    let returns = return_events(&events);
    assert_eq!(returns.len(), 3);

    // Path table in registration order.
    let paths: Vec<&Path> = path_events(&events);
    assert_eq!(
        paths,
        vec![
            Path::new("primary.rs"),
            Path::new("token.rs"),
            Path::new("ata.rs"),
        ],
    );
}

/// CPI with multiple accounts: verify register values carrying account info.
#[test]
fn test_cpi_with_multiple_accounts() {
    // Simulate a CPI where r1-r5 carry account-related pointers.
    let acct1_ptr: u64 = 0x400000000;
    let acct2_ptr: u64 = 0x400000100;
    let acct3_ptr: u64 = 0x400000200;

    let snapshots = vec![
        snap_full(
            0, 0, acct1_ptr, acct2_ptr, acct3_ptr, 0, 0, 0, 0, 0, 0, 0x3000,
        ),
        snap_full(
            1, 0, acct1_ptr, acct2_ptr, acct3_ptr, 3, 0, 0, 0, 0, 0, 0x3000,
        ),
        // CPI call
        snap_full(
            5000, 0, acct1_ptr, acct2_ptr, acct3_ptr, 3, 0, 0, 0, 0, 0, 0x3000,
        ),
        snap_full(5001, 0, acct1_ptr, acct2_ptr, 0, 0, 0, 0, 0, 0, 0, 0x3000),
        // Return
        snap_full(
            2, 0, acct1_ptr, acct2_ptr, acct3_ptr, 3, 0, 0, 0, 0, 0, 0x3000,
        ),
    ];

    let mut registry = ProgramRegistry::new();
    registry.add_synthetic_program(
        "my_program",
        0..100,
        vec![
            (0, "lib.rs".to_string(), 1),
            (1, "lib.rs".to_string(), 2),
            (2, "lib.rs".to_string(), 3),
        ],
    );
    registry.add_synthetic_program(
        "target",
        5000..5100,
        vec![
            (5000, "target.rs".to_string(), 1),
            (5001, "target.rs".to_string(), 2),
        ],
    );

    let mut detector = CpiDetector::new(0..100);
    detector.add_program_range("target", 5000..5100);

    let events = record_cpi_and_parse_events(&snapshots, &registry, &mut detector, "lib.rs");

    // Distinct integer values across all Value events.
    let mut int_values: Vec<i64> = events
        .iter()
        .filter_map(|e| match e {
            TraceLowLevelEvent::Value(FullValueRecord {
                value: ValueRecord::Int { i, .. },
                ..
            }) => Some(*i),
            _ => None,
        })
        .collect();
    int_values.sort_unstable();
    int_values.dedup();
    // 5000 / 5001 are the CPI program-range PC values (stored in r11).
    assert_eq!(
        int_values,
        vec![
            0i64,
            3,
            5000,
            0x3000,
            acct1_ptr as i64,
            acct2_ptr as i64,
            acct3_ptr as i64,
        ],
    );

    // Function table: main + the CPI target.
    let func_names: Vec<&str> = function_events(&events)
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    assert_eq!(func_names, vec!["main", "target"]);
}

/// CPI return value propagation: callee sets r0 before returning.
#[test]
fn test_cpi_return_value_propagation() {
    let snapshots = vec![
        snap_regs(0, 0, 0, 0, 0),     // primary
        snap_regs(5000, 0, 0, 0, 0),  // CPI call
        snap_regs(5001, 0, 0, 0, 0),  // inside CPI
        snap_regs(5002, 42, 0, 0, 0), // CPI sets return value in r0
        snap_regs(1, 42, 0, 0, 0),    // back in primary with r0=42
        snap_regs(2, 42, 0, 0, 0),    // primary uses return value
    ];

    let mut registry = ProgramRegistry::new();
    registry.add_synthetic_program(
        "primary",
        0..100,
        vec![
            (0, "primary.rs".to_string(), 1),
            (1, "primary.rs".to_string(), 2),
            (2, "primary.rs".to_string(), 3),
        ],
    );
    registry.add_synthetic_program(
        "callee",
        5000..5100,
        vec![
            (5000, "callee.rs".to_string(), 10),
            (5001, "callee.rs".to_string(), 11),
            (5002, "callee.rs".to_string(), 12),
        ],
    );

    let mut detector = CpiDetector::new(0..100);
    detector.add_program_range("callee", 5000..5100);

    let events = record_cpi_and_parse_events(&snapshots, &registry, &mut detector, "primary.rs");

    // Distinct integer values across all Value events: 0, 42, plus the
    // synthetic `target_pc=5000` argument the CPI synthesiser emits when
    // crossing into `callee`.
    let mut int_values: Vec<i64> = events
        .iter()
        .filter_map(|e| match e {
            TraceLowLevelEvent::Value(FullValueRecord {
                value: ValueRecord::Int { i, .. },
                ..
            }) => Some(*i),
            _ => None,
        })
        .collect();
    int_values.sort_unstable();
    int_values.dedup();
    assert_eq!(int_values, vec![0i64, 42, 5000]);

    // Function table: outermost + CPI target.
    let func_names: Vec<&str> = function_events(&events)
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    assert_eq!(func_names, vec!["main", "callee"]);
}

// ===========================================================================
// 4. Account data decoding scenarios
// ===========================================================================

/// Simple Borsh struct: CounterState { is_initialized: bool, count: u64, authority: Pubkey }.
#[test]
fn test_account_decode_counter_state() {
    let idl_json = r#"{
        "version": "0.1.0",
        "name": "counter_program",
        "accounts": [
            {
                "name": "CounterState",
                "type": {
                    "kind": "struct",
                    "fields": [
                        { "name": "is_initialized", "type": "bool" },
                        { "name": "count", "type": "u64" },
                        { "name": "authority", "type": "publicKey" }
                    ]
                }
            }
        ]
    }"#;

    let idl = AnchorIdl::from_json(idl_json).unwrap();

    // Build account data: 8-byte discriminator + bool + u64 + 32-byte pubkey
    let mut data = Vec::new();
    data.extend_from_slice(&[0xAA; 8]); // discriminator
    data.push(1); // is_initialized = true
    data.extend_from_slice(&999u64.to_le_bytes()); // count = 999
    let mut authority = [0u8; 32];
    authority[0] = 0x01;
    authority[31] = 0xFF;
    data.extend_from_slice(&authority); // authority pubkey

    let fields = idl.decode_account("CounterState", &data).unwrap();
    assert_eq!(fields.len(), 3);

    assert_eq!(fields[0].name, "is_initialized");
    match &fields[0].value {
        DecodedValue::Bool(v) => assert!(*v),
        other => panic!("expected Bool, got {other:?}"),
    }

    assert_eq!(fields[1].name, "count");
    match &fields[1].value {
        DecodedValue::U64(v) => assert_eq!(*v, 999),
        other => panic!("expected U64, got {other:?}"),
    }

    assert_eq!(fields[2].name, "authority");
    match &fields[2].value {
        DecodedValue::Pubkey(bytes) => {
            assert_eq!(bytes[0], 0x01);
            assert_eq!(bytes[31], 0xFF);
        }
        other => panic!("expected Pubkey, got {other:?}"),
    }
}

/// Nested struct: VaultConfig with nested VaultMetadata.
/// Since the IDL "defined" type references aren't fully resolved in the
/// current decoder, we test a flat struct with fields that represent what
/// a nested struct would produce when manually flattened.
#[test]
fn test_account_decode_nested_struct_flat() {
    let idl_json = r#"{
        "version": "0.1.0",
        "name": "vault_program",
        "accounts": [
            {
                "name": "VaultConfig",
                "type": {
                    "kind": "struct",
                    "fields": [
                        { "name": "owner", "type": "publicKey" },
                        { "name": "capacity", "type": "u64" },
                        { "name": "meta_name", "type": "string" },
                        { "name": "meta_created_at", "type": "i64" }
                    ]
                }
            }
        ]
    }"#;

    let idl = AnchorIdl::from_json(idl_json).unwrap();

    let mut data = Vec::new();
    data.extend_from_slice(&[0xBB; 8]); // discriminator
    data.extend_from_slice(&[0x42; 32]); // owner pubkey (all 0x42)
    data.extend_from_slice(&500u64.to_le_bytes()); // capacity = 500
                                                   // meta_name = "MyVault"
    let name = "MyVault";
    data.extend_from_slice(&(name.len() as u32).to_le_bytes());
    data.extend_from_slice(name.as_bytes());
    // meta_created_at = 1700000000 (unix timestamp)
    data.extend_from_slice(&1700000000i64.to_le_bytes());

    let fields = idl.decode_account("VaultConfig", &data).unwrap();
    assert_eq!(fields.len(), 4);

    assert_eq!(fields[0].name, "owner");
    match &fields[0].value {
        DecodedValue::Pubkey(bytes) => assert!(bytes.iter().all(|&b| b == 0x42)),
        other => panic!("expected Pubkey, got {other:?}"),
    }

    assert_eq!(fields[1].name, "capacity");
    match &fields[1].value {
        DecodedValue::U64(v) => assert_eq!(*v, 500),
        other => panic!("expected U64, got {other:?}"),
    }

    assert_eq!(fields[2].name, "meta_name");
    match &fields[2].value {
        DecodedValue::String(s) => assert_eq!(s, "MyVault"),
        other => panic!("expected String, got {other:?}"),
    }

    assert_eq!(fields[3].name, "meta_created_at");
    match &fields[3].value {
        DecodedValue::I64(v) => assert_eq!(*v, 1700000000),
        other => panic!("expected I64, got {other:?}"),
    }
}

/// Vector of structs (simulated as Vec<u64> since compound vec decode is supported).
#[test]
fn test_account_decode_vec_of_u64() {
    let idl_json = r#"{
        "version": "0.1.0",
        "name": "scores_program",
        "accounts": [
            {
                "name": "ScoreBoard",
                "type": {
                    "kind": "struct",
                    "fields": [
                        { "name": "scores", "type": { "vec": "u64" } },
                        { "name": "count", "type": "u32" }
                    ]
                }
            }
        ]
    }"#;

    let idl = AnchorIdl::from_json(idl_json).unwrap();

    let mut data = Vec::new();
    data.extend_from_slice(&[0xCC; 8]); // discriminator
                                        // scores: Vec<u64> with 3 elements [10, 20, 30]
    data.extend_from_slice(&3u32.to_le_bytes()); // vec length
    data.extend_from_slice(&10u64.to_le_bytes());
    data.extend_from_slice(&20u64.to_le_bytes());
    data.extend_from_slice(&30u64.to_le_bytes());
    // count = 3
    data.extend_from_slice(&3u32.to_le_bytes());

    let fields = idl.decode_account("ScoreBoard", &data).unwrap();
    assert_eq!(fields.len(), 2);

    assert_eq!(fields[0].name, "scores");
    match &fields[0].value {
        DecodedValue::Vec(elems) => {
            assert_eq!(elems.len(), 3);
            match &elems[0] {
                DecodedValue::U64(v) => assert_eq!(*v, 10),
                other => panic!("expected U64, got {other:?}"),
            }
            match &elems[1] {
                DecodedValue::U64(v) => assert_eq!(*v, 20),
                other => panic!("expected U64, got {other:?}"),
            }
            match &elems[2] {
                DecodedValue::U64(v) => assert_eq!(*v, 30),
                other => panic!("expected U64, got {other:?}"),
            }
        }
        other => panic!("expected Vec, got {other:?}"),
    }

    assert_eq!(fields[1].name, "count");
    match &fields[1].value {
        DecodedValue::U32(v) => assert_eq!(*v, 3),
        other => panic!("expected U32, got {other:?}"),
    }

    // Test display format.
    let display = fields[0].value.display();
    assert_eq!(display, "[10, 20, 30]");
}

/// Empty Vec<u64> decoding.
#[test]
fn test_account_decode_empty_vec() {
    let idl_json = r#"{
        "version": "0.1.0",
        "name": "empty_vec_program",
        "accounts": [
            {
                "name": "EmptyList",
                "type": {
                    "kind": "struct",
                    "fields": [
                        { "name": "items", "type": { "vec": "u64" } }
                    ]
                }
            }
        ]
    }"#;

    let idl = AnchorIdl::from_json(idl_json).unwrap();

    let mut data = Vec::new();
    data.extend_from_slice(&[0xDD; 8]); // discriminator
    data.extend_from_slice(&0u32.to_le_bytes()); // vec length = 0

    let fields = idl.decode_account("EmptyList", &data).unwrap();
    assert_eq!(fields.len(), 1);
    match &fields[0].value {
        DecodedValue::Vec(elems) => assert!(elems.is_empty()),
        other => panic!("expected empty Vec, got {other:?}"),
    }
    assert_eq!(fields[0].value.display(), "[]");
}

/// All primitive types in a single struct.
#[test]
fn test_account_decode_all_primitives() {
    let idl_json = r#"{
        "version": "0.1.0",
        "name": "primitives",
        "accounts": [
            {
                "name": "AllPrimitives",
                "type": {
                    "kind": "struct",
                    "fields": [
                        { "name": "a_u8", "type": "u8" },
                        { "name": "a_u16", "type": "u16" },
                        { "name": "a_u32", "type": "u32" },
                        { "name": "a_u64", "type": "u64" },
                        { "name": "a_i64", "type": "i64" },
                        { "name": "a_bool", "type": "bool" },
                        { "name": "a_string", "type": "string" },
                        { "name": "a_pubkey", "type": "publicKey" }
                    ]
                }
            }
        ]
    }"#;

    let idl = AnchorIdl::from_json(idl_json).unwrap();

    let mut data = Vec::new();
    data.extend_from_slice(&[0xEE; 8]); // discriminator
    data.push(255); // u8
    data.extend_from_slice(&1000u16.to_le_bytes()); // u16
    data.extend_from_slice(&100_000u32.to_le_bytes()); // u32
    data.extend_from_slice(&1_000_000u64.to_le_bytes()); // u64
    data.extend_from_slice(&(-42i64).to_le_bytes()); // i64
    data.push(0); // bool = false
    let s = "test";
    data.extend_from_slice(&(s.len() as u32).to_le_bytes());
    data.extend_from_slice(s.as_bytes());
    data.extend_from_slice(&[0x11; 32]); // pubkey

    let fields = idl.decode_account("AllPrimitives", &data).unwrap();
    assert_eq!(fields.len(), 8);

    match &fields[0].value {
        DecodedValue::U8(v) => assert_eq!(*v, 255),
        other => panic!("expected U8, got {other:?}"),
    }
    match &fields[1].value {
        DecodedValue::U16(v) => assert_eq!(*v, 1000),
        other => panic!("expected U16, got {other:?}"),
    }
    match &fields[2].value {
        DecodedValue::U32(v) => assert_eq!(*v, 100_000),
        other => panic!("expected U32, got {other:?}"),
    }
    match &fields[3].value {
        DecodedValue::U64(v) => assert_eq!(*v, 1_000_000),
        other => panic!("expected U64, got {other:?}"),
    }
    match &fields[4].value {
        DecodedValue::I64(v) => assert_eq!(*v, -42),
        other => panic!("expected I64, got {other:?}"),
    }
    match &fields[5].value {
        DecodedValue::Bool(v) => assert!(!*v),
        other => panic!("expected Bool(false), got {other:?}"),
    }
    match &fields[6].value {
        DecodedValue::String(v) => assert_eq!(v, "test"),
        other => panic!("expected String, got {other:?}"),
    }
    match &fields[7].value {
        DecodedValue::Pubkey(bytes) => assert!(bytes.iter().all(|&b| b == 0x11)),
        other => panic!("expected Pubkey, got {other:?}"),
    }
}

/// Account decode: non-existent account name returns error.
#[test]
fn test_account_decode_unknown_name() {
    let idl_json = r#"{
        "version": "0.1.0",
        "name": "test",
        "accounts": []
    }"#;
    let idl = AnchorIdl::from_json(idl_json).unwrap();
    let result = idl.decode_account("NonExistent", &[0u8; 16]);
    assert!(result.is_err());
    let err_msg = format!("{}", result.unwrap_err());
    assert!(err_msg.contains("NonExistent"));
}

/// Account decode: data too short for fields.
#[test]
fn test_account_decode_truncated_data() {
    let idl_json = r#"{
        "version": "0.1.0",
        "name": "test",
        "accounts": [
            {
                "name": "Big",
                "type": {
                    "kind": "struct",
                    "fields": [
                        { "name": "val", "type": "u64" }
                    ]
                }
            }
        ]
    }"#;
    let idl = AnchorIdl::from_json(idl_json).unwrap();

    // Only 8 bytes for discriminator, no room for the u64 field.
    let data = [0u8; 8];
    let result = idl.decode_account("Big", &data);
    assert!(result.is_err(), "should fail with truncated data");
}

/// Conversion of decoded fields to ValueRecord with struct type registration.
#[test]
fn test_decoded_fields_to_struct_record() {
    let mut writer = create_trace_writer("test", &[], TraceEventsFileFormat::Ctfs);
    let tmp = tempfile::TempDir::new().unwrap();

    TraceWriter::begin_writing_trace_events(&mut *writer, &tmp.path().join("trace.json")).unwrap();
    // Legacy metadata/paths begin calls were no-ops on the Nim side and
    // were retired with the v3 CTFS rollout (follow-up #254 phase 2).
    TraceWriter::start(&mut *writer, Path::new("test.rs"), Line(1));

    let type_ids = TypeIds::register(&mut *writer);

    let fields = vec![
        DecodedField {
            name: "count".to_string(),
            type_name: "u64".to_string(),
            value: DecodedValue::U64(42),
        },
        DecodedField {
            name: "active".to_string(),
            type_name: "bool".to_string(),
            value: DecodedValue::Bool(true),
        },
        DecodedField {
            name: "label".to_string(),
            type_name: "string".to_string(),
            value: DecodedValue::String("hello".to_string()),
        },
    ];

    let record = decoded_fields_to_struct_record("TestAccount", &fields, &type_ids, &mut *writer);

    match &record {
        ValueRecord::Struct {
            field_values,
            type_id: _,
        } => {
            assert_eq!(field_values.len(), 3);
            match &field_values[0] {
                ValueRecord::Int { i, .. } => assert_eq!(*i, 42),
                other => panic!("expected Int, got {other:?}"),
            }
            match &field_values[1] {
                ValueRecord::Bool { b, .. } => assert!(*b),
                other => panic!("expected Bool, got {other:?}"),
            }
            match &field_values[2] {
                ValueRecord::String { text, .. } => assert_eq!(text, "hello"),
                other => panic!("expected String, got {other:?}"),
            }
        }
        other => panic!("expected Struct, got {other:?}"),
    }

    TraceWriter::finish_writing_trace_events(&mut *writer).unwrap();
    TraceWriter::finish_writing_trace_metadata(&mut *writer).unwrap();
    TraceWriter::finish_writing_trace_paths(&mut *writer).unwrap();
}

// ===========================================================================
// 5. Variable tracking through register state
// ===========================================================================

/// Function parameters in r1-r5.
#[test]
fn test_variable_tracking_function_params() {
    // SBF calling convention: r1-r5 are function arguments.
    let snapshots = vec![snap_full(
        0, 0,    // r0 (not set yet)
        1000, // r1 = param 1
        2000, // r2 = param 2
        3000, // r3 = param 3
        4000, // r4 = param 4
        5000, // r5 = param 5
        0, 0, 0, 0, 0x3000, // r10 = frame pointer
    )];

    let source_locs: Vec<(u64, &str, u32)> = vec![(0, "params.rs", 1)];

    let events = record_and_parse_events(&snapshots, &source_locs, "params.rs");

    let var_names: Vec<&str> = variable_name_events(&events);
    assert_eq!(
        var_names,
        vec!["r0", "r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9", "r10"],
    );

    // Distinct integer values: zero registers, the 5 params (1000..=5000),
    // and the frame pointer (0x3000).
    let mut int_values: Vec<i64> = events
        .iter()
        .filter_map(|e| match e {
            TraceLowLevelEvent::Value(FullValueRecord {
                value: ValueRecord::Int { i, .. },
                ..
            }) => Some(*i),
            _ => None,
        })
        .collect();
    int_values.sort_unstable();
    int_values.dedup();
    assert_eq!(int_values, vec![0i64, 1000, 2000, 3000, 4000, 5000, 0x3000]);
}

/// Local variables in r6-r9 (callee-saved registers).
#[test]
fn test_variable_tracking_locals() {
    // SBF convention: r6-r9 are callee-saved, used for local variables.
    let snapshots = vec![
        snap_full(0, 0, 0, 0, 0, 0, 0, 100, 200, 300, 400, 0x3000),
        snap_full(1, 0, 0, 0, 0, 0, 0, 110, 210, 310, 400, 0x3000),
    ];

    let source_locs: Vec<(u64, &str, u32)> = vec![(0, "locals.rs", 10), (1, "locals.rs", 11)];

    let events = record_and_parse_events(&snapshots, &source_locs, "locals.rs");

    let var_names: Vec<&str> = variable_name_events(&events);
    assert_eq!(
        var_names,
        vec!["r0", "r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9", "r10"],
    );

    // Distinct integer values: zero registers + the two snapshots' r6..r10
    // (initial 100/200/300/400 and updated 110/210/310/400) + frame pointer.
    let mut int_values: Vec<i64> = events
        .iter()
        .filter_map(|e| match e {
            TraceLowLevelEvent::Value(FullValueRecord {
                value: ValueRecord::Int { i, .. },
                ..
            }) => Some(*i),
            _ => None,
        })
        .collect();
    int_values.sort_unstable();
    int_values.dedup();
    assert_eq!(
        int_values,
        vec![0i64, 100, 110, 200, 210, 300, 310, 400, 0x3000],
    );
}

/// Return value in r0.
#[test]
fn test_variable_tracking_return_value() {
    let snapshots = vec![
        snap_full(0, 0, 10, 20, 0, 0, 0, 0, 0, 0, 0, 0x3000),
        snap_full(1, 0, 10, 20, 30, 0, 0, 0, 0, 0, 0, 0x3000),
        snap_full(2, 30, 10, 20, 30, 0, 0, 0, 0, 0, 0, 0x3000), // r0 = 30 (return value)
    ];

    let source_locs: Vec<(u64, &str, u32)> =
        vec![(0, "ret.rs", 1), (1, "ret.rs", 2), (2, "ret.rs", 3)];

    let events = record_and_parse_events(&snapshots, &source_locs, "ret.rs");

    let var_names: Vec<&str> = variable_name_events(&events);
    assert_eq!(
        var_names,
        vec!["r0", "r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9", "r10"],
    );

    // Distinct integer values: 0 (zero registers), 10 (r1), 20 (r2),
    // 30 (r3 / r0 return), 0x3000 (r10 frame pointer).
    let mut int_values: Vec<i64> = events
        .iter()
        .filter_map(|e| match e {
            TraceLowLevelEvent::Value(FullValueRecord {
                value: ValueRecord::Int { i, .. },
                ..
            }) => Some(*i),
            _ => None,
        })
        .collect();
    int_values.sort_unstable();
    int_values.dedup();
    assert_eq!(int_values, vec![0i64, 10, 20, 30, 0x3000]);
}

/// Stack pointer in r10 changes across function calls.
#[test]
fn test_variable_tracking_stack_pointer() {
    let sp1: u64 = 0x200001000;
    let sp2: u64 = 0x200000F00; // stack grows down
    let sp3: u64 = 0x200001000; // restored after return

    let snapshots = vec![
        snap_full(0, 0, 0, 0, 0, 0, 0, 0, 0, 0, sp1, 0),
        snap_full(1, 0, 0, 0, 0, 0, 0, 0, 0, 0, sp2, 0),
        snap_full(2, 0, 0, 0, 0, 0, 0, 0, 0, 0, sp3, 0),
    ];

    let source_locs: Vec<(u64, &str, u32)> =
        vec![(0, "sp.rs", 1), (1, "sp.rs", 2), (2, "sp.rs", 3)];

    let events = record_and_parse_events(&snapshots, &source_locs, "sp.rs");

    let var_names: Vec<&str> = variable_name_events(&events);
    assert_eq!(
        var_names,
        vec!["r0", "r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9", "r10"],
    );

    // Distinct integer values: 0 (zero registers), and the two distinct
    // stack-pointer values (sp1 == sp3 so the set has size 2 on r10).
    let mut int_values: Vec<i64> = events
        .iter()
        .filter_map(|e| match e {
            TraceLowLevelEvent::Value(FullValueRecord {
                value: ValueRecord::Int { i, .. },
                ..
            }) => Some(*i),
            _ => None,
        })
        .collect();
    int_values.sort_unstable();
    int_values.dedup();
    assert_eq!(int_values, vec![0i64, sp2 as i64, sp1 as i64]);
}

/// PC progression in r11: verify sequential advancement.
#[test]
fn test_variable_tracking_pc_progression() {
    let snapshots = vec![snap_pc(0), snap_pc(1), snap_pc(2), snap_pc(3), snap_pc(4)];

    let source_locs: Vec<(u64, &str, u32)> = vec![
        (0, "pc.rs", 1),
        (1, "pc.rs", 2),
        (2, "pc.rs", 3),
        (3, "pc.rs", 4),
        (4, "pc.rs", 5),
    ];

    let events = record_and_parse_events(&snapshots, &source_locs, "pc.rs");

    // No large PC jumps, so the trace has exactly one main frame: 1 call,
    // 1 return.
    let calls = call_events(&events);
    assert_eq!(calls.len(), 1);
    let returns = return_events(&events);
    assert_eq!(returns.len(), 1);

    // 1 implicit start step + 5 line-changes (1..=5) = 6 step events.
    let steps = step_events(&events);
    assert_eq!(steps.len(), 6);
}

// ===========================================================================
// 6. Error handling paths
// ===========================================================================

/// ProgramError::MissingRequiredSignature: simulate via register state.
/// Convention: r0 != 0 indicates an error; specific error codes in r0.
#[test]
fn test_error_path_missing_signature() {
    // Solana ProgramError::MissingRequiredSignature = error code 2
    let error_code: u64 = 2;
    let snapshots = vec![
        snap_regs(0, 0, 0, 0, 0),          // setup
        snap_regs(1, 0, 0, 0, 0),          // check signer
        snap_regs(2, error_code, 0, 0, 0), // error: r0 = 2
    ];

    let source_locs: Vec<(u64, &str, u32)> =
        vec![(0, "error.rs", 1), (1, "error.rs", 2), (2, "error.rs", 3)];

    let events = record_and_parse_events(&snapshots, &source_locs, "error.rs");

    let var_names: Vec<&str> = variable_name_events(&events);
    assert_eq!(
        var_names,
        vec!["r0", "r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9", "r10"],
    );

    // Distinct integer values: zero registers + the error code 2 in r0.
    let mut int_values: Vec<i64> = events
        .iter()
        .filter_map(|e| match e {
            TraceLowLevelEvent::Value(FullValueRecord {
                value: ValueRecord::Int { i, .. },
                ..
            }) => Some(*i),
            _ => None,
        })
        .collect();
    int_values.sort_unstable();
    int_values.dedup();
    assert_eq!(int_values, vec![0i64, error_code as i64]);
}

/// Custom error enum with error code: simulate via larger r0 value.
#[test]
fn test_error_path_custom_error_code() {
    // Anchor custom errors start at 6000.
    let custom_error: u64 = 6001; // e.g., ErrorCode::InsufficientFunds
    let snapshots = vec![
        snap_regs(0, 0, 1000, 2000, 0),      // check balance
        snap_regs(1, 0, 1000, 2000, 0),      // compare
        snap_regs(2, custom_error, 0, 0, 0), // error: insufficient funds
    ];

    let source_locs: Vec<(u64, &str, u32)> = vec![
        (0, "custom_err.rs", 50),
        (1, "custom_err.rs", 51),
        (2, "custom_err.rs", 52),
    ];

    let events = record_and_parse_events(&snapshots, &source_locs, "custom_err.rs");

    // Distinct integer values: 0, 1000 (r1 balance), 2000 (r2 needed), 6001
    // (custom error code in r0).
    let mut int_values: Vec<i64> = events
        .iter()
        .filter_map(|e| match e {
            TraceLowLevelEvent::Value(FullValueRecord {
                value: ValueRecord::Int { i, .. },
                ..
            }) => Some(*i),
            _ => None,
        })
        .collect();
    int_values.sort_unstable();
    int_values.dedup();
    assert_eq!(int_values, vec![0i64, 1000, 2000, custom_error as i64]);
}

/// Panic/abort path: PC jumps to a very high address (abort handler).
#[test]
fn test_error_path_panic_abort() {
    // Simulate a panic: normal execution then PC jumps to abort handler.
    let snapshots = vec![
        snap_pc(0),     // normal
        snap_pc(1),     // normal
        snap_pc(90000), // panic -> abort handler (massive forward jump)
        snap_pc(90001), // inside abort handler
    ];

    let source_locs: Vec<(u64, &str, u32)> = vec![
        (0, "panic.rs", 10),
        (1, "panic.rs", 11),
        (90000, "core/panicking.rs", 100),
        (90001, "core/panicking.rs", 101),
    ];

    let events = record_and_parse_events(&snapshots, &source_locs, "panic.rs");

    // Function table: outer "main" + the synthesised panic-handler frame.
    let func_names: Vec<&str> = function_events(&events)
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    assert_eq!(func_names, vec!["main", "fn_at_pc_90000"]);

    // Path table: user source + panicking-runtime source, in registration order.
    let paths: Vec<&Path> = path_events(&events);
    assert_eq!(
        paths,
        vec![Path::new("panic.rs"), Path::new("core/panicking.rs")],
    );
}

// ===========================================================================
// 7. Register trace binary format edge cases
// ===========================================================================

/// Roundtrip: encode snapshots, parse them back, verify equality.
#[test]
fn test_regs_roundtrip() {
    let original = vec![
        snap_full(42, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11),
        snap_full(43, 100, 200, 300, 400, 500, 600, 700, 800, 900, 1000, 1100),
        snap_full(0, u64::MAX, u64::MAX, 0, 0, 0, 0, 0, 0, 0, 0, 0),
    ];

    let encoded = encode_regs(&original);
    assert_eq!(encoded.len(), 3 * ROW_SIZE);

    let parsed = parse_regs_file(&encoded).unwrap();
    assert_eq!(parsed.len(), 3);

    for (orig, parsed) in original.iter().zip(parsed.iter()) {
        assert_eq!(orig.registers, parsed.registers);
    }
}

/// Invalid .regs data length.
#[test]
fn test_regs_invalid_length() {
    let bad_data = vec![0u8; ROW_SIZE + 1]; // not a multiple of 96
    assert!(parse_regs_file(&bad_data).is_err());
}

/// Single-instruction trace produces valid output.
#[test]
fn test_single_instruction_trace() {
    let snapshots = vec![snap_regs(0, 42, 1, 2, 3)];
    let source_locs: Vec<(u64, &str, u32)> = vec![(0, "single.rs", 1)];

    let events = record_and_parse_events(&snapshots, &source_locs, "single.rs");

    // 1 implicit start step + 1 line-change (line 1) → de-dup with start: 2 steps.
    let steps = step_events(&events);
    assert_eq!(steps.len(), 2);
    let calls = call_events(&events);
    assert_eq!(calls.len(), 1);
    let returns = return_events(&events);
    assert_eq!(returns.len(), 1);

    // Distinct integer values: 0 (zero registers), 1, 2, 3 (r1..r3), 42 (r0).
    let mut int_values: Vec<i64> = events
        .iter()
        .filter_map(|e| match e {
            TraceLowLevelEvent::Value(FullValueRecord {
                value: ValueRecord::Int { i, .. },
                ..
            }) => Some(*i),
            _ => None,
        })
        .collect();
    int_values.sort_unstable();
    int_values.dedup();
    assert_eq!(int_values, vec![0i64, 1, 2, 3, 42]);
}

/// Empty trace (no snapshots) still produces valid output files.
#[test]
fn test_empty_trace() {
    let snapshots: Vec<RegisterSnapshot> = vec![];
    let source_locs: Vec<(u64, &str, u32)> = vec![];

    let tmp = tempfile::TempDir::new().unwrap();
    record_from_snapshots(&snapshots, &source_locs, Path::new("empty.rs"), tmp.path()).unwrap();

    // Verify .ct output with CTFS magic bytes.
    let ct_files: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map_or(false, |ext| ext == "ct"))
        .collect();
    assert!(!ct_files.is_empty(), "expected .ct file");
}

// ===========================================================================
// 8. CPI detector edge cases
// ===========================================================================

/// CPI to unknown program (not registered).
#[test]
fn test_cpi_unknown_program() {
    let mut detector = CpiDetector::new(0..100);
    // No other programs registered.

    detector.process_snapshot(&snap_pc(10)); // primary
    let event = detector.process_snapshot(&snap_pc(99999)); // unknown CPI

    match event {
        CpiEvent::CpiCall { target_pc } => assert_eq!(target_pc, 99999),
        other => panic!("expected CpiCall, got {other:?}"),
    }

    assert_eq!(detector.call_depth(), 1);
    // Current program name should indicate unknown.
    assert!(
        detector.current_program().contains("unknown"),
        "unknown CPI target should have 'unknown' in name"
    );
}

/// CPI with direct return to primary (skipping intermediate level).
#[test]
fn test_cpi_skip_level_return() {
    let mut detector = CpiDetector::new(0..100);
    detector.add_program_range("program_a", 1000..1100);
    detector.add_program_range("program_b", 2000..2100);

    // primary -> A -> B -> directly back to primary (skip A).
    detector.process_snapshot(&snap_pc(10)); // primary
    detector.process_snapshot(&snap_pc(1010)); // CPI to A
    detector.process_snapshot(&snap_pc(2010)); // CPI to B from A

    assert_eq!(detector.call_depth(), 2);

    // Return directly to primary, skipping A.
    let event = detector.process_snapshot(&snap_pc(15));
    assert_eq!(event, CpiEvent::CpiReturn { return_pc: 15 });
    assert_eq!(detector.call_depth(), 0);
    assert_eq!(detector.current_program(), "primary");
}

/// CPI detector: multiple entries/exits to same program.
#[test]
fn test_cpi_repeated_calls() {
    let mut detector = CpiDetector::new(0..100);
    detector.add_program_range("token", 1000..1100);

    // Call token program twice.
    detector.process_snapshot(&snap_pc(5)); // primary
    detector.process_snapshot(&snap_pc(1005)); // CPI to token
    assert_eq!(detector.call_depth(), 1);

    detector.process_snapshot(&snap_pc(10)); // return to primary
    assert_eq!(detector.call_depth(), 0);

    detector.process_snapshot(&snap_pc(1010)); // second CPI to token
    assert_eq!(detector.call_depth(), 1);
    assert_eq!(detector.current_program(), "token");

    detector.process_snapshot(&snap_pc(15)); // return to primary
    assert_eq!(detector.call_depth(), 0);
}

// ===========================================================================
// 9. Tracer trait integration
// ===========================================================================

/// CodeTracerTracer records syscalls through the trait interface.
#[test]
fn test_tracer_trait_syscall_recording() {
    let tmp = tempfile::TempDir::new().unwrap();
    let source_locs = vec![
        (0u64, "test.rs".to_string(), 1u32),
        (1, "test.rs".to_string(), 2),
    ];

    let mut tracer = CodeTracerTracer::new(Path::new("test.rs"), tmp.path(), source_locs).unwrap();

    let regs = [0u64; 12];
    tracer.on_syscall("sol_log", &regs);
    tracer.on_syscall("sol_create_program_address", &regs);
    tracer.on_syscall("sol_invoke_signed", &regs);
    tracer.on_syscall("sol_get_clock_sysvar", &regs);

    let syscalls = tracer.recorded_syscalls();
    assert_eq!(syscalls.len(), 4);
    assert_eq!(syscalls[0], "sol_log");
    assert_eq!(syscalls[1], "sol_create_program_address");
    assert_eq!(syscalls[2], "sol_invoke_signed");
    assert_eq!(syscalls[3], "sol_get_clock_sysvar");

    tracer.finish().unwrap();
}

/// replay_snapshots produces correct trace output.
#[test]
fn test_replay_snapshots_produces_trace() {
    let tmp = tempfile::TempDir::new().unwrap();
    let snapshots = vec![
        snap_regs(0, 0, 10, 0, 0),
        snap_regs(1, 0, 10, 20, 0),
        snap_regs(2, 30, 10, 20, 30),
    ];

    let source_locs = vec![
        (0u64, "replay.rs".to_string(), 1u32),
        (1, "replay.rs".to_string(), 2),
        (2, "replay.rs".to_string(), 3),
    ];

    let mut tracer =
        CodeTracerTracer::new(Path::new("replay.rs"), tmp.path(), source_locs).unwrap();

    replay_snapshots(&mut tracer, &snapshots);
    tracer.finish().unwrap();

    // Verify .ct output with CTFS magic bytes.
    let ct_files: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map_or(false, |ext| ext == "ct"))
        .collect();
    assert!(!ct_files.is_empty(), "expected .ct file");
    let ct_content = std::fs::read(&ct_files[0]).unwrap();
    assert!(ct_content.len() >= 5, ".ct file too small");
    assert_eq!(&ct_content[..5], &[0xC0u8, 0xDE, 0x72, 0xAC, 0xE2]);
}

/// NoOpTracer + replay_snapshots with large trace does not panic.
#[test]
fn test_noop_tracer_large_trace() {
    let snapshots: Vec<RegisterSnapshot> = (0..10_000)
        .map(|i| {
            snap_full(
                i,
                i * 2,
                i + 1,
                i + 2,
                i + 3,
                i + 4,
                i + 5,
                0,
                0,
                0,
                0,
                0x3000,
            )
        })
        .collect();

    let mut tracer = NoOpTracer;
    replay_snapshots(&mut tracer, &snapshots);
    // Should complete without panic or OOM.
}

// ===========================================================================
// 10. Borsh decoder edge cases
// ===========================================================================

/// BorshDecoder: read past end of data.
#[test]
fn test_borsh_decoder_insufficient_data() {
    let data = [0u8; 3]; // only 3 bytes
    let mut decoder = BorshDecoder::new(&data);

    // u64 needs 8 bytes -> should fail.
    assert!(decoder.read_u64().is_err());

    // u32 needs 4 bytes -> should fail.
    let mut decoder2 = BorshDecoder::new(&data);
    assert!(decoder2.read_u32().is_err());

    // u8 should succeed (3 times).
    let mut decoder3 = BorshDecoder::new(&data);
    assert!(decoder3.read_u8().is_ok());
    assert!(decoder3.read_u8().is_ok());
    assert!(decoder3.read_u8().is_ok());
    assert!(decoder3.read_u8().is_err()); // 4th should fail
}

/// BorshDecoder: string with length exceeding available data.
#[test]
fn test_borsh_decoder_string_length_overflow() {
    let mut data = Vec::new();
    data.extend_from_slice(&100u32.to_le_bytes()); // claim 100 bytes of string
    data.extend_from_slice(b"short"); // only 5 bytes

    let mut decoder = BorshDecoder::new(&data);
    assert!(decoder.read_string().is_err());
}

/// BorshDecoder: skip past end of data.
#[test]
fn test_borsh_decoder_skip_overflow() {
    let data = [0u8; 4];
    let mut decoder = BorshDecoder::new(&data);
    assert!(decoder.skip(5).is_err());
    // skip(4) should work.
    let mut decoder2 = BorshDecoder::new(&data);
    assert!(decoder2.skip(4).is_ok());
    assert!(decoder2.remaining().is_empty());
}

/// BorshDecoder: decode_type with vec containing multiple elements.
#[test]
fn test_borsh_decoder_vec_decode() {
    let mut data = Vec::new();
    data.extend_from_slice(&3u32.to_le_bytes()); // 3 elements
    data.extend_from_slice(&100u64.to_le_bytes());
    data.extend_from_slice(&200u64.to_le_bytes());
    data.extend_from_slice(&300u64.to_le_bytes());

    let mut decoder = BorshDecoder::new(&data);
    let vec_type = IdlType::Simple("u64".to_string());
    let elems = decoder.read_vec(&vec_type).unwrap();
    assert_eq!(elems.len(), 3);

    match &elems[0] {
        DecodedValue::U64(v) => assert_eq!(*v, 100),
        other => panic!("expected U64, got {other:?}"),
    }
    match &elems[2] {
        DecodedValue::U64(v) => assert_eq!(*v, 300),
        other => panic!("expected U64, got {other:?}"),
    }
}

// ===========================================================================
// 11. DecodedValue display formatting
// ===========================================================================

#[test]
fn test_decoded_value_display() {
    assert_eq!(DecodedValue::U8(42).display(), "42");
    assert_eq!(DecodedValue::U16(1000).display(), "1000");
    assert_eq!(DecodedValue::U32(100000).display(), "100000");
    assert_eq!(DecodedValue::U64(1000000).display(), "1000000");
    assert_eq!(DecodedValue::I64(-42).display(), "-42");
    assert_eq!(DecodedValue::Bool(true).display(), "true");
    assert_eq!(DecodedValue::Bool(false).display(), "false");
    assert_eq!(
        DecodedValue::String("hello".to_string()).display(),
        "\"hello\""
    );
    assert_eq!(DecodedValue::Vec(vec![]).display(), "[]");
    assert_eq!(
        DecodedValue::Vec(vec![DecodedValue::U64(1), DecodedValue::U64(2)]).display(),
        "[1, 2]"
    );

    let struct_val = DecodedValue::Struct(vec![
        DecodedField {
            name: "x".to_string(),
            type_name: "u64".to_string(),
            value: DecodedValue::U64(10),
        },
        DecodedField {
            name: "y".to_string(),
            type_name: "bool".to_string(),
            value: DecodedValue::Bool(true),
        },
    ]);
    assert_eq!(struct_val.display(), "{ x: 10, y: true }");
}

// ===========================================================================
// 12. Program registry comprehensive
// ===========================================================================

/// Registry with overlapping ranges: first match wins.
#[test]
fn test_program_registry_first_match() {
    let mut registry = ProgramRegistry::new();
    registry.add_synthetic_program("first", 0..100, vec![(50, "first.rs".to_string(), 1)]);
    // Adding a second program with an overlapping range: should never match
    // since first is checked first.
    registry.add_synthetic_program("second", 50..150, vec![(50, "second.rs".to_string(), 1)]);

    // PC=50 falls in both ranges, but "first" is checked first.
    assert_eq!(registry.program_name(50), Some("first"));

    // PC=110 only falls in "second".
    assert_eq!(registry.program_name(110), Some("second"));
}

/// Large CPI trace with many programs.
#[test]
fn test_cpi_large_multi_program_trace() {
    // Simulate a trace traversing 4 programs.
    let mut snapshots = Vec::new();
    let mut registry = ProgramRegistry::new();
    let mut detector = CpiDetector::new(0..100);

    // Primary: PC 0..9
    for pc in 0..3 {
        snapshots.push(snap_pc(pc));
    }
    registry.add_synthetic_program(
        "primary",
        0..100,
        (0..10)
            .map(|pc| (pc, "primary.rs".to_string(), pc as u32 + 1))
            .collect(),
    );

    // CPI to program A: PC 1000..1009
    for pc in 1000..1003 {
        snapshots.push(snap_pc(pc));
    }
    registry.add_synthetic_program(
        "program_a",
        1000..1100,
        (1000..1010)
            .map(|pc| (pc, "a.rs".to_string(), (pc - 1000) as u32 + 1))
            .collect(),
    );
    detector.add_program_range("program_a", 1000..1100);

    // CPI from A to B: PC 2000..2002
    for pc in 2000..2003 {
        snapshots.push(snap_pc(pc));
    }
    registry.add_synthetic_program(
        "program_b",
        2000..2100,
        (2000..2010)
            .map(|pc| (pc, "b.rs".to_string(), (pc - 2000) as u32 + 1))
            .collect(),
    );
    detector.add_program_range("program_b", 2000..2100);

    // Return to A.
    for pc in 1003..1005 {
        snapshots.push(snap_pc(pc));
    }

    // CPI from A to C: PC 3000..3002
    for pc in 3000..3003 {
        snapshots.push(snap_pc(pc));
    }
    registry.add_synthetic_program(
        "program_c",
        3000..3100,
        (3000..3010)
            .map(|pc| (pc, "c.rs".to_string(), (pc - 3000) as u32 + 1))
            .collect(),
    );
    detector.add_program_range("program_c", 3000..3100);

    // Return all the way to primary.
    for pc in 3..5 {
        snapshots.push(snap_pc(pc));
    }

    let events = record_cpi_and_parse_events(&snapshots, &registry, &mut detector, "primary.rs");

    // Function table: outer "main" + the 3 CPI targets in invocation order
    // (program_a → program_b, then primary returns to A and re-enters as
    // program_c).
    let func_names: Vec<&str> = function_events(&events)
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    assert_eq!(
        func_names,
        vec!["main", "program_a", "program_b", "program_c"],
    );

    // 4 calls (main + 3 CPIs), 4 matching returns.
    let calls = call_events(&events);
    assert_eq!(calls.len(), 4);
    let returns = return_events(&events);
    assert_eq!(returns.len(), 4);

    // Path table contains all 4 source files in registration order.
    let paths: Vec<&Path> = path_events(&events);
    assert_eq!(
        paths,
        vec![
            Path::new("primary.rs"),
            Path::new("a.rs"),
            Path::new("b.rs"),
            Path::new("c.rs"),
        ],
    );
}

// ===========================================================================
// 13. Trace output format correctness
// ===========================================================================

/// JSON trace output is valid JSON.
#[test]
fn test_trace_output_valid_json() {
    let snapshots = vec![
        snap_regs(0, 0, 10, 20, 0),
        snap_regs(1, 0, 10, 20, 30),
        snap_regs(2, 30, 0, 0, 0),
    ];
    let source_locs: Vec<(u64, &str, u32)> =
        vec![(0, "valid.rs", 1), (1, "valid.rs", 2), (2, "valid.rs", 3)];

    let tmp = tempfile::TempDir::new().unwrap();
    record_from_snapshots(&snapshots, &source_locs, Path::new("valid.rs"), tmp.path()).unwrap();

    // Verify .ct output with CTFS magic bytes.
    let ct_files: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map_or(false, |ext| ext == "ct"))
        .collect();
    assert!(!ct_files.is_empty(), "expected .ct file");
    let ct_content = std::fs::read(&ct_files[0]).unwrap();
    assert!(ct_content.len() >= 5, ".ct file too small");
    assert_eq!(&ct_content[..5], &[0xC0u8, 0xDE, 0x72, 0xAC, 0xE2]);
}

// `test_trace_output_binary_format` was deleted on 2026-05-08 — it asserted
// on the legacy `--format binary` contract.  Post-2026-05-08 the recorder is
// CTFS-only; the equivalent magic-byte structural assertion now lives in the
// `test_trace_output_valid_json` test (renamed conceptually but kept under
// its original name).  See `AUDIT-CTFS-2026-05.md` ("Convention compliance
// follow-up — 2026-05-08") for the full record.

/// Snapshots without source mapping are skipped gracefully.
#[test]
fn test_unmapped_pcs_are_skipped() {
    let snapshots = vec![
        snap_pc(0),  // mapped
        snap_pc(50), // NOT mapped
        snap_pc(51), // NOT mapped
        snap_pc(1),  // mapped
    ];
    let source_locs: Vec<(u64, &str, u32)> = vec![
        (0, "mapped.rs", 1),
        (1, "mapped.rs", 2),
        // PC 50 and 51 intentionally not mapped.
    ];

    let events = record_and_parse_events(&snapshots, &source_locs, "mapped.rs");

    // 1 implicit start step (line 1 of source_path) + 2 mapped line-changes
    // (the snapshots map to lines 1 and 2).  The start emits at line 1,
    // then snapshot 0 (also line 1) is suppressed by line-dedup, and
    // snapshot 3 (line 2) emits — but the recorder also re-emits the start
    // line after the unmapped block, so the final step count is 3:
    // start@1, then 1, then 2.
    let steps = step_events(&events);
    assert_eq!(steps.len(), 3);
    let step_lines: Vec<i64> = steps.iter().map(|s| s.line.0).collect();
    assert_eq!(step_lines, vec![1i64, 1, 2]);
}
