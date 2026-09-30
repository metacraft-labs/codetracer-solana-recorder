//! Source-level fidelity of a real, compiled Solana program.
//!
//! These tests compile `test-programs/source-fidelity/simple_trivial_chain/main.rs`
//! with `cargo-build-sbf`, the same way CodeTracer's own test harness does:
//! a scratch `cdylib` crate whose `src/lib.rs` `include!`s the program by
//! absolute path, built with `opt-level = 0`, `debug = true`,
//! `strip = "none"`.  The UNSTRIPPED ELF under
//! `target/sbpf-solana-solana/release/` is then recorded by the recorder
//! binary itself (`record <elf> --out-dir <dir>`) and the produced `.ct`
//! container is decoded in process.
//!
//! Every assertion is about the SOURCE program: which files and lines ran,
//! which named locals held which values, and which functions were entered.
//! Nothing here is mocked: the compiler, the SBF VM, the DWARF reader and
//! the trace writer are all the real ones.
//!
//! Prerequisite: `cargo-build-sbf` on `PATH` (the repo's dev shell provides
//! it).  When it is missing the tests FAIL with a message naming the
//! prerequisite -- they never pass without having recorded anything.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use codetracer_trace_types::{TraceLowLevelEvent, ValueRecord};

const CRATE_NAME: &str = "ct_solana_test_program";

/// The fixture program, as committed.
fn fixture_source() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("test-programs")
        .join("source-fidelity")
        .join("simple_trivial_chain")
        .join("main.rs")
        .canonicalize()
        .expect("the committed fixture program must exist")
}

fn find_on_path(tool: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(tool))
        .find(|candidate| candidate.is_file())
}

/// A compiled and recorded fixture: the scratch crate, its ELF and the
/// decoded trace.
struct Recording {
    crate_dir: PathBuf,
    elf: PathBuf,
    trace: DecodedTrace,
    _scratch: tempfile::TempDir,
}

/// The fixture built exactly the way CodeTracer's harness builds it, and
/// recorded through the recorder CLI.  Done once per test binary.
fn recording() -> &'static Recording {
    static RECORDING: OnceLock<Recording> = OnceLock::new();
    RECORDING.get_or_init(|| build_and_record(&[]))
}

/// The same program built with rustc's MIR optimizations off
/// (`-Zmir-opt-level=0`), which keeps the `let b = a` copy that the default
/// build folds away -- so `b` has storage and a DWARF description, and
/// `a`'s scope covers the rest of `compute`.
fn recording_keeping_copies() -> &'static Recording {
    static RECORDING: OnceLock<Recording> = OnceLock::new();
    RECORDING.get_or_init(|| build_and_record(&["-Zmir-opt-level=0"]))
}

