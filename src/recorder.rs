//! Core recording logic for Solana/SBF program execution.
//!
//! This module consumes register trace data from the SBF VM,
//! correlates it with DWARF source mapping, and produces CodeTracer
//! trace output.
//!
//! The output format is fixed to CTFS — see
//! `Recorder-CLI-Conventions.md` §4 in `codetracer-specs`.  Use
//! `ct print` (from `codetracer-trace-format-nim`) for human-readable
//! conversion of the produced bundle.
//!
//! ## Source-driven event synthesis
//!
//! When recording from synthetic register snapshots (the `record_from_snapshots`
//! path used by integration fixtures and the cargo-build-sbf-less unit test
//! harness), the SBF interpreter is not actually running, so syscalls like
//! `msg!` / `sol_log` and the typed Rust value-construction sites
//! (`vec![..]`, tuple literals, `Struct { .. }`) never fire on their own.
//!
//! To keep the recorder's output spec-compliant in that mode, we read the
//! source file pointed to by `source_path` at recording start and build:
//!
//! * a **function map** — for every `fn name(...)` declaration, the line
//!   range it occupies — so the recorder can resolve nested call frames
//!   to real names instead of the synthetic `fn_at_pc_<pc>` placeholder
//!   the original heuristic used.
//! * a **per-line content map** — so for every visited step the recorder
//!   can scan the corresponding source line for `msg!(...)`, `panic!(...)`,
//!   `Err(...)` and Solana logging macros and synthesise the matching EVENT
//!   (`register_special_event`), with the macro's literal text as payload.
//!
//! It synthesises NO variables. This path has no DWARF variable information
//! and the trace format cannot mark a name as inferred, so it records only the
//! VM registers `r0`..`r10`, named as registers (Recorder-Solana CXP-SOL-2,
//! operator decision 2026-11-12). Until then it guessed locals from `let`
//! lines and function parameters and showed them as recovered variables.
//! Named locals come from `source_recorder` when DWARF describes the program.
//!
//! The event synthesis is purely additive: the register-variable emission
//! continues unchanged.  When the source file is missing
//! (e.g. legacy unit tests that pass a fictitious `test.rs`), the maps
//! degrade to empty and the recorder behaves exactly as before.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use codetracer_trace_types::{EventLogKind, Line, NONE_VALUE, TypeId, TypeKind, ValueRecord};
use codetracer_trace_writer_nim::trace_writer::TraceWriter;
use codetracer_trace_writer_nim::{TraceEventsFileFormat, create_trace_writer};
use eyre::{Context, Result, eyre};

use crate::cpi::{CpiDetector, CpiEvent};
use crate::dwarf::DwarfParser;
use crate::multi_program::ProgramRegistry;
use crate::register_trace::{RegisterSnapshot, parse_regs_file};

/// Read per-line byte counts for `path` and return them as a flat `Vec<u32>`
/// where entry `i` is the addressable column count of source line `i+1`
/// (1-based numbering matching the `paths.dat` Layout A contract — see
/// `codetracer-trace-format-spec/trace-events.md` §"paths.dat per-line
/// offset table — Layout A").
///
/// Synthetic-only test paths (e.g. `<stdin>`) and missing/unreadable files
/// degrade to an empty `Vec`; the writer treats `register_path_with_line_lengths`
/// with an empty slice as "no per-line data", so the column resolution at
/// read time falls back to surfacing `None`, matching the back-compat-safe
/// default codified in P6.5.
///
/// Ported from the Python recorder's
/// `runtime/output_paths.rs::read_line_lengths_for_path`.
pub(crate) fn read_line_lengths_for_path(path: &Path) -> Vec<u32> {
    let lossy = path.to_string_lossy();
    if lossy.starts_with('<') && lossy.ends_with('>') {
        return Vec::new();
    }
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    let mut lines: Vec<u32> = Vec::new();
    let mut current_len: u32 = 0;
    for byte in &bytes {
        if *byte == b'\n' {
            lines.push(current_len);
            current_len = 0;
        } else {
            current_len = current_len.saturating_add(1);
        }
    }
    if current_len > 0 || bytes.last() != Some(&b'\n') {
        lines.push(current_len);
    }
    lines
}

// The recorder is CTFS-only per `Recorder-CLI-Conventions.md` §4 (see
// `codetracer-specs`).  We pin every `create_trace_writer` call site to
// this constant so the recorder surface no longer carries a `format`
// parameter and the writer cannot accidentally drift away from the
// canonical multi-stream container.
const CTFS_FORMAT: TraceEventsFileFormat = TraceEventsFileFormat::Ctfs;

/// Record a Solana program execution from pre-generated register trace
/// and DWARF-annotated ELF, producing CodeTracer trace output.
///
/// # Arguments
///
/// * `regs_data` - Raw `.regs` file content (binary register snapshots)
/// * `elf_data`  - Raw ELF file content (unstripped, with DWARF)
/// * `source_path` - Path to display in the trace for source locations
/// * `out_dir`   - Directory where trace files will be written
///
/// The output format is fixed to the canonical CodeTracer CTFS multi-stream
/// container.
pub fn record_from_traces(
    regs_data: &[u8],
    elf_data: &[u8],
    source_path: &Path,
    out_dir: &Path,
) -> Result<()> {
    // 1. Parse register snapshots.
    let snapshots = parse_regs_file(regs_data)?;

    // 2. Parse DWARF from ELF.
    let dwarf = DwarfParser::new(elf_data)?;

    // 3. Build source locations from DWARF — carry the column when DWARF
    //    populated it so column-aware replay navigation has byte-accurate
    //    landing points.  DWARF emitters that only produce line-only
    //    entries (e.g. `-g1` builds) leave `column == None` and the
    //    writer falls back to the column-less step path.
    let source_locations: Vec<(u64, String, u32, Option<u32>)> = snapshots
        .iter()
        .filter_map(|snap| {
            let loc = dwarf.find_location(snap.pc())?;
            Some((snap.pc(), loc.file, loc.line, loc.column))
        })
        .collect();

    // Convert to borrowed form for the shared implementation.
    let source_locs_ref: Vec<(u64, &str, u32, Option<u32>)> = source_locations
        .iter()
        .map(|(pc, f, l, c)| (*pc, f.as_str(), *l, *c))
        .collect();

    // Derive the trace's "source file" from DWARF: the file that contains
    // the first PC seen in execution.  Without this, the caller's
    // ``source_path`` (which the recorder treats as the program identifier
    // and writes into the trace via ``TraceWriter::start``) is the ELF
    // ``.so`` path -- the DAP server then asks VS Code to open the ``.so``
    // as a source file and the smoke test's ``opens
    // solana_flow_test.rs in the editor`` assertion times out after 120s
    // (observed against cross-repo runs at d92244b and earlier).  Resolve
    // the .rs path from the first valid source location, fall back to the
    // ELF path only if DWARF resolution turned up empty.
    // Prefer the .rs source file from DWARF, falling back to:
    //   1. A sibling .rs file next to the .so (looks for <out_dir
    //      grandparent>/src/*.rs -- the layout produced by
    //      cargo-build-sbf for a single-source crate).
    //   2. The original ELF path (last resort, may not open in VS
    //      Code as a source tab).
    //
    // The fallback exists because cargo-build-sbf release builds (the
    // only profile sBPF supports) currently emit an empty .debug_info
    // section in the .so even when ``--debug`` is set -- so the
    // primary DWARF→source map will be empty in practice.  Without
    // the fallback the recorder would write the .so as the trace's
    // source path and the DAP server would tell VS Code to open the
    // ELF as text, hanging the smoke test's ``opens
    // solana_flow_test.rs in the editor`` assertion at the 120s
    // timeout.
    // ``source_locations.first()`` would point at the ``entrypoint!``
    // macro source (under ``.cargo/registry/.../solana-program-
    // entrypoint-*/src/lib.rs``) because that's the first PC executed
    // after the trampoline jumps into the user's program -- this
    // confused VS Code into opening the third-party crate as the
    // active editor tab and the smoke test waited forever for
    // ``solana_flow_test.rs``.  Skip stdlib / cargo-registry paths
    // and prefer the first DWARF location that lives outside both,
    // i.e. the user's program source.
    let trace_source_path: PathBuf = if let Some((_, f, _, _)) = source_locations
        .iter()
        .find(|(_, f, _, _)| !is_third_party_source(f))
    {
        PathBuf::from(f)
    } else if let Some(rs) = sibling_rs_source(source_path) {
        rs
    } else if let Some((_, f, _, _)) = source_locations.first() {
        PathBuf::from(f)
    } else {
        source_path.to_path_buf()
    };
    // ``cargo-build-sbf`` on CI invokes the compiler with
    // ``--remap-path-prefix`` so DWARF paths for the user's crate
    // arrive relative to the crate root (e.g. ``src/solana_flow_test.rs``)
    // rather than absolute.  The recorder runs from a different cwd at
    // trace time -- ``SourceModel::load`` then reads from cwd, the file
    // is missing, the model is empty, and every call site degrades to
    // the ``fn_at_pc_<pc>`` placeholder.  Resolve the relative path
    // against the ELF's crate root (the nearest ancestor of the .so
    // that contains ``Cargo.toml``) so the source model gets populated
    // and the WDIO smoke test's ``finds process_instruction in the
    // calltrace`` assertion finds the real function name.
    let trace_source_path = resolve_source_against_elf_crate(&trace_source_path, source_path);
    eprintln!(
        "Trace source path: {} ({} DWARF source locations)",
        trace_source_path.display(),
        source_locations.len(),
    );

    record_from_snapshots_with_columns(&snapshots, &source_locs_ref, &trace_source_path, out_dir)
}

/// Look for a sibling ``.rs`` source file next to the given ELF path.
///
/// cargo-build-sbf places the linked ELF at
/// ``test-programs/target/deploy/test_programs.so`` and the lib source
/// at ``test-programs/src/<name>.rs``.  Resolve from the ELF up to the
/// crate root (two ``..`` from ``target/deploy``) and look for a single
/// ``.rs`` file under ``src/``.  Returns ``None`` if the layout differs
/// or more than one candidate is present (ambiguous -- safer to leave
/// the caller's fallback in place).
/// Identify a DWARF source path that points at a third-party crate or
/// the bundled rust standard library rather than the user's own source.
///
/// SBF programs link in ``solana-program`` (and its ``entrypoint!``
/// macro source ``solana-program-entrypoint-*/src/lib.rs``) plus the
/// platform-tools-pinned ``rust/library/{core,alloc,std}/`` source
/// tree.  Those paths appear in the DWARF debug_info because the
/// compiler embeds the original source location of every inlined
/// helper, but VS Code can't usefully open them as the active editor
/// tab for the smoke test (they live in ``~/.cargo/registry`` or in
/// a path that exists only on the platform-tools build machine).
/// Filter both classes out so the trace's primary source path is the
/// user's crate.
fn is_third_party_source(path: &str) -> bool {
    if path.contains(".cargo/registry/")
        || path.contains("/rust/library/")
        || path.contains("/rustlib/src/rust/library/")
    {
        return true;
    }
    // ``cargo-build-sbf`` on CI uses ``--remap-path-prefix`` so the
    // ``.cargo/registry/...`` paths above arrive as a bare relative
    // path (observed against CI run 27531347645:
    // ``source_locations[0] = src/lib.rs``).  We can't tell from a
    // relative ``src/lib.rs`` alone whether the source is a registry
    // dep or the user's crate -- but by convention the user's SBF
    // crate root is named after its program rather than the cargo
    // default ``lib.rs`` (the test-programs crate's lib was renamed
    // to ``solana_flow_test.rs`` precisely so the DAP server picks
    // the user file as the editor tab).  Treat the bare ``lib.rs``
    // basename as third-party -- a heuristic, but the only one
    // reliable across local-vs-CI builds with different
    // ``--remap-path-prefix`` settings.
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        == Some("lib.rs")
}

