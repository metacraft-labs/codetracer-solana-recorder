//! Every source file of a column-aware Solana trace is one `paths.dat` record,
//! and that record carries the file's own per-line table.
//!
//! In a column-aware trace the writer fixes a file's line table when the file
//! is first interned, and a file is interned by its first mention: an explicit
//! registration, or a step, function, call or id request that names it
//! (`codetracer-trace-format-spec/internal-files.md` §"`paths.dat` Layout A").
//! A table offered after that mention cannot be honoured. So the recorder must
//! register each readable file's table before anything names the file, and it
//! must name the file by the same string everywhere -- a second spelling is a
//! second record, interned by whichever mention reaches the writer first.
//!
//! The tests record real source files written to a scratch directory and read
//! the container back with the trace-format reader. No mocks: the recorder runs
//! its real entry points against the real writer and the real filesystem.

use std::collections::BTreeMap;
use std::path::Path;

use codetracer_solana_recorder::cpi::CpiDetector;
use codetracer_solana_recorder::multi_program::ProgramRegistry;
use codetracer_solana_recorder::recorder::{record_from_snapshots_with_columns, record_with_cpi};
use codetracer_solana_recorder::register_trace::RegisterSnapshot;

fn snap(pc: u64) -> RegisterSnapshot {
    let mut registers = [0u64; 12];
    registers[11] = pc;
    RegisterSnapshot { registers }
}

/// The per-line table the recorder states for a source file: the byte length
/// of each line, without its line terminator.
fn table_of(text: &str) -> Vec<u32> {
    text.split_inclusive('\n')
        .map(|l| l.trim_end_matches('\n').len() as u32)
        .collect()
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// Every `paths.dat` record of the container in `out_dir`, as path → table.
/// Fails when a path string appears in two records.
fn path_records(out_dir: &Path) -> BTreeMap<String, Vec<u32>> {
    let ct = std::fs::read_dir(out_dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", out_dir.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "ct"))
        .unwrap_or_else(|| panic!("no .ct container in {}", out_dir.display()));
    let tables = codetracer_trace_reader::interning_tables_reader::open_interning_tables(&ct)
        .unwrap_or_else(|e| panic!("open interning tables of {}: {e}", ct.display()))
        .expect("the container carries interning tables");
    assert!(tables.is_column_aware(), "the trace is column-aware");
    let mut records = BTreeMap::new();
    for id in 0..tables.path_count() as u64 {
        let path = tables.path_str(id).unwrap();
        let table = tables.path_line_lengths(id).unwrap();
        if let Some(earlier) = records.insert(path.clone(), table) {
            panic!("{path} has two paths.dat records (the first with table {earlier:?})");
        }
    }
    records
}

fn assert_records(out_dir: &Path, expected: &[(&Path, &str)]) {
    let actual = path_records(out_dir);
    let expected: BTreeMap<String, Vec<u32>> = expected
        .iter()
        .map(|(p, text)| (p.to_string_lossy().into_owned(), table_of(text)))
        .collect();
    assert_eq!(
        actual, expected,
        "each source file is one paths.dat record carrying its own table"
    );
}

const PRIMARY: &str = "fn main() {\n    let a = 1;\n    callee_program();\n}\n";
const CALLEE: &str = "pub fn entry() {\n    let value_from_callee = 42;\n}\n";

/// A file first reached through a cross-program call is named by the call
/// before the snapshot loop gets to it, and the primary program's DWARF names
/// its own source by a shorter spelling than the trace anchor.
#[test]
fn cpi_callee_and_primary_each_keep_their_own_table() {
    let scratch = tempfile::tempdir().unwrap();
    let primary = scratch.path().join("primary").join("main.rs");
    let callee = scratch.path().join("callee").join("entry.rs");
    write(&primary, PRIMARY);
    write(&callee, CALLEE);

    let callee_str = callee.to_string_lossy().into_owned();
    let mut registry = ProgramRegistry::new();
    registry.add_synthetic_program(
        "primary",
        0..100,
        vec![
            (0, "main.rs".to_string(), 1),
            (1, "main.rs".to_string(), 2),
            (2, "main.rs".to_string(), 3),
        ],
    );
    registry.add_synthetic_program(
        "callee",
        1000..1100,
        vec![(1000, callee_str.clone(), 1), (1001, callee_str.clone(), 2)],
    );
    let mut detector = CpiDetector::new(0..100);
    detector.add_program_range("callee", 1000..1100);

    let snapshots = [snap(0), snap(1), snap(1000), snap(1001), snap(2)];
    let out = scratch.path().join("trace");
    record_with_cpi(&snapshots, &registry, &mut detector, &primary, &out).unwrap();

    assert_records(&out, &[(&primary, PRIMARY), (&callee, CALLEE)]);
}

const LIB: &str = "pub fn process() {\n    let a = 1;\n    helper();\n}\n";
const HELPER: &str = "pub fn helper() {\n    let helper_local = 7;\n}\n";

/// `cargo-build-sbf` remaps source paths, so DWARF names the crate's files
/// relative to the crate root while the trace anchor is absolute.
#[test]
fn crate_relative_dwarf_paths_are_one_record_per_file() {
    let scratch = tempfile::tempdir().unwrap();
    let crate_root = scratch.path().join("program");
    write(
        &crate_root.join("Cargo.toml"),
        "[package]\nname = \"program\"\n",
    );
    let lib = crate_root.join("src").join("lib.rs");
    let helper = crate_root.join("src").join("helper.rs");
    write(&lib, LIB);
    write(&helper, HELPER);

    let locations: Vec<(u64, &str, u32, Option<u32>)> = vec![
        (0, "src/lib.rs", 1, Some(1)),
        (1, "src/lib.rs", 2, Some(5)),
        (5, "src/helper.rs", 1, Some(1)),
        (6, "src/helper.rs", 2, Some(5)),
    ];
    let snapshots = [snap(0), snap(1), snap(5), snap(6)];
    let out = scratch.path().join("trace");
    record_from_snapshots_with_columns(&snapshots, &locations, &lib, &out).unwrap();

    assert_records(&out, &[(&lib, LIB), (&helper, HELPER)]);
}