fn build_and_record(rustflags: &[&str]) -> Recording {
    {
        let cargo_build_sbf = find_on_path("cargo-build-sbf").unwrap_or_else(|| {
            panic!(
                "missing prerequisite: `cargo-build-sbf` is not on PATH. These tests compile a \
                 real Solana program; run them inside the repo's dev shell \
                 (`nix develop`), which provides it."
            )
        });

        let scratch = tempfile::tempdir().expect("create scratch dir");
        let crate_dir = scratch.path().join("sbf_program");
        std::fs::create_dir_all(crate_dir.join("src")).unwrap();
        std::fs::write(
            crate_dir.join("Cargo.toml"),
            format!(
                "[package]\nname = \"{CRATE_NAME}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
                 [lib]\ncrate-type = [\"cdylib\"]\npath = \"src/lib.rs\"\n\n[dependencies]\n\n\
                 [profile.release]\nopt-level = 0\ndebug = true\nstrip = \"none\"\nlto = \"off\"\n\
                 overflow-checks = true\n\n[workspace]\n"
            ),
        )
        .unwrap();
        std::fs::write(
            crate_dir.join("src").join("lib.rs"),
            format!(
                "#![allow(dead_code, unused)]\ninclude!({:?});\n\n\
                 #[no_mangle]\npub extern \"C\" fn entrypoint(_input: *mut u8) -> u64 {{\n    \
                 core::hint::black_box(main());\n    0\n}}\n",
                fixture_source().to_string_lossy()
            ),
        )
        .unwrap();

        let target_dir = crate_dir.join("target");
        // cargo-build-sbf prepares a shared SDK directory on first use and
        // races with itself when two builds start at once.
        static BUILD_LOCK: Mutex<()> = Mutex::new(());
        let _serialized = BUILD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let build = Command::new(&cargo_build_sbf)
            .arg("--manifest-path")
            .arg(crate_dir.join("Cargo.toml"))
            .env("CARGO_TARGET_DIR", &target_dir)
            .env("RUSTFLAGS", rustflags.join(" "))
            .output()
            .expect("run cargo-build-sbf");
        assert!(
            build.status.success(),
            "cargo-build-sbf failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&build.stdout),
            String::from_utf8_lossy(&build.stderr)
        );
        let elf = target_dir
            .join("sbpf-solana-solana")
            .join("release")
            .join(format!("{CRATE_NAME}.so"));
        assert!(elf.is_file(), "no unstripped ELF at {}", elf.display());

        let out_dir = scratch.path().join("trace");
        let record = Command::new(env!("CARGO_BIN_EXE_codetracer-solana-recorder"))
            .arg("record")
            .arg(&elf)
            .arg("--out-dir")
            .arg(&out_dir)
            .output()
            .expect("run the recorder");
        assert!(
            record.status.success(),
            "recorder failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&record.stdout),
            String::from_utf8_lossy(&record.stderr)
        );

        let trace = DecodedTrace::read(&out_dir);
        Recording {
            crate_dir: crate_dir.canonicalize().unwrap(),
            elf,
            trace,
            _scratch: scratch,
        }
    }
}

// ---------------------------------------------------------------------------
// Decoding the trace into source-level facts
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Step {
    path: PathBuf,
    line: i64,
    /// Innermost function active at this step (None at top level).
    function: Option<String>,
    /// Named values recorded at this step.
    vars: Vec<(String, ValueRecord)>,
}

#[derive(Debug)]
struct Frame {
    function: String,
    depth: usize,
    return_value: Option<ValueRecord>,
}

#[derive(Debug)]
struct DecodedTrace {
    paths: Vec<PathBuf>,
    steps: Vec<Step>,
    frames: Vec<Frame>,
}

impl DecodedTrace {
    fn read(out_dir: &Path) -> Self {
        let ct = std::fs::read_dir(out_dir)
            .unwrap_or_else(|e| panic!("read {}: {e}", out_dir.display()))
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|x| x == "ct"))
            .unwrap_or_else(|| panic!("no .ct container in {}", out_dir.display()));
        let events = codetracer_trace_reader::ctfs_reader::read_trace_from_ctfs(&ct)
            .unwrap_or_else(|e| panic!("decode {}: {e}", ct.display()));
        assert!(!events.is_empty(), "{} decoded to no events", ct.display());

        let mut paths = Vec::new();
        let mut functions = Vec::new();
        let mut varnames = Vec::new();
        let mut steps: Vec<Step> = Vec::new();
        let mut frames: Vec<Frame> = Vec::new();
        let mut open: Vec<usize> = Vec::new();
        for event in events {
            match event {
                TraceLowLevelEvent::Path(p) => paths.push(p),
                TraceLowLevelEvent::Function(f) => functions.push(f.name),
                TraceLowLevelEvent::VariableName(n) | TraceLowLevelEvent::Variable(n) => {
                    varnames.push(n)
                }
                TraceLowLevelEvent::Call(c) => {
                    frames.push(Frame {
                        function: functions[c.function_id.0].clone(),
                        depth: open.len() + 1,
                        return_value: None,
                    });
                    open.push(frames.len() - 1);
                }
                TraceLowLevelEvent::Return(r) => {
                    let idx = open.pop().expect("a return must close an open frame");
                    frames[idx].return_value = Some(r.return_value);
                }
                TraceLowLevelEvent::Step(s) => steps.push(Step {
                    path: paths[s.path_id.0].clone(),
                    line: s.line.0,
                    function: open.last().map(|&i| frames[i].function.clone()),
                    vars: Vec::new(),
                }),
                TraceLowLevelEvent::Value(v) => {
                    if let Some(step) = steps.last_mut() {
                        step.vars.push((varnames[v.variable_id.0].clone(), v.value));
                    }
                }
                _ => {}
            }
        }
        DecodedTrace {
            paths,
            steps,
            frames,
        }
    }

    fn steps_in(&self, function: &str) -> Vec<&Step> {
        self.steps
            .iter()
            .filter(|s| s.function.as_deref() == Some(function))
            .collect()
    }
}