/// Resolve a (possibly relative) DWARF source path against the ELF's
/// Cargo crate root.
///
/// ``cargo-build-sbf`` on CI passes ``--remap-path-prefix`` to make DWARF
/// paths relative to the crate root.  At trace time the recorder may run
/// from a different cwd than the crate root, so a relative path won't
/// resolve against ``std::fs::read_to_string``.  Walk upwards from the
/// ELF until we find an ancestor that contains a ``Cargo.toml``; if the
/// relative path resolves under that ancestor, return the resolved path.
/// Otherwise return the input unchanged (preserves the legacy absolute-
/// path / found-locally behaviour for local builds).
fn resolve_source_against_elf_crate(dwarf_path: &Path, elf_path: &Path) -> PathBuf {
    if dwarf_path.is_absolute() || dwarf_path.exists() {
        return dwarf_path.to_path_buf();
    }
    for anc in elf_path.ancestors() {
        if anc.join("Cargo.toml").exists() {
            let candidate = anc.join(dwarf_path);
            if candidate.exists() {
                return candidate;
            }
        }
    }
    dwarf_path.to_path_buf()
}

/// Decide whether the DWARF source path ``file_str`` for a snapshot
/// is third-party from the user's perspective.
///
/// The basename-only ``is_third_party_source`` heuristic
/// (``lib.rs`` plus ``.cargo/registry/`` / ``/rust/library/`` path
/// fragments) only catches the most common cases: ``solana-program-
/// entrypoint``'s ``src/lib.rs`` wrapper.  sBPF programs link in
/// many more third-party stdlib files whose ``--remap-path-prefix``
/// projection is indistinguishable from a user-crate source
/// (``src/hazmat.rs`` from ``curve25519-dalek``'s ``subtle`` ops,
/// ``src/syscalls.rs`` from ``solana-program``'s syscall shims,
/// etc.).  Stepping over to those in the WDIO ``performs multiple
/// step-over operations and changes line`` deep test wedges VS
/// Code's editor cursor (the third-party file is not on disk so
/// ``editor.selection.active.line`` doesn't advance, the
/// ``uniqueLines.size > 1`` assertion sees the same line on every
/// iteration -- cross-repo run 27657183885).
///
/// Filter strategy: when ``user_crate_root`` is known (production:
/// the nearest ancestor of ``source_path`` carrying a
/// ``Cargo.toml``), check whether ``file_str`` resolves to an
/// existing file under that root.  Any path that does NOT resolve
/// is third-party (it came from a sibling crate's source tree the
/// linker pulled in via DWARF).  When ``user_crate_root`` is
/// ``None`` (synthetic-fixture tests pass a relative
/// ``source_path`` with no real filesystem layout to consult), only
/// fall back to ``is_third_party_source`` -- and even there, the
/// ``same_logical_source`` guard keeps snapshots whose
/// ``file_str`` matches the trace anchor (so a test that uses
/// ``"lib.rs"`` as both ``source_path`` and the snapshot file
/// stays visible).
/// Resolve a snapshot's DWARF ``file_str`` to the same path
/// representation the trace's anchor (``source_path``) uses, so
/// every emitted ``Step`` shares a single path string per logical
/// source file.
///
/// Why it matters: VS Code's DAP integration moves the editor
/// cursor on each ``stopped`` event only when the event's
/// ``source`` path matches the path of an already-open editor tab.
/// The trace's call_entry ``Step`` uses ``source_path`` (absolute --
/// resolved by ``resolve_source_against_elf_crate``) but
/// ``cargo-build-sbf``'s ``--remap-path-prefix`` projects every
/// later DWARF source path to a short relative form like
/// ``src/solana_flow_test.rs``.  Without normalisation the trace
/// emits step 0 at the absolute anchor, step 1+ at the relative
/// snapshot — VS Code opens the absolute file as the editor tab,
/// can't reconcile the relative paths the DAP server later
/// reports, the cursor stays at line 1, and the WDIO ``performs
/// multiple step-over operations and changes line`` deep test
/// sees the same line on every iteration (cross-repo run
/// 27658362322 attempt 2 — 10/11 deep tests passing, this one
/// still wedged).
///
/// Strategy: ``file_str`` that matches the anchor via
/// ``same_logical_source`` collapses straight to ``source_path``.
/// Sibling user-crate files (the multi-file user-crate scenario
/// in ``test_multiple_source_files``) are resolved against
/// ``user_crate_root`` when available.  Everything else is passed
/// through unchanged so synthetic-fixture tests that pass bare
/// basenames still see the basename in the recorded trace.
fn canonical_step_path(
    file_str: &str,
    source_path_str: &str,
    user_crate_root: Option<&Path>,
) -> PathBuf {
    if same_logical_source(source_path_str, file_str) {
        return PathBuf::from(source_path_str);
    }
    if let Some(crate_root) = user_crate_root {
        let candidate = Path::new(file_str);
        if candidate.is_absolute() {
            return candidate.to_path_buf();
        }
        let joined = crate_root.join(candidate);
        if joined.exists() {
            return joined;
        }
    }
    PathBuf::from(file_str)
}

/// The source files of one recording, each under one path string and each
/// registered with its line table before anything else names it.
///
/// In a column-aware trace the writer fixes a file's table at the file's first
/// mention -- a registration, or a step, function or call naming it -- and a
/// table offered later cannot be honoured. Every mention therefore goes through
/// [`SourcePaths::ensure`], which registers the file's table on the first one
/// and returns the single string ([`canonical_step_path`]) the file is named by
/// in every record, so two spellings of one file cannot become two records.
struct SourcePaths {
    source_path: PathBuf,
    source_path_str: String,
    user_crate_root: Option<PathBuf>,
    /// Per spelling seen in debug info: the path the file is recorded under.
    canonical: HashMap<String, PathBuf>,
    /// Per recorded path: whether it was registered with a line table.
    tabled: HashMap<PathBuf, bool>,
}

impl SourcePaths {
    fn new(source_path: &Path) -> Self {
        // The user's crate root: the nearest ancestor of an absolute
        // ``source_path`` that holds a ``Cargo.toml``. ``cargo-build-sbf``
        // remaps every source path to a relative ``src/<name>.rs``, so this is
        // what such a path is resolved against, and what tells the user's
        // files from bundled dependencies'. ``None`` for a relative
        // ``source_path`` (synthetic fixtures).
        let user_crate_root = if source_path.is_absolute() {
            source_path
                .ancestors()
                .find(|anc| anc.join("Cargo.toml").exists())
                .map(Path::to_path_buf)
        } else {
            None
        };
        Self {
            source_path: source_path.to_path_buf(),
            source_path_str: source_path.to_string_lossy().into_owned(),
            user_crate_root,
            canonical: HashMap::new(),
            tabled: HashMap::new(),
        }
    }

    /// The path `file_str` is recorded under, registered with its line table
    /// if this is the file's first mention.
    ///
    /// The table is read from the recorded path, or, when that does not read
    /// (a bare basename), from `file_str` beside the trace anchor. A file that
    /// reads neither way is registered without one.
    fn ensure(&mut self, writer: &mut dyn TraceWriter, file_str: &str) -> PathBuf {
        if let Some(path) = self.canonical.get(file_str) {
            return path.clone();
        }
        let path = canonical_step_path(
            file_str,
            &self.source_path_str,
            self.user_crate_root.as_deref(),
        );
        if !self.tabled.contains_key(&path) {
            let lengths = match read_line_lengths_for_path(&path) {
                v if !v.is_empty() => v,
                _ => self
                    .source_path
                    .parent()
                    .map(|p| read_line_lengths_for_path(&p.join(file_str)))
                    .unwrap_or_default(),
            };
            let _ = writer.register_path_with_line_lengths(&path, &lengths);
            self.tabled.insert(path.clone(), !lengths.is_empty());
        }
        self.canonical.insert(file_str.to_string(), path.clone());
        path
    }

    /// Whether `path` (as returned by [`SourcePaths::ensure`]) has a line
    /// table.
    ///
    /// A column is only addressable in a file the writer sized from its own
    /// line table. In a file without one, a column folded into the address
    /// reads back as a later line. DWARF paths routinely fail to resolve on
    /// the recording machine (`--remap-path-prefix`, a build on another
    /// host), so this is the normal case rather than an edge one.
    fn has_table(&self, path: &Path) -> bool {
        self.tabled.get(path).copied().unwrap_or(false)
    }
}

fn is_third_party_for_user(
    source_path_str: &str,
    file_str: &str,
    user_crate_root: Option<&Path>,
) -> bool {
    // Anchor match wins everywhere: a snapshot whose file matches
    // the trace's ``source_path`` (exact, suffix, or basename --
    // see ``same_logical_source``) is the user's source by
    // construction, even if ``--remap-path-prefix`` projected it
    // to a bare basename ``cargo-build-sbf`` couldn't resolve
    // against the crate root (e.g. the test program lives in
    // ``test-programs/solana/<name>.rs`` but DWARF stores
    // ``<name>.rs`` and ``user_crate_root`` is ``test-programs/``
    // so ``crate_root.join("<name>.rs")`` doesn't exist).
    if same_logical_source(source_path_str, file_str) {
        return false;
    }
    if let Some(crate_root) = user_crate_root {
        let candidate = Path::new(file_str);
        if candidate.is_absolute() {
            return !candidate.exists();
        }
        return !crate_root.join(candidate).exists();
    }
    is_third_party_source(file_str)
}

/// Returns ``true`` if ``candidate`` could plausibly refer to the
/// same logical source file as ``anchor``.
///
/// Used so synthetic-fixture tests that pass bare basenames
/// (``"lib.rs"``) as both the trace anchor and the per-snapshot
/// source path aren't accidentally classified as third-party --
/// ``is_third_party_source("lib.rs")`` is ``true`` (the macro-
/// inline heuristic for sBPF programs whose user crate root is
/// renamed away from the cargo default), so we need a way to say
/// "this looks third-party but it's actually the trace anchor".
/// The comparison treats any of:
///   * exact equality,
///   * ``anchor`` ending in ``"/" + candidate`` (relative or basename
///     hanging off the anchor's directory tree),
///   * ``candidate`` ending in ``"/" + anchor`` (rare inverse case
///     when the test fixture stores the longer path in the snapshot),
/// as referring to the same file.  A pure basename ("foo.rs") matches
/// any anchor whose final path component is "foo.rs".
fn same_logical_source(anchor: &str, candidate: &str) -> bool {
    if anchor == candidate {
        return true;
    }
    if let Some(tail) = anchor.strip_suffix(candidate)
        && (tail.ends_with('/') || tail.is_empty())
    {
        return true;
    }
    if let Some(tail) = candidate.strip_suffix(anchor)
        && (tail.ends_with('/') || tail.is_empty())
    {
        return true;
    }
    false
}

fn sibling_rs_source(elf_path: &Path) -> Option<PathBuf> {
    let crate_root = elf_path.parent()?.parent()?.parent()?;
    let src_dir = crate_root.join("src");
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&src_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("rs"))
        .collect();
    if entries.len() == 1 {
        Some(entries.pop().unwrap())
    } else {
        None
    }
}

/// Record a Solana program execution from pre-parsed register snapshots
/// and source locations, producing CodeTracer trace output.
///
/// This is the lower-level entry point useful for testing with synthetic data
/// (bypassing ELF/DWARF parsing).
///
/// # Arguments
///
/// * `snapshots`        - Parsed register snapshots
/// * `source_locations` - Tuples of (pc, file, line) mapping PCs to source
/// * `source_path`      - Path to display in the trace
/// * `out_dir`          - Directory where trace files will be written
///
/// The output format is fixed to the canonical CodeTracer CTFS multi-stream
/// container.
pub fn record_from_snapshots(
    snapshots: &[RegisterSnapshot],
    source_locations: &[(u64, &str, u32)],
    source_path: &Path,
    out_dir: &Path,
) -> Result<()> {
    // Promote line-only locations to the column-aware tuple shape with
    // `column == None`.  The downstream writer routes those through the
    // column-less `register_step_with_column` path, so back-compat for
    // every existing call site (tests + the cargo-build-sbf-less
    // fixtures) is preserved byte-for-byte.
    let locs: Vec<(u64, &str, u32, Option<u32>)> = source_locations
        .iter()
        .map(|(pc, f, l)| (*pc, *f, *l, None))
        .collect();
    record_from_snapshots_with_columns(snapshots, &locs, source_path, out_dir)
}

/// Column-aware variant of [`record_from_snapshots`].
///
/// `source_locations` carries a `(pc, file, line, column)` tuple per
/// snapshot PC.  When `column == Some(c)` the recorder emits a column-
/// aware step (`register_step_with_column`, P6.3 wire encoding); when
/// `column == None` the writer falls back to the column-less path so
/// DWARF debug info that only carries line numbers still records steps.
///
/// This is the entry point the DWARF-fed `record_from_traces` pipeline
/// uses, plus the column-aware integration test fixture.  The non-
/// column-aware path delegates here with `column = None` for every row,
/// preserving the legacy meta.dat shape (bit 4 clear) for existing
/// fixtures that did not assert column ordering.
pub fn record_from_snapshots_with_columns(
    snapshots: &[RegisterSnapshot],
    source_locations: &[(u64, &str, u32, Option<u32>)],
    source_path: &Path,
    out_dir: &Path,
) -> Result<()> {
    // Create output directory.
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("cannot create output dir: {}", out_dir.display()))?;

    // Create the trace writer (CTFS only).
    let program_name = source_path.to_string_lossy();
    let mut writer = create_trace_writer(&program_name, &[], CTFS_FORMAT);

    // Set up output files.  CTFS-only writer — events stream lives in
    // `trace.bin`.
    let events_path = out_dir.join("trace.bin");

    TraceWriter::begin_writing_trace_events(&mut *writer, &events_path)
        .map_err(|e| eyre!("{e}"))?;

    record_from_snapshots_into_writer(snapshots, source_locations, source_path, &mut *writer)?;

    // Finish writing.
    TraceWriter::finish_writing_trace_events(&mut *writer).map_err(|e| eyre!("{e}"))?;
    writer
        .write_meta_dat("codetracer-solana-recorder")
        .map_err(|e| eyre!("{e}"))?;
    writer.close().map_err(|e| eyre!("{e}"))?;

    Ok(())
}

/// Source-derived view of the program being recorded.
///
/// Built once at the top of `record_from_snapshots_into_writer` from the
/// fixture file pointed to by `source_path`; thereafter consulted on every
/// visited step to (a) resolve nested call frames to real function names
/// (replacing the previous `fn_at_pc_<pc>` synthetic placeholder) and
/// (b) recognise side-effecting / structured-value constructs the
/// synthetic-snapshot pipeline cannot otherwise observe (`msg!(...)`,
/// `panic!(...)`, `Err(...)`, `vec![..]` / `[..]` array literal, tuple
/// literal, `Name { .. }` struct literal) and synthesise the matching
/// trace events.
///
/// When the source file is missing or unreadable (legacy unit-test paths
/// pass synthetic `test.rs` / `caller.rs` strings that don't exist on
/// disk), every accessor degrades to a zero-information answer and the
/// recorder behaves exactly as before this module landed.
#[derive(Default)]
struct SourceModel {
    /// One entry per source line (1-indexed; `lines[0]` is "" sentinel).
    lines: Vec<String>,
    /// `(start_line, end_line, function_name)` for every `fn name(...)`
    /// declaration found in the file, in declaration order.  `end_line`
    /// is inclusive and is the line of the matching `}` (using simple
    /// brace-balance tracking that ignores braces inside `'..'` /
    /// `"..."` / `//`-comments — sufficient for the workspace's
    /// hand-written Rust fixtures).
    functions: Vec<(u32, u32, String)>,
}

impl SourceModel {
    /// Try to load the source file at `source_path`.  Missing files are
    /// not an error — they yield an empty model so legacy unit tests that
    /// pass synthetic paths keep working.
    fn load(source_path: &Path) -> Self {
        let Ok(content) = std::fs::read_to_string(source_path) else {
            return Self::default();
        };
        let mut lines = vec![String::new()]; // 1-indexed sentinel
        for line in content.lines() {
            lines.push(line.to_string());
        }
        let functions = parse_function_ranges(&lines);
        Self { lines, functions }
    }

    /// The 1-indexed source line text, or `""` if the line is out of range.
    fn line(&self, line_no: u32) -> &str {
        self.lines
            .get(line_no as usize)
            .map(String::as_str)
            .unwrap_or("")
    }

    /// The function declaration covering `line_no`, or `None` if no
    /// declaration was found that brackets the line.
    fn function_at(&self, line_no: u32) -> Option<&str> {
        // Pick the innermost enclosing function (smallest range).
        self.functions
            .iter()
            .filter(|(start, end, _)| *start <= line_no && line_no <= *end)
            .min_by_key(|(start, end, _)| *end - *start)
            .map(|(_, _, name)| name.as_str())
    }
}

/// Parse top-level `fn name(...)` declarations and their `{ ... }` bodies
/// into `(start_line, end_line, name)` triples.  Brace balancing is
/// best-effort: it tracks `{` / `}` inside `"..."` strings, `//` line
/// comments, and `/* ... */` block comments to avoid being thrown by
/// the workspace's hand-written Rust fixtures.  Generic / where-clause
/// declarations spread across multiple lines are not supported (none of
/// the fixtures rely on them).
fn parse_function_ranges(lines: &[String]) -> Vec<(u32, u32, String)> {
    let mut out = Vec::new();
    let mut i: usize = 1;
    while i < lines.len() {
        let raw = &lines[i];
        if let Some(name) = match_fn_declaration(raw) {
            // Find the opening `{` (may be on this or a later line) and
            // walk to the matching `}` with brace balance.
            let mut start_brace_seen = false;
            let mut depth: i32 = 0;
            let mut end_line = i as u32;
            let mut j = i;
            'outer: while j < lines.len() {
                let line = &lines[j];
                let mut chars = line.chars().peekable();
                let mut in_string = false;
                let mut in_block_comment = false;
                while let Some(c) = chars.next() {
                    if in_block_comment {
                        if c == '*' && chars.peek() == Some(&'/') {
                            chars.next();
                            in_block_comment = false;
                        }
                        continue;
                    }
                    if in_string {
                        if c == '\\' {
                            chars.next();
                        } else if c == '"' {
                            in_string = false;
                        }
                        continue;
                    }
                    match c {
                        '"' => in_string = true,
                        '/' if chars.peek() == Some(&'/') => break, // line comment
                        '/' if chars.peek() == Some(&'*') => {
                            chars.next();
                            in_block_comment = true;
                        }
                        '{' => {
                            depth += 1;
                            start_brace_seen = true;
                        }
                        '}' => {
                            depth -= 1;
                            if start_brace_seen && depth == 0 {
                                end_line = j as u32;
                                break 'outer;
                            }
                        }
                        _ => {}
                    }
                }
                j += 1;
            }
            out.push((i as u32, end_line, name));
            i = j + 1;
            continue;
        }
        i += 1;
    }
    out
}

/// Match a `fn name(` declaration on a single line and extract the name.
/// Tolerates `pub `, `pub(crate) `, `async `, and `unsafe ` modifiers.
/// Returns `None` for `fn` keywords that are part of a type (`extern "C" fn`)
/// or a closure (`|x: fn(u8) -> u8|`) — those don't appear at the start of
/// a fixture line in this workspace.
fn match_fn_declaration(raw: &str) -> Option<String> {
    let trimmed = raw.trim_start();
    // Strip common leading modifiers in declaration position.
    let body = trimmed
        .strip_prefix("pub(crate) ")
        .or_else(|| trimmed.strip_prefix("pub "))
        .unwrap_or(trimmed);
    let body = body
        .strip_prefix("async ")
        .or_else(|| body.strip_prefix("unsafe "))
        .unwrap_or(body);
    let rest = body.strip_prefix("fn ")?;
    // Name = identifier chars up to the first `(` / `<` / whitespace.
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    let name = &rest[..end];
    // Make sure `(` follows (possibly via `<...>` generics — allow both).
    let after = rest[end..].trim_start();
    if after.starts_with('(') || after.starts_with('<') {
        Some(name.to_string())
    } else {
        None
    }
}

/// Strip leading whitespace and any trailing line comment so the
/// pattern-detector can do simple substring checks without being thrown
/// off by trailing `// ...` annotations on a let-binding line.
fn strip_line_for_match(raw: &str) -> &str {
    let trimmed = raw.trim();
    if let Some(idx) = trimmed.find("//") {
        trimmed[..idx].trim_end()
    } else {
        trimmed
    }
}

/// Extract the LHS name of a `let mut? NAME ...` binding.  Returns `None`
/// if `text` doesn't look like a let-binding.
fn extract_let_lhs(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("let ")?;
    let rest = rest.strip_prefix("mut ").unwrap_or(rest);
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    Some(&rest[..end])
}

/// Slice out the substring between the first `=` and the trailing `;`,
/// then trim.  Returns `None` if no `=` is present or the slice is empty.
fn extract_let_rhs(text: &str) -> Option<&str> {
    let eq = text.find('=')?;
    let mut rhs = text[eq + 1..].trim();
    if let Some(stripped) = rhs.strip_suffix(';') {
        rhs = stripped.trim();
    }
    if rhs.is_empty() {
        return None;
    }
    Some(rhs)
}

/// Extract both the format-string argument and the trailing positional
/// argument expressions from a Rust macro call (`msg!("fmt", a, b)`,
/// `panic!("fmt", x)`).  Each trailing arg is returned as the trimmed
/// source text (e.g. `"balance"`, `"new_balance"`).  Returns `None` if
/// the line doesn't contain the macro or the format-string argument
/// isn't a simple `"..."` literal.
fn extract_macro_call(line: &str, macro_name: &str) -> Option<(String, Vec<String>)> {
    let needle = format!("{macro_name}!");
    let idx = line.find(&needle)?;
    let after = &line[idx + needle.len()..];
    // Skip optional whitespace then `(` or `[` or `{` opener.
    let after = after.trim_start();
    let mut chars = after.chars();
    let opener = chars.next()?;
    if !matches!(opener, '(' | '[' | '{') {
        return None;
    }
    let after = chars.as_str();
    // Find first `"` and capture until next unescaped `"`.
    let q1 = after.find('"')?;
    let body = &after[q1 + 1..];
    let mut content = String::new();
    let mut iter = body.chars();
    let mut closed = false;
    let mut consumed_bytes = 0usize; // bytes consumed from `body` up to and including the closing `"`.
    while let Some(c) = iter.next() {
        consumed_bytes += c.len_utf8();
        if c == '"' {
            closed = true;
            break;
        }
        if c == '\\' {
            if let Some(next) = iter.next() {
                consumed_bytes += next.len_utf8();
                content.push(c);
                content.push(next);
            }
        } else {
            content.push(c);
        }
    }
    if !closed {
        return None;
    }
    // Tail = everything after the closing `"`.
    let tail = &body[consumed_bytes..];
    let tail = tail.trim_start();
    let mut args: Vec<String> = Vec::new();
    if let Some(rest) = tail.strip_prefix(',') {
        // Find the matching close-delimiter (`)` for `(`, `]` for `[`, `}` for `{`).
        let close = match opener {
            '(' => ')',
            '[' => ']',
            '{' => '}',
            _ => return None,
        };
        // Walk the tail to find the matching close.
        let mut depth: i32 = 1; // we're past the `opener` already
        let mut end_byte = rest.len();
        let mut in_string = false;
        let mut prev: char = ' ';
        for (i, c) in rest.char_indices() {
            if in_string {
                if c == '"' && prev != '\\' {
                    in_string = false;
                }
                prev = c;
                continue;
            }
            match c {
                '"' => in_string = true,
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => {
                    if c == close {
                        depth -= 1;
                        if depth == 0 {
                            end_byte = i;
                            break;
                        }
                    } else {
                        depth -= 1;
                    }
                }
                _ => {}
            }
            prev = c;
        }
        let inside = &rest[..end_byte];
        for tok in split_top_level_commas(inside) {
            let t = tok.trim();
            if !t.is_empty() {
                args.push(t.to_string());
            }
        }
    }
    Some((content, args))
}

/// Parse a comma-separated argument list into trimmed segments, honouring
/// parens/brackets/braces.  Used by the tuple, struct, and array
/// synthesisers.  `text` should NOT include the outer delimiters.
fn split_top_level_commas(text: &str) -> Vec<&str> {
    let mut depth: i32 = 0;
    let mut last = 0usize;
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut in_string = false;
    let mut prev_byte: u8 = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if in_string {
            if b == b'"' && prev_byte != b'\\' {
                in_string = false;
            }
            prev_byte = b;
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b',' if depth == 0 => {
                out.push(text[last..i].trim());
                last = i + 1;
            }
            _ => {}
        }
        prev_byte = b;
    }
    let tail = text[last..].trim();
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