fn int_value(value: &ValueRecord) -> Option<i64> {
    match value {
        ValueRecord::Int { i, .. } => Some(*i),
        _ => None,
    }
}

/// Names of the variables DWARF describes inside `function` (by `DW_AT_name`
/// of its `DW_TAG_subprogram`), read straight from the ELF.
fn dwarf_variable_names(elf: &Path, function: &str) -> Vec<String> {
    use object::{Object, ObjectSection};
    let data = std::fs::read(elf).unwrap();
    let obj = object::File::parse(&*data).unwrap();
    let dwarf = gimli::Dwarf::load(|id| -> Result<_, gimli::Error> {
        let bytes = obj
            .section_by_name(id.name())
            .and_then(|s| s.uncompressed_data().ok())
            .map(|c| c.into_owned())
            .unwrap_or_default();
        Ok(gimli::EndianRcSlice::new(
            std::rc::Rc::from(bytes),
            gimli::RunTimeEndian::Little,
        ))
    })
    .unwrap();
    let mut names = Vec::new();
    let mut units = dwarf.units();
    while let Some(header) = units.next().unwrap() {
        let unit = dwarf.unit(header).unwrap();
        let mut entries = unit.entries();
        let mut inside: Option<isize> = None;
        let mut depth: isize = 0;
        while let Some((delta, entry)) = entries.next_dfs().unwrap() {
            depth += delta;
            if let Some(d) = inside
                && depth <= d
            {
                inside = None;
            }
            let name = entry
                .attr_value(gimli::DW_AT_name)
                .unwrap()
                .and_then(|v| dwarf.attr_string(&unit, v).ok())
                .map(|s| gimli::Reader::to_string_lossy(&s).unwrap().into_owned());
            if entry.tag() == gimli::DW_TAG_subprogram && name.as_deref() == Some(function) {
                inside = Some(depth);
            } else if inside.is_some()
                && matches!(
                    entry.tag(),
                    gimli::DW_TAG_variable | gimli::DW_TAG_formal_parameter
                )
                && let Some(n) = name
            {
                names.push(n);
            }
        }
    }
    names
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Every step names a real source file by its absolute path.  In particular
/// the scratch crate's `src/lib.rs` (which DWARF records relative to the
/// build directory) resolves to the crate that was actually compiled, and
/// the program's own lines are attributed to the committed `main.rs`.
#[test]
fn step_paths_are_the_real_source_files() {
    let rec = recording();
    let lib_rs = rec.crate_dir.join("src").join("lib.rs");
    let main_rs = fixture_source();

    for step in &rec.trace.steps {
        assert!(
            step.path.is_absolute(),
            "step at line {} names a relative path {:?}; paths must resolve to real files",
            step.line,
            step.path
        );
    }
    let step_paths: Vec<&PathBuf> = rec.trace.steps.iter().map(|s| &s.path).collect();
    assert!(
        step_paths.contains(&&lib_rs),
        "the entrypoint's steps must be attributed to the compiled crate's {}; got paths {:?}",
        lib_rs.display(),
        rec.trace.paths
    );
    assert!(
        step_paths.contains(&&main_rs),
        "the program's steps must be attributed to {}; got paths {:?}",
        main_rs.display(),
        rec.trace.paths
    );
    for step in &rec.trace.steps {
        let name = step.path.file_name().unwrap();
        if name == "lib.rs" || name == "main.rs" {
            assert!(
                step.path == lib_rs || step.path == main_rs,
                "step at line {} names {:?}, which is neither the compiled {} nor {}",
                step.line,
                step.path,
                lib_rs.display(),
                main_rs.display()
            );
        }
    }
}

/// The lines recorded in `main.rs` are the program's own lines, in execution
/// order, each inside the function that contains it.
#[test]
fn steps_follow_the_program_lines_in_their_functions() {
    let rec = recording();
    let main_rs = fixture_source();
    let program_steps: Vec<(i64, Option<&str>)> = rec
        .trace
        .steps
        .iter()
        .filter(|s| s.path == main_rs)
        .map(|s| (s.line, s.function.as_deref()))
        .collect();
    // `main` calls `compute` on line 21; compute's statements execute on
    // lines 15 and 14 (the compiler schedules `b`'s store before `a`'s), its
    // closing brace on 18; then main finishes on 22.
    assert_eq!(
        program_steps,
        vec![
            (21, Some("main")),
            (15, Some("compute")),
            (14, Some("compute")),
            (18, Some("compute")),
            (22, Some("main")),
        ],
        "main.rs steps (line, function) in execution order"
    );
}

/// `compute` is recorded as a frame of its own: called from `main`, one level
/// deeper than it, returning the u64 10 it computes.
#[test]
fn compute_is_recorded_as_a_frame_called_from_main() {
    let rec = recording();
    let names: Vec<(&str, usize)> = rec
        .trace
        .frames
        .iter()
        .map(|f| (f.function.as_str(), f.depth))
        .collect();
    let main = rec
        .trace
        .frames
        .iter()
        .find(|f| f.function == "main")
        .unwrap_or_else(|| panic!("no `main` frame; frames: {names:?}"));
    let compute = rec
        .trace
        .frames
        .iter()
        .find(|f| f.function == "compute")
        .unwrap_or_else(|| panic!("no `compute` frame; frames: {names:?}"));
    assert_eq!(
        compute.depth,
        main.depth + 1,
        "compute must be called from main; frames: {names:?}"
    );
    assert_eq!(
        compute.return_value.as_ref().and_then(int_value),
        Some(10),
        "compute returns c == 10; got {:?}",
        compute.return_value
    );
    for step in rec.trace.steps_in("compute") {
        assert!(
            (13..=18).contains(&step.line),
            "a step inside compute is on line {}, outside compute's body (13..=18)",
            step.line
        );
    }
}

/// `a` is a named local of `compute` whose value is read from its stack slot
/// through its DWARF location (`DW_OP_fbreg`), never the address of that
/// slot.
///
/// Values are those at the start of each step.  In the default build `a`'s
/// DWARF scope (its lexical block) ends with the instruction that stores 10
/// into it, so `a` is visible only at lines 15 and 14, before that store,
/// when its slot still holds the zeroed stack's 0.  See
/// `copies_kept_by_the_compiler_are_read_with_their_values` for `a` == 10.
#[test]
fn local_a_is_read_from_its_stack_slot() {
    let rec = recording();
    let a_by_line: Vec<(i64, Vec<Option<i64>>)> = rec
        .trace
        .steps_in("compute")
        .iter()
        .map(|s| {
            (
                s.line,
                s.vars
                    .iter()
                    .filter(|(n, _)| n == "a")
                    .map(|(_, v)| int_value(v))
                    .collect(),
            )
        })
        .collect();
    assert_eq!(
        a_by_line,
        vec![(15, vec![Some(0)]), (14, vec![Some(0)]), (18, vec![]),],
        "`a` per compute step (line, values): in scope, and still 0, until its store executes"
    );
    assert_no_vm_addresses(&rec.trace);
}

/// With the copies kept, `a` and `b` are both in scope once assigned, and
/// both read back as 10 from their stack slots.
#[test]
fn copies_kept_by_the_compiler_are_read_with_their_values() {
    let rec = recording_keeping_copies();
    let at_16: Vec<&Step> = rec
        .trace
        .steps_in("compute")
        .into_iter()
        .filter(|s| s.line == 16)
        .collect();
    assert_eq!(at_16.len(), 1, "one step at `let c: u64 = b;`");
    let mut vars: Vec<(String, Option<i64>)> = at_16[0]
        .vars
        .iter()
        .map(|(n, v)| (n.clone(), int_value(v)))
        .collect();
    vars.sort();
    assert_eq!(
        vars,
        vec![("a".to_string(), Some(10)), ("b".to_string(), Some(10))],
        "locals at line 16"
    );
    assert_no_vm_addresses(&rec.trace);
}

fn assert_no_vm_addresses(trace: &DecodedTrace) {
    for step in &trace.steps {
        for (name, value) in &step.vars {
            if let Some(i) = int_value(value) {
                assert!(
                    !(0x1_0000_0000..0x6_0000_0000).contains(&(i as u64)),
                    "`{name}` at line {} holds {i:#x}, an SBF VM address, not a source value",
                    step.line
                );
            }
        }
    }
}

/// `b` and `c` are recorded exactly when the compiler described them in
/// DWARF -- with value 10 -- and are never invented otherwise.
///
/// rustc's MIR optimizations fold `let b = a; let c = b;` into constants even
/// at `opt-level = 0` and emit no `DW_TAG_variable` for either, so no location
/// exists to read them from; with those optimizations off `b` is described
/// (and must then read 10) while `c` still is not.  Checked on both builds,
/// this keeps the recorder honest in both directions.
#[test]
fn b_and_c_are_recorded_exactly_when_dwarf_describes_them() {
    for rec in [recording(), recording_keeping_copies()] {
        check_recorded_exactly_when_described(rec);
    }
}

fn check_recorded_exactly_when_described(rec: &Recording) {
    assert!(
        !rec.trace.steps_in("compute").is_empty(),
        "no steps inside compute, so its locals cannot be checked"
    );
    let described = dwarf_variable_names(&rec.elf, "compute");
    assert!(
        described.iter().any(|n| n == "a"),
        "DWARF for compute should describe `a`; describes {described:?}"
    );
    for name in ["b", "c"] {
        let recorded: Vec<&ValueRecord> = rec
            .trace
            .steps_in("compute")
            .iter()
            .flat_map(|s| s.vars.iter())
            .filter(|(n, _)| n == name)
            .map(|(_, v)| v)
            .collect();
        if described.iter().any(|n| n == name) {
            assert!(
                recorded.iter().any(|v| int_value(v) == Some(10)),
                "DWARF describes `{name}`, so it must be recorded with value 10; got {recorded:?}"
            );
        } else {
            assert!(
                recorded.is_empty(),
                "DWARF has no entry for `{name}` (it describes {described:?}), yet the trace \
                 records it as {recorded:?}: a value without debug info is invented"
            );
        }
    }
}

/// With DWARF available, values are named source-level locals; raw machine
/// registers are not presented as variables.
#[test]
fn no_machine_registers_are_recorded_as_variables() {
    let rec = recording();
    let mut registers: HashMap<String, usize> = HashMap::new();
    for step in &rec.trace.steps {
        for (name, _) in &step.vars {
            let is_register = name
                .strip_prefix('r')
                .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
            if is_register {
                *registers.entry(name.clone()).or_default() += 1;
            }
        }
    }
    assert!(
        registers.is_empty(),
        "registers recorded as source variables: {registers:?}"
    );
}