/// Parse a Rust integer literal ("`1`", "`-2`", "`1_000`", "`true`"
/// (treated as 1), `"false"` (0)) into an `i64`.  Returns `None` for
/// anything we can't recognise so the caller falls back to skipping the
/// element.
fn parse_int_literal(text: &str) -> Option<i64> {
    let s = text.trim();
    if s == "true" {
        return Some(1);
    }
    if s == "false" {
        return Some(0);
    }
    let cleaned: String = s.chars().filter(|c| *c != '_').collect();
    cleaned.parse::<i64>().ok()
}

/// Try to detect an `EnumPath::Variant ...` enum-variant construction
/// in `rhs` and decode it into the enum path, the variant name, and a
/// payload shape (struct-form named fields, tuple-form positional
/// values, or a unit variant).  Returns `None` when `rhs` doesn't look
/// like an enum-variant construction — the caller falls back to the
/// existing struct-literal detector.
///
/// Recognised forms (all real Solana instruction-enum idioms):
///   * `MyInstruction::Init { lamports: 500 }` (struct-like variant)
///   * `MyInstruction::Update(7)` (tuple-like variant; positional ints)
///   * `MyInstruction::Close` (unit variant)
///
/// The path must contain `::` and start with an uppercase letter so we
/// don't accidentally match `account.field` access (no `::`) or a bare
/// type name (no `::`) which is already handled by the struct-literal
/// detector.
fn parse_variant_construction(rhs: &str) -> Option<(String, String, VariantPayload)> {
    let rhs = rhs.trim_start_matches('&').trim();
    let rhs = rhs.strip_prefix("mut ").unwrap_or(rhs);
    // The path component is everything before the first `{` / `(` / end.
    let mut head_end = rhs.len();
    for (i, c) in rhs.char_indices() {
        if c == '{' || c == '(' || c.is_whitespace() {
            head_end = i;
            break;
        }
    }
    let head = rhs[..head_end].trim();
    if head.is_empty() {
        return None;
    }
    // Require an `::` separator and an uppercase first character so we
    // distinguish enum variants from struct literals and function calls.
    let sep = head.rfind("::")?;
    if !head.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
        return None;
    }
    let enum_path = head[..sep].trim().to_string();
    let variant = head[sep + 2..].trim().to_string();
    if enum_path.is_empty() || variant.is_empty() {
        return None;
    }
    // The variant name must start with an uppercase letter.  Guards
    // against `account.is_signer::clone` (no chance in practice but be
    // safe) and turbofish-style `::<T>` references.
    if !variant
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_uppercase())
    {
        return None;
    }
    let tail = rhs[head_end..].trim();
    if let Some(after_brace) = tail.strip_prefix('{') {
        // Struct-form variant: `MyInstruction::Init { lamports: 500 }`.
        let close = after_brace.rfind('}')?;
        let body = &after_brace[..close];
        let mut fields = Vec::new();
        for tok in split_top_level_commas(body) {
            let Some(colon) = tok.find(':') else { continue };
            let name = tok[..colon].trim().to_string();
            let val = tok[colon + 1..].trim();
            if let Some(i) = parse_int_literal(val) {
                fields.push((name, i));
            }
        }
        return Some((enum_path, variant, VariantPayload::Struct(fields)));
    }
    if let Some(after_paren) = tail.strip_prefix('(') {
        let close = after_paren.rfind(')')?;
        let body = after_paren[..close].trim();
        let mut elements = Vec::new();
        for tok in split_top_level_commas(body) {
            if let Some(i) = parse_int_literal(tok) {
                elements.push(i);
            }
        }
        return Some((enum_path, variant, VariantPayload::Tuple(elements)));
    }
    // Unit variant (no `{` / `(` follows).
    if tail.is_empty() || tail == ";" {
        return Some((enum_path, variant, VariantPayload::Unit));
    }
    None
}

#[derive(Debug)]
enum VariantPayload {
    Struct(Vec<(String, i64)>),
    Tuple(Vec<i64>),
    Unit,
}

/// Try to detect a `Name { field: value, ... }` struct literal in `rhs`
/// and decode it into `(struct_name, field_values)` tuples for the
/// caller to pass to `register_variable_with_full_value`.  Multi-line
/// struct literals are condensed by the caller before calling us
/// (see `collect_struct_literal_lines`).  Field values that aren't
/// simple int / bool literals are dropped — keeping the synthesiser
/// honest about what it can decode without a full Rust parser.
fn parse_struct_literal(rhs: &str) -> Option<(String, Vec<(String, i64)>)> {
    // Skip a leading `&mut ` / `&` (e.g. `let r = &mut Account { .. };`).
    let rhs = rhs.trim_start_matches('&').trim();
    let rhs = rhs.strip_prefix("mut ").unwrap_or(rhs);
    let brace = rhs.find('{')?;
    let name_part = rhs[..brace].trim();
    if name_part.is_empty() {
        return None;
    }
    // Type names start with an uppercase letter — guards against tuple
    // returns being mistaken for structs.
    let first = name_part.chars().next()?;
    if !first.is_ascii_uppercase() {
        return None;
    }
    let close = rhs.rfind('}')?;
    if close <= brace {
        return None;
    }
    let body = &rhs[brace + 1..close];
    let mut fields = Vec::new();
    for tok in split_top_level_commas(body) {
        let colon = tok.find(':')?;
        let name = tok[..colon].trim().to_string();
        let val = tok[colon + 1..].trim();
        if let Some(i) = parse_int_literal(val) {
            fields.push((name, i));
        }
    }
    Some((name_part.to_string(), fields))
}

/// Strip an outer `"..."` Rust string literal and return its inner text.
/// Returns `None` if `text` doesn't begin and end with `"` (so callers
/// fall through to other decode paths).  Backslash escapes are preserved
/// verbatim — the synthesiser treats the literal as opaque text the
/// debugger can render however it likes.
fn parse_string_literal(text: &str) -> Option<String> {
    let s = text.trim();
    let inner = s.strip_prefix('"')?.strip_suffix('"')?;
    Some(inner.to_string())
}

/// Decode an `&[u8; 32]` byte-array literal that surfaces inside a
/// `Pubkey::new_from_array([..])` call.  Returns the canonical base58
/// placeholder for the "all-zeros" case (`Pubkey::default()`) and the
/// raw `[..]` token form otherwise — the synthesiser's job is to
/// surface the *shape* of the literal, not to perform real base58
/// encoding (which would require pulling in `bs58` for a placeholder).
fn pubkey_call_to_string(rhs: &str) -> Option<String> {
    let s = rhs.trim();
    // `Pubkey::default()` → canonical 11111... placeholder.  The string
    // matches what `bs58::encode([0u8; 32])` would produce so a debugger
    // rendering this trace shows the expected on-chain key.
    if s == "Pubkey::default()" {
        return Some("11111111111111111111111111111111".to_string());
    }
    // `Pubkey::new_from_array([..])` → preserve the array literal as the
    // placeholder text.  Real base58 encoding is the recorder's job once
    // the SBF VM is wired up; today the strict tests pin the literal
    // surface form so a regression in the parser is loud.
    let after = s.strip_prefix("Pubkey::new_from_array(")?;
    let close = after.rfind(')')?;
    Some(format!("Pubkey::new_from_array({})", after[..close].trim()))
}

/// Decode a single field-value RHS expression into a `ValueRecord`.
/// Supports:
///   * integer / bool literals (`42`, `true`),
///   * string literals (`"hello"`),
///   * `vec![..]` / `[..]` array literals (Sequence),
///   * tuple literals (`(a, b)` with at least 2 elements),
///   * nested `Name { .. }` struct literals (recursive),
///   * `Pubkey::default()` / `Pubkey::new_from_array([..])`
///     (decoded as `String` placeholders so the strict pin can assert
///     on the canonical surface form without a live SBF VM).
///
/// Returns `None` for anything else so the caller can drop the field
/// (mirrors the legacy int/bool-only behaviour for unknown shapes).
fn decode_value_literal(
    rhs: &str,
    type_ids: &mut TypeIdCache,
    writer: &mut dyn TraceWriter,
) -> Option<ValueRecord> {
    let s = rhs.trim().trim_end_matches(',');
    if s.is_empty() {
        return None;
    }
    // Pubkey calls — both `Pubkey::default()` and
    // `Pubkey::new_from_array([..])` decode as base58 / shape placeholders.
    if s.starts_with("Pubkey::")
        && let Some(text) = pubkey_call_to_string(s)
    {
        return Some(ValueRecord::String {
            text,
            type_id: type_ids.string,
        });
    }
    // String literal.
    if let Some(text) = parse_string_literal(s) {
        return Some(ValueRecord::String {
            text,
            type_id: type_ids.string,
        });
    }
    // Bool / int literal.
    if let Some(i) = parse_int_literal(s) {
        return Some(ValueRecord::Int {
            i,
            type_id: type_ids.int,
        });
    }
    // `vec![..]` / `[..]` sequence literal.
    if let Some(elements_csv) = extract_array_or_vec_literal(s) {
        let mut elements = Vec::new();
        for tok in split_top_level_commas(elements_csv) {
            if let Some(v) = decode_value_literal(tok, type_ids, writer) {
                elements.push(v);
            }
        }
        if !elements.is_empty() {
            return Some(ValueRecord::Sequence {
                elements,
                is_slice: false,
                type_id: type_ids.seq,
            });
        }
    }
    // Tuple literal.
    if let Some(elements_csv) = extract_tuple_literal(s) {
        let mut elements = Vec::new();
        for tok in split_top_level_commas(elements_csv) {
            if let Some(v) = decode_value_literal(tok, type_ids, writer) {
                elements.push(v);
            }
        }
        if elements.len() >= 2 {
            return Some(ValueRecord::Tuple {
                elements,
                type_id: type_ids.tuple,
            });
        }
    }
    // Nested `Name { .. }` struct literal — recurse via the rich parser.
    if let Some((struct_name, fields)) = parse_struct_literal_rich(s, type_ids, writer) {
        let type_id = type_ids.ensure_struct(writer, &struct_name);
        let field_values = fields.into_iter().map(|(_, v)| v).collect();
        return Some(ValueRecord::Struct {
            field_values,
            type_id,
        });
    }
    None
}

/// Returns `true` when `value` is a `Result`-shaped `Variant` whose
/// outer discriminator is `"Err"`.  Used by the recorder loop to
/// identify error-shaped returns that should fill the
/// `?`-propagation slot (`last_err_return`) so the caller's
/// `?`-bearing line can re-emit the same typed value without
/// re-parsing the callee's source.
fn is_err_variant(value: &ValueRecord) -> bool {
    matches!(
        value,
        ValueRecord::Variant { discriminator, .. } if discriminator == "Err"
    )
}

/// `synthesise_return_value` extended with `?`-propagation: when the
/// source line at `line_no` ends in a `?;` (the canonical
/// early-return shape), the recorder re-emits the most recently
/// observed `Err`-shaped return value from `last_err`.  This mirrors
/// Rust's `?` semantics — the operator forwards the same `Err`
/// up the call stack rather than chaining a fresh wrapper.
fn synthesise_return_value_with_propagation(
    model: &SourceModel,
    line_no: u32,
    type_ids: &mut TypeIdCache,
    writer: &mut dyn TraceWriter,
    last_err: Option<&ValueRecord>,
) -> Option<ValueRecord> {
    if let Some(v) = synthesise_return_value(model, line_no, type_ids, writer) {
        return Some(v);
    }
    let raw = model.line(line_no);
    let text = strip_line_for_match(raw);
    if line_carries_propagation(text) {
        return last_err.cloned();
    }
    None
}

/// Returns `true` when `text` carries a `?` operator at expression
/// position — the canonical early-return shape (`let v = call()?;` or
/// `call()?` as an expression statement).  Excludes `?` characters
/// inside string literals via a simple state machine so commented or
/// quoted markers don't fire.
fn line_carries_propagation(text: &str) -> bool {
    let mut in_string = false;
    let mut prev = ' ';
    for c in text.chars() {
        if in_string {
            if c == '"' && prev != '\\' {
                in_string = false;
            }
            prev = c;
            continue;
        }
        if c == '"' {
            in_string = true;
        } else if c == '?' {
            return true;
        }
        prev = c;
    }
    false
}

/// Inspect the source text at `line_no` for a `return Err(..)` /
/// `return Ok(..)` / bare `Err(..)` / bare `Ok(..)` expression and
/// synthesise the matching `Result`-shaped `ValueRecord::Variant`.
/// Returns `None` when the line doesn't carry an explicit return shape
/// — the caller falls back to `NONE_VALUE` (today's behaviour).
///
/// Decoding rules for the inner payload:
///   * `Path::Variant` (unit variant) → `Variant { discriminator, contents: Tuple([]) }`
///   * `Path::Variant(args)` (tuple variant) → `Variant { discriminator, contents: Tuple(args) }`
///   * Any other recognised literal → handled via [`decode_value_literal`].
///   * Unrecognised shapes degrade to a `String` placeholder carrying
///     the raw payload text so the strict test can still pin a stable
///     surface form without needing a full Rust parser.
fn synthesise_return_value(
    model: &SourceModel,
    line_no: u32,
    type_ids: &mut TypeIdCache,
    writer: &mut dyn TraceWriter,
) -> Option<ValueRecord> {
    let raw = model.line(line_no);
    let text = strip_line_for_match(raw);
    if text.is_empty() {
        return None;
    }
    let (discriminator, payload_text) = if text.contains("return Err(") {
        ("Err".to_string(), extract_err_payload(text)?)
    } else if text.starts_with("Err(") {
        ("Err".to_string(), extract_err_payload(text)?)
    } else if let Some(stripped) = text.strip_prefix("return Ok(") {
        let close = stripped.rfind(')')?;
        ("Ok".to_string(), stripped[..close].trim().to_string())
    } else {
        let stripped = text.strip_prefix("Ok(")?;
        let close = stripped.rfind(')')?;
        ("Ok".to_string(), stripped[..close].trim().to_string())
    };
    let inner = decode_return_payload(&payload_text, type_ids, writer);
    let result_type_id = type_ids.ensure_variant(writer, "Result");
    Some(ValueRecord::Variant {
        discriminator,
        contents: Box::new(inner),
        type_id: result_type_id,
    })
}

/// Decode the inner payload of a `Result::Err(..)` / `Result::Ok(..)`
/// expression into a typed `ValueRecord`.  Recognises:
///   * `Path::Variant` unit-variant constructions (canonical
///     `ProgramError::MissingRequiredSignature` shape) — surface as a
///     nested `Variant` whose discriminator is the variant name.
///   * `Path::Variant(args)` tuple-variant constructions
///     (`ProgramError::Custom(42)` shape) — surface as a nested
///     `Variant` whose contents is the decoded `Tuple`.
///   * Any expression `decode_value_literal` recognises (int / bool /
///     string / vec / array / tuple / struct / Pubkey-call) — surface
///     directly.
///   * Unrecognised shapes — degrade to a `String` placeholder
///     carrying the verbatim payload text so the strict pin still has
///     a deterministic surface.
fn decode_return_payload(
    payload: &str,
    type_ids: &mut TypeIdCache,
    writer: &mut dyn TraceWriter,
) -> ValueRecord {
    let s = payload.trim();
    // Enum-variant style payloads: `EnumPath::Variant`
    // (unit) or `EnumPath::Variant(args)` (tuple).  Reuse
    // `parse_variant_construction` so the decode rules stay aligned
    // with the let-binding-side variant emission.
    if let Some((enum_path, variant, vp)) = parse_variant_construction(s) {
        let contents = match vp {
            VariantPayload::Struct(fields) => {
                let field_values: Vec<ValueRecord> = fields
                    .iter()
                    .map(|(_, i)| ValueRecord::Int {
                        i: *i,
                        type_id: type_ids.int,
                    })
                    .collect();
                let struct_type_id = type_ids
                    .struct_type_for(&variant)
                    .unwrap_or(type_ids.struct_default);
                ValueRecord::Struct {
                    field_values,
                    type_id: struct_type_id,
                }
            }
            VariantPayload::Tuple(values) => {
                let elements: Vec<ValueRecord> = values
                    .iter()
                    .map(|i| ValueRecord::Int {
                        i: *i,
                        type_id: type_ids.int,
                    })
                    .collect();
                ValueRecord::Tuple {
                    elements,
                    type_id: type_ids.tuple,
                }
            }
            VariantPayload::Unit => ValueRecord::Tuple {
                elements: Vec::new(),
                type_id: type_ids.tuple,
            },
        };
        let variant_type_id = type_ids.ensure_variant(writer, &enum_path);
        return ValueRecord::Variant {
            discriminator: variant,
            contents: Box::new(contents),
            type_id: variant_type_id,
        };
    }
    if let Some(value) = decode_value_literal(s, type_ids, writer) {
        return value;
    }
    // Fallback: surface the verbatim payload text so the strict pin
    // can still anchor on it (rather than a NONE placeholder).
    ValueRecord::String {
        text: s.to_string(),
        type_id: type_ids.string,
    }
}

/// Rich variant of [`parse_struct_literal`] that decodes each field
/// value into a typed [`ValueRecord`] via [`decode_value_literal`]
/// rather than dropping non-int fields.  Used by the synthesiser's
/// emission path so nested struct literals, string fields, `Vec<T>`
/// fields, and `Pubkey::default()` placeholders all surface with their
/// proper types instead of being silently elided.
///
/// Field values that don't match any known literal shape are still
/// dropped (no `Raw`/`Error` placeholders) — mirrors the legacy
/// behaviour so unrecognised shapes never produce ghost fields the
/// strict tests would have to special-case.
fn parse_struct_literal_rich(
    rhs: &str,
    type_ids: &mut TypeIdCache,
    writer: &mut dyn TraceWriter,
) -> Option<(String, Vec<(String, ValueRecord)>)> {
    // Mirror `parse_struct_literal`'s outer-shape detection so the two
    // entry points stay in lock-step.
    let rhs = rhs.trim_start_matches('&').trim();
    let rhs = rhs.strip_prefix("mut ").unwrap_or(rhs);
    let brace = rhs.find('{')?;
    let name_part = rhs[..brace].trim();
    if name_part.is_empty() {
        return None;
    }
    let first = name_part.chars().next()?;
    if !first.is_ascii_uppercase() {
        return None;
    }
    // Reject `Foo::Bar { .. }` enum-variant constructions — those have
    // their own dedicated decode path (`parse_variant_construction`)
    // and would otherwise be mis-registered as a struct named
    // `Foo::Bar`.
    if name_part.contains("::") {
        return None;
    }
    let close = rhs.rfind('}')?;
    if close <= brace {
        return None;
    }
    let body = &rhs[brace + 1..close];
    let mut fields: Vec<(String, ValueRecord)> = Vec::new();
    for tok in split_top_level_commas(body) {
        let Some(colon) = tok.find(':') else { continue };
        let name = tok[..colon].trim().to_string();
        let val = tok[colon + 1..].trim();
        if let Some(v) = decode_value_literal(val, type_ids, writer) {
            fields.push((name, v));
        }
    }
    Some((name_part.to_string(), fields))
}

/// Stitch together the contiguous source lines that begin a multi-line
/// struct literal at `start_line` until the matching `}` closes it.
/// Falls back to the single line at `start_line` if no opening `{`
/// appears on it (the caller's pattern check is conservative).
fn collect_struct_literal_lines(model: &SourceModel, start_line: u32) -> String {
    let first = model.line(start_line);
    if !first.contains('{') {
        return first.trim().to_string();
    }
    let mut depth: i32 = 0;
    let mut acc = String::new();
    let mut line_no = start_line;
    while (line_no as usize) < model.lines.len() {
        let raw = model.line(line_no);
        let stripped = strip_line_for_match(raw);
        if !acc.is_empty() {
            acc.push(' ');
        }
        acc.push_str(stripped);
        for c in stripped.chars() {
            match c {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
        }
        if depth <= 0 && acc.contains('}') {
            break;
        }
        line_no += 1;
    }
    acc
}

/// Synthesise the spec-mandated `RecordEvent`s for a step at `line_no`.
/// Called immediately after `register_step` in the snapshot loop.
///
/// It emits EVENTS only, never variables. Recorder-Solana CXP-SOL-2 (operator
/// decision 2026-11-12): this path has no DWARF variable information, and the
/// trace format cannot mark a name as inferred, so it records no name or value
/// matched from source text. A `msg!`/`panic!` payload is its literal format
/// string, never interpolated with a guessed binding; named locals come only
/// from `source_recorder` when DWARF describes the program.
fn synthesise_step_events(model: &SourceModel, writer: &mut dyn TraceWriter, line_no: u32) {
    let raw = model.line(line_no);
    let text = strip_line_for_match(raw);
    if text.is_empty() {
        return;
    }

    // Side-effect macros / errors → io_events.
    if text.contains("msg!(") {
        let payload = match extract_macro_call(text, "msg") {
            Some((fmt, _)) => fmt,
            None => "<msg!>".to_string(),
        };
        TraceWriter::register_special_event(writer, EventLogKind::Write, "SolanaMsg", &payload);
    }
    if text.contains("panic!(") {
        let payload = match extract_macro_call(text, "panic") {
            Some((fmt, _)) => fmt,
            None => "<panic!>".to_string(),
        };
        TraceWriter::register_special_event(writer, EventLogKind::Error, "SolanaPanic", &payload);
    }
    // `return Err(..)` and bare `Err(..)` literals.  Excludes `assert_err`
    // / `serde::Error` / etc. by anchoring on `Err(` after `return ` or
    // standalone-with-non-ident-prefix.
    if text.contains("return Err(") || starts_with_err_literal(text) {
        let payload = extract_err_payload(text).unwrap_or_else(|| "<Err>".to_string());
        TraceWriter::register_special_event(writer, EventLogKind::Error, "SolanaError", &payload);
    }
    // `sol_log_data!(...)` — Solana's binary log syscall, distinct from
    // `msg!`.  The args are an arbitrary `&[&[u8]]` slice expression
    // (e.g. `&[b"event", &payload]`) we don't try to interpret in the
    // synthetic-snapshot pipeline.  Surfaced as a `TraceLogEvent`
    // io_event tagged `SolanaLogData` with a `data:<source-text>` payload
    // so the strict pin can distinguish it from a `Write`-tagged
    // `msg!` event by `io_kind` (TraceLogEvent maps to `ioStderr` in
    // the multi-stream IO event stream).
    if text.contains("sol_log_data!(") {
        let payload = extract_macro_args_raw(text, "sol_log_data")
            .map(|a| format!("data:{a}"))
            .unwrap_or_else(|| "<sol_log_data>".to_string());
        TraceWriter::register_special_event(
            writer,
            EventLogKind::TraceLogEvent,
            "SolanaLogData",
            &payload,
        );
    }
    // `sol_log_compute_units!(<remaining>)` — Solana's compute-units
    // metering syscall.  The synthetic-snapshot pipeline can't observe
    // the BPF VM's actual remaining-units counter, so the fixture
    // stand-in macro carries the canonical value in its single integer
    // literal arg; the recorder parses it out and emits a metadata-style
    // `TraceLogEvent` event tagged `SolanaCompute` with the
    // `compute_units_remaining=<N>` payload.
    if text.contains("sol_log_compute_units!(") {
        let payload = extract_macro_args_raw(text, "sol_log_compute_units")
            .and_then(|a| {
                let trimmed = a.trim();
                trimmed.parse::<i64>().ok().map(|n| n.to_string())
            })
            .map(|n| format!("compute_units_remaining={n}"))
            .unwrap_or_else(|| "compute_units_remaining=?".to_string());
        TraceWriter::register_special_event(
            writer,
            EventLogKind::TraceLogEvent,
            "SolanaCompute",
            &payload,
        );
    }
}

fn starts_with_err_literal(text: &str) -> bool {
    // Recognise bare `Err(` at column 0 of the trimmed line (e.g. an
    // expression-position `Err(...)` returned from a match arm) without
    // matching identifiers ending in `Err`.
    text.starts_with("Err(")
}

/// Extract the raw inside-parens text of `<macro_name>!(...)`.  Unlike
/// [`extract_macro_call`], this does NOT try to parse out a leading
/// `"format"` string literal — it returns the entire arg expression
/// verbatim (with leading/trailing whitespace stripped).  Used by the
/// `sol_log_data!` / `sol_log_compute_units!` recognisers where the
/// argument is a slice / int literal (no format-string semantics).
fn extract_macro_args_raw(text: &str, macro_name: &str) -> Option<String> {
    let needle = format!("{macro_name}!(");
    let idx = text.find(&needle)?;
    let after = &text[idx + needle.len()..];
    // Walk to the matching `)` with depth balance, ignoring `(` / `)`
    // inside `"..."` strings.
    let mut depth: i32 = 1;
    let mut end_byte = after.len();
    let mut in_string = false;
    let mut prev: char = ' ';
    for (i, c) in after.char_indices() {
        if in_string {
            if c == '"' && prev != '\\' {
                in_string = false;
            }
            prev = c;
            continue;
        }
        match c {
            '"' => in_string = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    end_byte = i;
                    break;
                }
            }
            _ => {}
        }
        prev = c;
    }
    if depth != 0 {
        return None;
    }
    Some(after[..end_byte].trim().to_string())
}

fn extract_err_payload(text: &str) -> Option<String> {
    let idx = text.find("Err(")?;
    let after = &text[idx + 4..];
    // Capture the matching `)`; if not balanced, fall through.
    let mut depth = 1;
    let mut end = 0;
    for (i, c) in after.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    end = i;
                    break;
                }
            }
            _ => {}
        }
    }
    if end == 0 {
        return None;
    }
    Some(after[..end].to_string())
}

fn extract_array_or_vec_literal(rhs: &str) -> Option<&str> {
    // `vec![ ... ]`
    if let Some(after) = rhs.strip_prefix("vec![") {
        let end = after.rfind(']')?;
        return Some(after[..end].trim());
    }
    // `&[..]` / `[..]` array literal (bracketed; cardinality >=1) — the
    // caller's pattern guards against `xs[i]` indexing because that uses
    // a name (not `[`) before the `[`.
    let r = rhs.trim_start_matches('&').trim();
    if let Some(after) = r.strip_prefix('[') {
        let end = after.rfind(']')?;
        let inner = after[..end].trim();
        // Reject `[i64; 4]` type-only forms: those have a `;` separator.
        if inner.contains(';') {
            // It might be `[1, 2, 3, 4]` AFTER a `: [i64; 4] =` —
            // already past the `=` so this branch shouldn't fire, but
            // be defensive.
            return None;
        }
        if inner.is_empty() {
            return None;
        }
        return Some(inner);
    }
    None
}

fn extract_tuple_literal(rhs: &str) -> Option<&str> {
    let r = rhs.trim();
    let after = r.strip_prefix('(')?;
    // Find matching `)`.  Use rfind to skip nested parens.
    let end = after.rfind(')')?;
    let inner = after[..end].trim();
    if inner.is_empty() {
        return None;
    }
    let parts = split_top_level_commas(inner);
    // At least 2 elements distinguishes a tuple literal from a parenthesised
    // expression like `(x + y)`.
    if parts.len() < 2 {
        return None;
    }
    Some(inner)
}

/// Cached `TypeId`s used by the synthesiser so we don't re-register on
/// every step.  Per-struct ids are populated lazily.
struct TypeIdCache {
    int: TypeId,
    seq: TypeId,
    tuple: TypeId,
    string: TypeId,
    struct_default: TypeId,
    /// Fallback variant TypeId for the rare case where the synthesiser
    /// needs to emit a `ValueRecord::Variant` for an enum path that
    /// wasn't pre-registered.  Today every emission path uses
    /// `ensure_variant`, but the field is kept symmetric with
    /// `struct_default` so future code paths that lazy-emit before the
    /// pre-scan completes have a deterministic fallback.
    #[allow(dead_code)]
    variant_default: TypeId,
    structs: HashMap<String, TypeId>,
    variants: HashMap<String, TypeId>,
}

impl TypeIdCache {
    fn new(writer: &mut dyn TraceWriter) -> Self {
        let int = TraceWriter::ensure_type_id(writer, TypeKind::Int, "u64");
        let seq = TraceWriter::ensure_type_id(writer, TypeKind::Seq, "Vec<u64>");
        let tuple = TraceWriter::ensure_type_id(writer, TypeKind::Tuple, "(u64, u64)");
        let string = TraceWriter::ensure_type_id(writer, TypeKind::String, "string");
        let struct_default = TraceWriter::ensure_type_id(writer, TypeKind::Struct, "Struct");
        let variant_default = TraceWriter::ensure_type_id(writer, TypeKind::Variant, "Variant");
        Self {
            int,
            seq,
            tuple,
            string,
            struct_default,
            variant_default,
            structs: HashMap::new(),
            variants: HashMap::new(),
        }
    }

    fn ensure_struct(&mut self, writer: &mut dyn TraceWriter, name: &str) -> TypeId {
        if let Some(id) = self.structs.get(name) {
            return *id;
        }
        let id = TraceWriter::ensure_type_id(writer, TypeKind::Struct, name);
        self.structs.insert(name.to_string(), id);
        id
    }

    fn struct_type_for(&self, name: &str) -> Option<TypeId> {
        self.structs.get(name).copied()
    }

    fn ensure_variant(&mut self, writer: &mut dyn TraceWriter, enum_path: &str) -> TypeId {
        if let Some(id) = self.variants.get(enum_path) {
            return *id;
        }
        let id = TraceWriter::ensure_type_id(writer, TypeKind::Variant, enum_path);
        self.variants.insert(enum_path.to_string(), id);
        id
    }
}

/// Core recording logic that writes into any TraceWriter.
/// Useful for tests with NonStreamingTraceWriter.
pub fn record_from_snapshots_into_writer(
    snapshots: &[RegisterSnapshot],
    source_locations: &[(u64, &str, u32, Option<u32>)],
    source_path: &Path,
    writer: &mut dyn TraceWriter,
) -> Result<()> {
    // Build a PC -> (file, line, column) lookup.
    let pc_to_loc: std::collections::HashMap<u64, (&str, u32, Option<u32>)> = source_locations
        .iter()
        .map(|(pc, file, line, column)| (*pc, (*file, *line, *column)))
        .collect();

    // Opt the writer into column-aware step encoding so the trace's
    // `meta.dat` carries `FlagHasColumnAwareSteps` (bit 4) and the
    // emitter is permitted to layer `DeltaColumn` events on each step.
    // Must be called before the first `register_step_with_column`.  When
    // every source location has `column == None` (e.g. fixtures that pass
    // only line info) the writer still surfaces the flag, but emits zero
    // `DeltaColumn` events — column-aware readers handle that cleanly,
    // and the existing per-program ct-print test asserts exact step
    // counts so any spurious DeltaColumn would fail loudly.
    writer.enable_column_aware_steps();

    // M-capability-flags: Solana's PC→source maps mean each VM
    // instruction has a sharp sub-statement source position, so the
    // GUI can offer per-column breakpoints and per-column motions
    // against the recorded steps.  Advertise both capabilities so the
    // M6 Alt+click affordance and sub-statement step buttons are
    // available.
    writer.enable_column_breakpoints_support();
    writer.enable_column_motions_support();

    // Register every source file with its per-line table before the first
    // record that names it (`start` names the anchor), under the one path
    // string every later record uses for it. See `SourcePaths`.
    let mut paths = SourcePaths::new(source_path);
    paths.ensure(writer, &source_path.to_string_lossy());
    for (_, file_str, _, _) in source_locations {
        paths.ensure(writer, file_str);
    }

    // Load and analyse the fixture source so we can resolve nested call
    // frames to real function names and synthesise the spec-mandated
    // syscall / typed-value events the SBF synthetic-snapshot pipeline
    // can't observe on its own.  Missing / unreadable source files
    // degrade to an empty model and the recorder falls back to the
    // pre-existing `fn_at_pc_<pc>` / Int-only behaviour.
    let model = SourceModel::load(source_path);

    // Start the trace.
    TraceWriter::start(writer, source_path, Line(1));

    // Register all the type ids the synthesiser will need up front so
    // each registration call site stays cheap.
    let mut type_ids = TypeIdCache::new(writer);

    // Pre-populate per-struct / per-variant TypeIds for every literal
    // construction we might encounter.  The synthesiser also lazy-registers,
    // but walking the source up front keeps the type table deterministic
    // (declaration order) regardless of which step happens to fire first.
    for line_no in 1..(model.lines.len() as u32) {
        let raw = model.line(line_no);
        let text = strip_line_for_match(raw);
        if let (Some(_), Some(rhs)) = (extract_let_lhs(text), extract_let_rhs(text)) {
            // Enum-variant construction takes priority over struct detection
            // because `Foo::Bar { .. }` would otherwise be registered as a
            // struct named `Foo::Bar`.
            if let Some((enum_path, _variant, _payload)) = parse_variant_construction(rhs) {
                type_ids.ensure_variant(writer, &enum_path);
                continue;
            }
            let candidate = if rhs.contains('{') && !rhs.contains('}') {
                collect_struct_literal_lines(&model, line_no)
            } else {
                rhs.to_string()
            };
            let candidate_rhs = if let Some(eq) = candidate.find('=') {
                candidate[eq + 1..]
                    .trim()
                    .trim_end_matches(';')
                    .trim()
                    .to_string()
            } else {
                candidate
            };
            if let Some((struct_name, _)) = parse_struct_literal(&candidate_rhs) {
                type_ids.ensure_struct(writer, &struct_name);
            }
        }
    }

    // The outer call frame's name comes from the source: whichever
    // function contains the first visited line.  Falling back to "main"
    // matches the legacy behaviour for tests that pass synthetic
    // source paths (no source file on disk) or visit lines outside any
    // `fn` declaration.
    let outer_fn_name: String = snapshots
        .iter()
        .find_map(|snap| {
            let pc = snap.pc();
            let &(_file, line, _column) = pc_to_loc.get(&pc)?;
            model.function_at(line).map(str::to_string)
        })
        .unwrap_or_else(|| "main".to_string());

    let main_fn_id = TraceWriter::ensure_function_id(writer, &outer_fn_name, source_path, Line(1));

    // Emit initial call.
    TraceWriter::register_call(writer, main_fn_id, vec![]);

    // Track the call stack (function names, outermost first) so backward
    // jumps that cross multiple frames worth of return boundaries unwind
    // correctly.  The previous heuristic emitted exactly one
    // `register_return` per backward jump > 2, which mis-attributed the
    // exit when the snapshot stream skipped intermediate return sites
    // (e.g. inner → outer in one step pops both inner and middle in
    // call order).
    let mut fn_stack: Vec<String> = vec![outer_fn_name.clone()];

    // Walk snapshots.
    //
    // Snapshots whose DWARF source path is third-party
    // (``is_third_party_source``) are entirely silent in the trace:
    // no ``Step``, no ``Value`` events, no call-boundary push/pop.
    // The recorder still advances ``prev_pc`` across them so call and
    // return detection at the next user-source landing stays continuous.
    //
    // Why this beats the alternatives: SBF programs reach the
    // user's ``process_instruction`` through the
    // ``entrypoint!(process_instruction)`` macro which inline-
    // expands into ``solana-program-entrypoint``'s ``src/lib.rs``
    // (the trampoline that deserialises ``InstructionData`` and
    // calls into the user fn).  Those wrapper steps live in a
    // third-party file the user has no source on disk for, so DAP
    // ``next`` (step-over) at depth=0 would otherwise surface them
    // as user-visible destinations, VS Code couldn't open the file,
    // the editor cursor would stay put, and the WDIO ``performs
    // multiple step-over operations and changes line`` assertion
    // would see the same ``editor.selection.active.line`` every
    // iteration (cross-repo run 27626199922 / solana-deep.e2e.ts).
    //
    // Pushing a call frame at the macro-into-stdlib transition was
    // tried first (move the wrapper steps to ``depth=1`` so step-
    // over skips them) but the frame push reattached the trace's
    // very first ``Step`` to the inner ``fn_at_pc_<pc>`` frame (cross-repo
    // run 27640716644). Silently dropping third-party snapshots avoids it:
    // the trace contains only user-source steps, so step-over advances
    // through real lines and ``process_instruction`` spans every one.
    let mut prev_line: Option<u32> = None;
    // Track the previously emitted column on the same line so we can
    // fire a fresh column-aware step when the next snapshot lands on a
    // different column of the same line (the column-aware navigation
    // contract from `codetracer-trace-format-spec/trace-events.md`
    // §"Column Encoding").  Outside column-aware mode this stays at
    // `None` and the legacy line-only dedupe applies.
    let mut prev_column: Option<u32> = None;
    let mut prev_pc: Option<u64> = None;

    // Track the most recently synthesised `Err`-shaped return value so
    // `?`-propagation in the caller can re-emit it without re-parsing
    // the callee's source.  Updated each time the recorder emits a
    // typed `Result::Err` return (via `synthesise_return_value`); reset
    // to `None` whenever a non-error return is emitted.
    let mut last_err_return: Option<ValueRecord> = None;

    for snap in snapshots {
        let pc = snap.pc();

        // Look up source location for this PC.
        let (file_str, line, column) = match pc_to_loc.get(&pc) {
            Some(&(f, l, c)) => (f, l, c),
            None => continue, // No source mapping; skip.
        };

        // Silently absorb third-party snapshots: advance prev_pc so the
        // next user-source landing sees a continuous flow, but emit no Step / Value / Call
        // events for them.  See the loop preamble for rationale.
        if is_third_party_for_user(
            &paths.source_path_str,
            file_str,
            paths.user_crate_root.as_deref(),
        ) {
            prev_pc = Some(pc);
            continue;
        }

        // Detect function call/return from large PC jumps.  When the
        // source model resolves a function name for both the previous
        // and current line, we use its function-boundary view to
        // suppress within-function PC noise (e.g. an if-then-else
        // landing pad in `inner` registering a spurious call to itself).
        // When either side falls outside any known fn (e.g. callers
        // passing synthetic source paths with no real source file), we
        // fall back to the legacy "any forward/backward jump > 2"
        // heuristic so existing test_comprehensive scenarios stay green.
        if let Some(prev) = prev_pc {
            let pc_diff = if pc > prev { pc - prev } else { prev - pc };
            if pc_diff > 2 {
                let prev_fn = prev_line.and_then(|l| model.function_at(l));
                let curr_fn = model.function_at(line);
                let cross_boundary = match (prev_fn, curr_fn) {
                    (Some(a), Some(b)) => a != b,
                    _ => true, // unknown side → preserve legacy heuristic
                };
                if cross_boundary {
                    let push = pc > prev;
                    if push {
                        let callee_name = curr_fn
                            .map(str::to_string)
                            .unwrap_or_else(|| format!("fn_at_pc_{pc}"));
                        let callee_path = paths.ensure(writer, file_str);
                        let callee_fn_id = TraceWriter::ensure_function_id(
                            writer,
                            &callee_name,
                            &callee_path,
                            Line(line as i64),
                        );
                        TraceWriter::register_call(writer, callee_fn_id, vec![]);
                        fn_stack.push(callee_name);
                    } else {
                        // Backward cross-boundary jump: unwind the
                        // call stack until we're back in `curr_fn`.
                        // When `curr_fn` is unknown, pop a single
                        // frame to match the legacy heuristic.
                        //
                        // The synthesised return value comes from the
                        // last visited line of the unwinding frame —
                        // when that line is `return Err(..)` /
                        // `return Ok(..)`, the recorder surfaces the
                        // typed `Result`-shaped Variant instead of the
                        // legacy `NONE_VALUE` placeholder.  When the
                        // unwinding fn's last line carries a `?`
                        // operator, the recorder propagates the most
                        // recently synthesised `Err`-shaped value
                        // (mirrors Rust's `?` semantics: re-emit, NOT
                        // chain).
                        let return_value = prev_line
                            .and_then(|l| {
                                synthesise_return_value_with_propagation(
                                    &model,
                                    l,
                                    &mut type_ids,
                                    writer,
                                    last_err_return.as_ref(),
                                )
                            })
                            .unwrap_or(NONE_VALUE);
                        // Update `last_err_return` so the caller's
                        // closing `?`-bearing line can re-emit it.
                        if is_err_variant(&return_value) {
                            last_err_return = Some(return_value.clone());
                        } else if !matches!(return_value, ValueRecord::None { .. }) {
                            // A non-error typed return (e.g. `Ok(..)`)
                            // wipes the propagation slot so a later
                            // `?` doesn't accidentally pick it up.
                            last_err_return = None;
                        }
                        match curr_fn {
                            Some(target) => {
                                while fn_stack.len() > 1
                                    && fn_stack.last().map(String::as_str) != Some(target)
                                {
                                    TraceWriter::register_return(writer, return_value.clone());
                                    fn_stack.pop();
                                }
                            }
                            None => {
                                if fn_stack.len() > 1 {
                                    TraceWriter::register_return(writer, return_value);
                                    fn_stack.pop();
                                } else {
                                    TraceWriter::register_return(writer, return_value);
                                }
                            }
                        }
                    }
                }
            }
        }
        // Emit step on a fresh (line, column) — line changes always
        // fire, and same-line column changes fire too so column-aware
        // navigation can land on each statement of a multi-statement
        // line.
        let line_changed = prev_line != Some(line);
        let column_changed = column.is_some() && prev_column != column;
        if line_changed || column_changed {
            // One path string per file, the anchor's for the anchor: see
            // `canonical_step_path`.
            let step_path = paths.ensure(writer, file_str);
            // Offer the column only for a file the writer can address one in.
            let column = if paths.has_table(&step_path) {
                column
            } else {
                None
            };
            TraceWriter::register_step_with_column(
                writer,
                &step_path,
                Line(line as i64),
                column.map(|c| Line(c as i64)),
            );

            // Synthesise side-effecting / typed-value events the SBF
            // synthetic-snapshot pipeline cannot observe.  Done before
            // the per-register Int emission so the synthesised
            // structured variable appears alongside its formal name in
            // the step's variable list (rather than after the r0..r10
            // block).
            synthesise_step_events(&model, writer, line);

            prev_line = Some(line);
            prev_column = column;
        }

        // Emit register values as variables (r0 through r10).
        for r in 0..=10 {
            let name = format!("r{r}");
            let value = ValueRecord::Int {
                i: snap.reg(r) as i64,
                type_id: type_ids.int,
            };
            TraceWriter::register_variable_with_full_value(writer, &name, value);
        }

        prev_pc = Some(pc);
    }

    // Emit return for the main function.  When the last visited line
    // is itself a `return Err(..)` / `Err(..)` / `?`-bearing
    // expression, the synthesiser surfaces the typed Variant return so
    // a stepping debugger sees the same shape an in-VM run would emit
    // (e.g. `?`-propagated `Err(..)` re-emerging from the outer fn).
    let final_return = prev_line
        .and_then(|l| {
            synthesise_return_value_with_propagation(
                &model,
                l,
                &mut type_ids,
                writer,
                last_err_return.as_ref(),
            )
        })
        .unwrap_or(NONE_VALUE);
    TraceWriter::register_return(writer, final_return);

    Ok(())
}

/// Record a Solana program execution with CPI (Cross-Program Invocation)
/// awareness, producing CodeTracer trace output with nested Call/Return
/// events at CPI boundaries.
///
/// # Arguments
///
/// * `snapshots`         - Parsed register snapshots (may span multiple programs)
/// * `registry`          - Program registry mapping PCs to programs and source info
/// * `cpi_detector`      - CPI detector initialised with the primary program's range
/// * `source_path`       - Path to display in the trace for the primary program
/// * `out_dir`           - Directory where trace files will be written
///
/// The output format is fixed to the canonical CodeTracer CTFS multi-stream
/// container.
pub fn record_with_cpi(
    snapshots: &[RegisterSnapshot],
    registry: &ProgramRegistry,
    cpi_detector: &mut CpiDetector,
    source_path: &Path,
    out_dir: &Path,
) -> Result<()> {
    // Create output directory.
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("cannot create output dir: {}", out_dir.display()))?;

    // Create the trace writer (CTFS only).
    let program_name = source_path.to_string_lossy();
    let mut writer = create_trace_writer(&program_name, &[], CTFS_FORMAT);

    // Set up output files.  CTFS-only writer — events stream lives in
    // `trace.bin`.
    let events_path = out_dir.join("trace.bin");

    TraceWriter::begin_writing_trace_events(&mut *writer, &events_path)
        .map_err(|e| eyre!("{e}"))?;

    // Opt the writer into column-aware step encoding.  Mirrors the
    // non-CPI path: when DWARF carries column info the recorder layers
    // `DeltaColumn` events on each step; otherwise the writer falls
    // back to the column-less path.  Either way `meta.dat` bit 4
    // (`FlagHasColumnAwareSteps`) is set, which is the contract
    // column-aware readers rely on.
    TraceWriter::enable_column_aware_steps(&mut *writer);

    // M-capability-flags: see the sibling non-CPI path above for the
    // same rationale.  Solana's PC→source maps support per-column
    // breakpoint placement and per-column step motions.
    TraceWriter::enable_column_breakpoints_support(&mut *writer);
    TraceWriter::enable_column_motions_support(&mut *writer);

    // Load the primary program's source so we can resolve nested call
    // frames to their real `fn name(...)` declarations and synthesise
    // the spec-mandated syscall / typed-value events the SBF
    // synthetic-snapshot pipeline can't observe.  Mirrors the
    // `record_from_snapshots_into_writer` path; missing files (legacy
    // tests pass synthetic `primary.rs` strings) degrade to an empty
    // model and we fall back to the registry-derived names.
    let model = SourceModel::load(source_path);

    // Register the anchor with its per-line table before `start` names it.
    // Every other file is registered by `SourcePaths::ensure` at its first
    // mention -- the call into it, or the step on it -- under the one path
    // string every record uses for it.
    let mut paths = SourcePaths::new(source_path);
    paths.ensure(&mut *writer, &source_path.to_string_lossy());

    // Start the trace.
    TraceWriter::start(&mut *writer, source_path, Line(1));

    // Register the type ids the synthesiser needs up front (matches
    // the non-CPI path's deterministic-type-table behaviour).
    let mut type_ids = TypeIdCache::new(&mut *writer);

    // Pre-populate per-struct / per-variant TypeIds for every literal
    // construction the synthesiser might encounter on a step.
    for line_no in 1..(model.lines.len() as u32) {
        let raw = model.line(line_no);
        let text = strip_line_for_match(raw);
        if let (Some(_), Some(rhs)) = (extract_let_lhs(text), extract_let_rhs(text)) {
            if let Some((enum_path, _v, _p)) = parse_variant_construction(rhs) {
                type_ids.ensure_variant(&mut *writer, &enum_path);
                continue;
            }
            let candidate = if rhs.contains('{') && !rhs.contains('}') {
                collect_struct_literal_lines(&model, line_no)
            } else {
                rhs.to_string()
            };
            let candidate_rhs = if let Some(eq) = candidate.find('=') {
                candidate[eq + 1..]
                    .trim()
                    .trim_end_matches(';')
                    .trim()
                    .to_string()
            } else {
                candidate
            };
            if let Some((struct_name, _)) = parse_struct_literal(&candidate_rhs) {
                type_ids.ensure_struct(&mut *writer, &struct_name);
            }
        }
    }

    // Resolve the outer call frame's name from the source — same logic
    // as the non-CPI path.  When the fixture source isn't on disk
    // (legacy unit tests), fall back to "main" so existing CPI tests
    // (test_cpi.rs / test_cpi_execution.rs) keep observing their
    // present-day call shape.
    let outer_fn_name: String = snapshots
        .iter()
        .find_map(|snap| {
            let (_file, line) = registry.find_location(snap.pc())?;
            model.function_at(line).map(str::to_string)
        })
        .unwrap_or_else(|| "main".to_string());

    let main_fn_id =
        TraceWriter::ensure_function_id(&mut *writer, &outer_fn_name, source_path, Line(1));

    // Emit initial call.
    TraceWriter::register_call(&mut *writer, main_fn_id, vec![]);

    // Per-frame call-site `fn name` so within-program forward jumps
    // emit a named call frame instead of `fn_at_pc_<pc>`.
    let mut fn_stack: Vec<String> = vec![outer_fn_name.clone()];

    // Walk snapshots with CPI detection.
    let mut prev_line: Option<u32> = None;
    // Mirrors the non-CPI path: track the previously emitted column so
    // multi-statement lines surface distinct column-aware steps.
    let mut prev_column: Option<u32> = None;
    let mut prev_pc: Option<u64> = None;

    for snap in snapshots {
        let pc = snap.pc();

        // Check for CPI boundaries.
        let cpi_event = cpi_detector.process_snapshot(snap);
        match cpi_event {
            CpiEvent::CpiCall { target_pc } => {
                let program_name_str = registry
                    .program_name(target_pc)
                    .unwrap_or("unknown_program");
                let (file_str, line) = registry
                    .find_location(target_pc)
                    .unwrap_or_else(|| (format!("{program_name_str}.sbf"), 0));
                // Prefer the source-resolved fn name (e.g. `invoke_signed`)
                // over the registry's program name when the SourceModel
                // covers the target PC's line.  Real CPI shows up in the
                // calltrace as the called function, not the program id.
                let callee_name = model
                    .function_at(line)
                    .map(str::to_string)
                    .unwrap_or_else(|| program_name_str.to_string());
                let callee_path = paths.ensure(&mut *writer, &file_str);
                let cpi_fn_id = TraceWriter::ensure_function_id(
                    &mut *writer,
                    &callee_name,
                    &callee_path,
                    Line(line as i64),
                );
                // Stage CPI-target metadata as call args so the calltrace
                // pane displays which program was invoked and at what PC.
                // Mirrors the `register_call_arg` / `arg` pattern from the
                // Ruby (1.22) and JS (1.38) recorders — see section 5.6 of
                // /tmp/isonim-migration.txt.
                let pc_type_id = TraceWriter::ensure_type_id(&mut *writer, TypeKind::Int, "u64");
                let str_type_id =
                    TraceWriter::ensure_type_id(&mut *writer, TypeKind::String, "string");
                let _ = TraceWriter::arg(
                    &mut *writer,
                    "target_program",
                    ValueRecord::String {
                        text: program_name_str.to_string(),
                        type_id: str_type_id,
                    },
                );
                let _ = TraceWriter::arg(
                    &mut *writer,
                    "target_pc",
                    ValueRecord::Int {
                        i: target_pc as i64,
                        type_id: pc_type_id,
                    },
                );
                TraceWriter::register_call(&mut *writer, cpi_fn_id, vec![]);
                fn_stack.push(callee_name);
                // Reset line tracking for the new program context.
                prev_line = None;
                prev_column = None;
            }
            CpiEvent::CpiReturn { return_pc: _ } => {
                TraceWriter::register_return(&mut *writer, NONE_VALUE);
                if fn_stack.len() > 1 {
                    fn_stack.pop();
                }
                // Reset line tracking for the returned-to context.
                prev_line = None;
                prev_column = None;
            }
            CpiEvent::SameProgram => {
                // Within the same program, detect cross-fn jumps using
                // the SourceModel-derived function boundaries (matches
                // the non-CPI path's call-resolution heuristic).  When
                // the model can't resolve either side, fall back to the
                // legacy `fn_at_pc_<pc>` placeholder.
                if let Some(prev) = prev_pc {
                    let diff = if pc > prev { pc - prev } else { prev - pc };
                    if diff > 2 {
                        let current_program = cpi_detector.current_program();
                        let (file_str, line) = registry
                            .find_location(pc)
                            .unwrap_or_else(|| (format!("{current_program}.sbf"), 0));
                        let prev_fn = prev_line.and_then(|l| model.function_at(l));
                        let curr_fn = model.function_at(line);
                        let cross_boundary = match (prev_fn, curr_fn) {
                            (Some(a), Some(b)) => a != b,
                            _ => true,
                        };
                        if cross_boundary {
                            if pc > prev {
                                let callee_name = curr_fn
                                    .map(str::to_string)
                                    .unwrap_or_else(|| format!("fn_at_pc_{pc}"));
                                let callee_path = paths.ensure(&mut *writer, &file_str);
                                let callee_fn_id = TraceWriter::ensure_function_id(
                                    &mut *writer,
                                    &callee_name,
                                    &callee_path,
                                    Line(line as i64),
                                );
                                TraceWriter::register_call(&mut *writer, callee_fn_id, vec![]);
                                fn_stack.push(callee_name);
                            } else {
                                // Backward cross-boundary jump — unwind
                                // the within-program frame stack until
                                // we're back in `curr_fn`.
                                match curr_fn {
                                    Some(target) => {
                                        while fn_stack.len() > 1
                                            && fn_stack.last().map(String::as_str) != Some(target)
                                        {
                                            TraceWriter::register_return(&mut *writer, NONE_VALUE);
                                            fn_stack.pop();
                                        }
                                    }
                                    None => {
                                        if fn_stack.len() > 1 {
                                            TraceWriter::register_return(&mut *writer, NONE_VALUE);
                                            fn_stack.pop();
                                        } else {
                                            TraceWriter::register_return(&mut *writer, NONE_VALUE);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // Look up source location for this PC, including DWARF column
        // when available — `find_location_with_column` returns `None`
        // for the column on synthetic-program entries.
        let (file_str, line, column) = match registry.find_location_with_column(pc) {
            Some((f, l, c)) => (f, l, c),
            None => {
                // No source mapping; emit opcode-based line number.
                let program_name_str = cpi_detector.current_program();
                (format!("{program_name_str}.sbf"), pc as u32, None)
            }
        };

        let step_path = paths.ensure(&mut *writer, &file_str);
        // Offer the column only for a file the writer can address one in.
        let column = if paths.has_table(&step_path) {
            column
        } else {
            None
        };

        // Emit step on a fresh (line, column) — same contract as the
        // non-CPI path.  Line changes always fire; same-line column
        // changes fire too so multi-statement lines surface distinct
        // column-aware steps.
        let line_changed = prev_line != Some(line);
        let column_changed = column.is_some() && prev_column != column;
        if line_changed || column_changed {
            TraceWriter::register_step_with_column(
                &mut *writer,
                &step_path,
                Line(line as i64),
                column.map(|c| Line(c as i64)),
            );

            // Synthesise side-effecting / typed-value events.
            synthesise_step_events(&model, &mut *writer, line);

            prev_line = Some(line);
            prev_column = column;
        }

        // Emit register values as variables (r0 through r10).
        for r in 0..=10 {
            let name = format!("r{r}");
            let value = ValueRecord::Int {
                i: snap.reg(r) as i64,
                type_id: type_ids.int,
            };
            TraceWriter::register_variable_with_full_value(&mut *writer, &name, value);
        }

        prev_pc = Some(pc);
    }

    // Emit returns for any remaining CPI contexts (handles traces that
    // end while still inside nested CPIs).
    for _ in 0..cpi_detector.call_depth() {
        TraceWriter::register_return(&mut *writer, NONE_VALUE);
    }
    // Emit returns for any source-model-tracked within-program frames
    // that didn't unwind via a backward jump.
    while fn_stack.len() > 1 {
        TraceWriter::register_return(&mut *writer, NONE_VALUE);
        fn_stack.pop();
    }

    // Emit return for the main function.
    TraceWriter::register_return(&mut *writer, NONE_VALUE);

    // Finish writing.
    TraceWriter::finish_writing_trace_events(&mut *writer).map_err(|e| eyre!("{e}"))?;
    writer
        .write_meta_dat("codetracer-solana-recorder")
        .map_err(|e| eyre!("{e}"))?;
    writer.close().map_err(|e| eyre!("{e}"))?;

    Ok(())
}

#[cfg(test)]
mod source_path_tests {
    use super::is_third_party_source;

    #[test]
    fn cargo_registry_is_third_party() {
        assert!(is_third_party_source(
            "/home/user/.cargo/registry/src/index.crates.io-xxx/solana-program-entrypoint-2.3.0/src/lib.rs"
        ));
    }

    #[test]
    fn platform_tools_rust_library_is_third_party() {
        assert!(is_third_party_source(
            "/home/runner/work/platform-tools/platform-tools/out/rust/library/core/src/cmp.rs"
        ));
    }

    #[test]
    fn rustlib_src_is_third_party() {
        // ``/.../rustlib/src/rust/library/...`` is the rustup-installed
        // sysroot layout; treat it like platform-tools' bundled stdlib
        // so it never wins source-path selection.
        assert!(is_third_party_source(
            "/home/user/.rustup/toolchains/x/lib/rustlib/src/rust/library/core/src/option.rs"
        ));
    }

    #[test]
    fn user_crate_source_is_not_third_party() {
        assert!(!is_third_party_source(
            "/home/user/codetracer-solana-recorder/test-programs/src/solana_flow_test.rs"
        ));
    }

    #[test]
    fn bare_lib_rs_is_third_party_after_remap() {
        // ``cargo-build-sbf`` on CI strips ``$CARGO_HOME/registry/...``
        // off DWARF paths via ``--remap-path-prefix`` so registry deps
        // appear as bare relative paths -- match the convention that
        // user-named lib sources don't keep cargo's default ``lib.rs``
        // basename.
        assert!(is_third_party_source("src/lib.rs"));
    }

    #[test]
    fn relative_user_crate_source_is_not_third_party() {
        // The user's lib was renamed away from ``lib.rs`` precisely
        // for this reason -- a relative ``src/solana_flow_test.rs``
        // (post-remap) is still recognisable as the user's source.
        assert!(!is_third_party_source("src/solana_flow_test.rs"));
    }

    #[test]
    fn relative_dwarf_path_resolves_against_elf_crate_root() {
        use super::resolve_source_against_elf_crate;
        use std::path::{Path, PathBuf};

        // Lay out a synthetic SBF crate:
        //   <tmp>/test-programs/Cargo.toml
        //   <tmp>/test-programs/src/solana_flow_test.rs
        //   <tmp>/test-programs/target/sbpf-solana-solana/release/test_programs.so
        let tmp = std::env::temp_dir().join(format!(
            "ct-solana-resolve-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let crate_root = tmp.join("test-programs");
        let src_dir = crate_root.join("src");
        let elf_dir = crate_root.join("target/sbpf-solana-solana/release");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::create_dir_all(&elf_dir).unwrap();
        std::fs::write(crate_root.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        std::fs::write(src_dir.join("solana_flow_test.rs"), "fn main() {}\n").unwrap();
        let elf = elf_dir.join("test_programs.so");
        std::fs::write(&elf, b"").unwrap();

        let resolved = resolve_source_against_elf_crate(Path::new("src/solana_flow_test.rs"), &elf);
        assert_eq!(
            resolved,
            crate_root.join("src/solana_flow_test.rs"),
            "relative DWARF path should resolve against the ELF's Cargo crate root"
        );

        // Unknown relative paths fall through to the input so the caller's
        // empty-model fallback still kicks in.
        let unresolved = resolve_source_against_elf_crate(Path::new("src/does_not_exist.rs"), &elf);
        assert_eq!(unresolved, PathBuf::from("src/does_not_exist.rs"));

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn source_model_loads_via_resolver_against_relative_dwarf_path() {
        // End-to-end regression test for the cross-repo WDIO smoke failure
        // at run 27532963247: on CI, ``cargo-build-sbf`` produces DWARF
        // paths relative to the crate root (via ``--remap-path-prefix``).
        // The recorder must resolve those against the ELF's crate root
        // before ``SourceModel::load`` reads the file -- otherwise
        // ``read_to_string`` silently fails (cwd doesn't contain
        // ``src/<file>``), the model is empty, and every nested call
        // gets the ``fn_at_pc_<pc>`` synthetic placeholder instead of
        // the real function name from the source.
        use super::{SourceModel, resolve_source_against_elf_crate};
        use std::path::Path;

        let tmp = std::env::temp_dir().join(format!(
            "ct-solana-model-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let crate_root = tmp.join("test-programs");
        let src_dir = crate_root.join("src");
        let elf_dir = crate_root.join("target/sbpf-solana-solana/release");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::create_dir_all(&elf_dir).unwrap();
        std::fs::write(crate_root.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        std::fs::write(
            src_dir.join("solana_flow_test.rs"),
            "fn process_instruction(_a: u64) -> u64 {\n    let x = 1;\n    x\n}\n",
        )
        .unwrap();
        let elf = elf_dir.join("test_programs.so");
        std::fs::write(&elf, b"").unwrap();

        // Mimic the recorder's CI input: source_locations carries a
        // relative path (the cwd would be the recorder repo root,
        // where ``src/solana_flow_test.rs`` does NOT exist).
        let dwarf_relative = Path::new("src/solana_flow_test.rs");

        // Without the resolver, SourceModel::load would return an empty
        // model because the relative path doesn't resolve against cwd.
        let unresolved_model = SourceModel::load(dwarf_relative);
        assert!(
            unresolved_model.function_at(1).is_none(),
            "unresolved model must be empty; otherwise this test isn't reproducing the CI scenario"
        );

        // With the resolver, the model loads the source and surfaces
        // the real function name -- restoring the call-frame data the
        // WDIO smoke test polls for.
        let resolved = resolve_source_against_elf_crate(dwarf_relative, &elf);
        let model = SourceModel::load(&resolved);
        assert_eq!(
            model.function_at(2),
            Some("process_instruction"),
            "SourceModel should resolve `process_instruction` from the source on line 2 \
             after the resolver walks up from the ELF to find the crate root"
        );

        std::fs::remove_dir_all(&tmp).ok();
    }
}
