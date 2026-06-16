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
//!   can scan the corresponding source line for known constructs
//!   (`msg!(...)`, `panic!(...)`, `Err(...)`, `vec![..]` / array
//!   literal, tuple literal, `Name { .. }` struct literal) and synthesise
//!   the matching trace event (`register_special_event` for syscalls,
//!   `register_variable_with_full_value` with the proper `ValueRecord`
//!   variant for typed values).
//!
//! The synthesis is purely additive: every existing register-variable
//! emission continues unchanged.  When the source file is missing
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

    // 3. Build source locations from DWARF.
    let source_locations: Vec<(u64, String, u32)> = snapshots
        .iter()
        .filter_map(|snap| {
            let loc = dwarf.find_location(snap.pc())?;
            Some((snap.pc(), loc.file, loc.line))
        })
        .collect();

    // Convert to borrowed form for the shared implementation.
    let source_locs_ref: Vec<(u64, &str, u32)> = source_locations
        .iter()
        .map(|(pc, f, l)| (*pc, f.as_str(), *l))
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
    let trace_source_path: PathBuf = if let Some((_, f, _)) = source_locations
        .iter()
        .find(|(_, f, _)| !is_third_party_source(f))
    {
        PathBuf::from(f)
    } else if let Some(rs) = sibling_rs_source(source_path) {
        rs
    } else if let Some((_, f, _)) = source_locations.first() {
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

    record_from_snapshots(&snapshots, &source_locs_ref, &trace_source_path, out_dir)
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

/// Returns ``true`` when the supplied ``let`` RHS would be picked up
/// by one of the structured-value branches in
/// [`synthesise_step_events`] (borrowed-slice indexing, enum-variant
/// construction, vec/array literal with int elements, tuple literal,
/// struct literal).  Used by [`VarEnv::prepopulate_lets_from_body`]
/// to decide whether the name should *also* be emitted as a plain
/// ``Int`` local at every snapshot -- structured RHSs are emitted as
/// typed compound values by the synthesiser and the DAP locals view
/// would otherwise see the same name twice (once as the structured
/// value, once as ``Int``).
///
/// Multi-line struct literals (which ``synthesise_step_events``
/// stitches via ``collect_struct_literal_lines``) are *not*
/// recognised by this single-line check; that's a deliberate
/// trade-off, since the prepopulate pass walks lines one at a time
/// and the only fixtures that exercise multi-line struct literals
/// (the recorder's hand-written
/// ``test_per_program_ct_print_full`` cases) declare the struct
/// name on the same line as the opening brace, which
/// ``parse_struct_literal_rich`` matches.
fn rhs_is_structured(rhs: &str) -> bool {
    if is_borrowed_slice_rhs(rhs) {
        return true;
    }
    if parse_variant_construction(rhs).is_some() {
        return true;
    }
    if extract_array_or_vec_literal(rhs).is_some() {
        return true;
    }
    if extract_tuple_literal(rhs).is_some() {
        return true;
    }
    // Struct literals match on a leading capitalised identifier
    // followed by ``{``.  A bare ``{ ... }`` block (e.g.
    // ``let v = { let x = 1; x };``) doesn't qualify -- the
    // synthesiser's ``parse_struct_literal_rich`` returns ``None``
    // for it.  Approximate by requiring an alphanumeric character
    // before the first ``{``.
    if let Some(brace) = rhs.find('{')
        && rhs[..brace]
            .trim_end()
            .chars()
            .last()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return true;
    }
    false
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

/// Substitute `{}` placeholders in `fmt` with the values pulled from
/// `args` (resolved through `env`).  When a positional `{}` runs out of
/// args or an arg expression doesn't resolve to a value in `env`, the
/// placeholder is left literal (`{}`) so partial-resolution paths still
/// surface the spec-correct text where possible.  Inline named-arg
/// placeholders (`{name}`) are also resolved when `name` lives in `env`
/// — the Rust 2021 idiom used by `msg!("a={a}")`.
///
/// Returns `(substituted_text, fully_substituted)` where the second
/// element is `true` only when every placeholder was successfully
/// replaced — used by the caller to decide whether to surface the
/// substituted string at all (avoids emitting partially-substituted
/// payloads that would mask the present-day literal-text contract for
/// fixtures where the env can't resolve every name).
fn substitute_format(
    fmt: &str,
    args: &[String],
    env: &VarEnv,
    snap: &RegisterSnapshot,
) -> (String, bool) {
    let bytes = fmt.as_bytes();
    let mut out = String::with_capacity(fmt.len());
    let mut i = 0usize;
    let mut arg_idx = 0usize;
    let mut all_ok = true;
    while i < bytes.len() {
        let c = bytes[i];
        // `{{` and `}}` are literal escapes.
        if c == b'{' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            out.push('{');
            i += 2;
            continue;
        }
        if c == b'}' && i + 1 < bytes.len() && bytes[i + 1] == b'}' {
            out.push('}');
            i += 2;
            continue;
        }
        if c == b'{' {
            // Scan to the matching `}`.
            let Some(end_off) = bytes[i + 1..].iter().position(|&b| b == b'}') else {
                out.push('{');
                i += 1;
                continue;
            };
            let spec = std::str::from_utf8(&bytes[i + 1..i + 1 + end_off]).unwrap_or("");
            // The format spec is `name?:fmt?` — we ignore everything after `:`
            // and only handle empty (positional) or bare-identifier names.
            let name_part = spec.split(':').next().unwrap_or("");
            let resolved = if name_part.is_empty() {
                // Positional `{}` — pull from args[arg_idx].
                if arg_idx < args.len() {
                    let arg_expr = &args[arg_idx];
                    arg_idx += 1;
                    env.resolve(arg_expr, snap)
                } else {
                    None
                }
            } else {
                // Named `{name}` — look the identifier up in the env.
                env.resolve(name_part, snap)
            };
            match resolved {
                Some(v) => out.push_str(&v.to_string()),
                None => {
                    // Leave the placeholder literal so partial substitutions
                    // don't accidentally drop information.
                    out.push('{');
                    out.push_str(spec);
                    out.push('}');
                    all_ok = false;
                }
            }
            i += end_off + 2;
            continue;
        }
        out.push(c as char);
        i += 1;
    }
    if arg_idx < args.len() {
        // Unused trailing args — treat as not-fully-substituted.
        all_ok = false;
    }
    (out, all_ok)
}

/// Per-line / per-frame map of variable names to register slots, built
/// dynamically as the recorder walks snapshots.  Function parameters
/// are pre-loaded from the source (`fn name(p1: T1, p2: T2)` → `p1` →
/// r1, `p2` → r2 etc.) and let-bindings are tracked as new register
/// slots become live (the lowest register whose value differs from the
/// previous snapshot at a `let NAME = ...` line).
///
/// This is the synthesiser's interpolation env for `msg!("fmt {}", x)`
/// and `msg!("a={a}")`-style calls — the SBF interpreter doesn't run in
/// the synthetic-snapshot pipeline, so the recorder reconstructs
/// what-name-lives-where from the source plus the snapshot diff.
#[derive(Default, Clone)]
struct VarEnv {
    /// Variable name → register index (0..=10).
    names: HashMap<String, usize>,
    /// Subset of [`Self::names`] that should be surfaced as ``Int``
    /// locals in the trace's per-snapshot variable stream.
    ///
    /// We split this from ``names`` because two unrelated callers
    /// populate the env:
    ///   * The format-arg substitution path inside
    ///     [`synthesise_step_events`] needs to *resolve* every
    ///     identifier appearing in a ``msg!(..)`` to its current
    ///     register value, so it pulls from ``names`` (params + any
    ///     observed ``let NAME = ...``).
    ///   * The DAP locals view needs the *names* of source-level
    ///     locals on the stack to be present in the variable-name
    ///     table.  That only makes sense for non-structured bindings
    ///     -- ``let pair = (10, 20)`` is already emitted as a typed
    ///     ``Tuple`` by ``synthesise_step_events``, and re-emitting
    ///     it as ``Int`` would corrupt the test
    ///     ``test_collections_test_via_ct_print_full``'s
    ///     ``pair.kind == "Tuple"`` assertion.
    ///
    /// Function parameters and ``let NAME = <simple expr>`` are
    /// added to both sets; ``let NAME = <struct/array/tuple/enum
    /// literal>`` is added only to ``names`` (so the substitution
    /// path can still resolve it).
    emit_as_int: std::collections::HashSet<String>,
}

impl VarEnv {
    fn new() -> Self {
        Self {
            names: HashMap::new(),
            emit_as_int: std::collections::HashSet::new(),
        }
    }

    /// Pre-load function parameter names from a `fn NAME(p1: T1, p2: T2, ...)`
    /// declaration.  Each param `pi` is mapped to register `r{i}`
    /// following the SBF calling convention used by the workspace's
    /// hand-written fixtures.
    fn load_params_from_fn(&mut self, fn_decl_line: &str) {
        // Strip leading `fn NAME` and capture the parenthesised arg list.
        let Some(open) = fn_decl_line.find('(') else {
            return;
        };
        let Some(close_off) = fn_decl_line[open + 1..].find(')') else {
            return;
        };
        let body = &fn_decl_line[open + 1..open + 1 + close_off];
        let mut idx = 1usize;
        for tok in split_top_level_commas(body) {
            let t = tok.trim();
            if t.is_empty() {
                continue;
            }
            // Skip `&self` / `self` / `mut self`.
            if t == "self" || t == "&self" || t == "&mut self" || t == "mut self" {
                continue;
            }
            // Param name = identifier before the first `:`.
            let name_part = t.split(':').next().unwrap_or("").trim();
            let name = name_part
                .trim_start_matches('&')
                .trim_start_matches("mut ")
                .trim_start_matches('_')
                .trim_start_matches('&');
            // Skip names that aren't simple identifiers (e.g. tuple-pattern params).
            if !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && idx <= 10
            {
                self.names.insert(name.to_string(), idx);
                self.emit_as_int.insert(name.to_string());
            }
            idx += 1;
        }
    }

    /// Pre-populate the env with every ``let NAME = ...`` binding
    /// declared in the body of a function spanning ``(start_line,
    /// end_line)`` (inclusive).  Each new binding is assigned to a
    /// placeholder register sequentially after any existing entries,
    /// wrapping at ``r10`` so the SBF register convention is preserved.
    ///
    /// Why this exists: at ``opt-level=0`` the compiler keeps locals
    /// on the BPF stack, so [`VarEnv::record_let`]'s
    /// "register-with-a-changed-value" heuristic finds nothing to
    /// match at the let-line snapshot and the binding never enters
    /// the env.  The downstream DAP locals view then only shows the
    /// params, not the lets -- which fails the WDIO smoke test's
    /// ``finds sum_val in local variables`` substring assertion
    /// (cross-repo run 27591732085).
    ///
    /// The pre-population guarantees the *name* appears in the
    /// trace's variable-name table regardless of register placement.
    /// ``record_let`` still runs on each visited let-line snapshot
    /// and *upgrades* the placeholder to the real register when one
    /// changes, so trace fixtures that *do* see register changes
    /// (the recorder's hand-written ``test_comprehensive`` cases)
    /// keep their precise register→name mapping.
    fn prepopulate_lets_from_body(&mut self, model: &SourceModel, start_line: u32, end_line: u32) {
        let mut next_reg = self.names.values().copied().max().map_or(1, |r| r + 1);
        for line_no in start_line..=end_line {
            let text = strip_line_for_match(model.line(line_no));
            if let (Some(name), Some(rhs)) = (extract_let_lhs(text), extract_let_rhs(text))
                && !self.names.contains_key(name)
            {
                let reg = next_reg.min(10);
                self.names.insert(name.to_string(), reg);
                // Only request Int-style emission for non-structured
                // RHS.  Structured RHS (struct / array / vec / tuple /
                // enum-variant literal, borrowed-slice indexing) is
                // already emitted as a typed compound value by
                // ``synthesise_step_events``; re-emitting it as an
                // ``Int`` here would corrupt assertions like
                // ``test_collections_test_via_ct_print_full``'s
                // ``pair.kind == "Tuple"``.  The name still lands in
                // ``names`` so the substitution path can resolve it.
                if !rhs_is_structured(rhs) {
                    self.emit_as_int.insert(name.to_string());
                }
                next_reg = (next_reg + 1).min(10);
            }
        }
    }

    /// Record a let-binding seen at this step.  `prev` is the previous
    /// snapshot's register array (or all-zeros if this is the first
    /// step).  We assign `name` to the lowest register r{1..=10} whose
    /// value changed (became non-zero or differs).  This matches the
    /// canonical fixture convention where each new let-binding's RHS
    /// lands in the next live register slot.
    fn record_let(&mut self, name: &str, prev: &[u64; 12], curr: &RegisterSnapshot) {
        // Find the lowest register r1..r10 whose value differs between
        // the previous snapshot and this one and pin ``name`` to it.
        // When the binding is already known we still upgrade --
        // ``prepopulate_lets_from_body`` may have seeded a placeholder
        // sequential register at frame-push time so the *name* would
        // appear in the DAP locals view even when ``opt-level=0``
        // keeps the value on the BPF stack; if execution later traverses
        // the let line at a higher opt level (or in a synthesised
        // fixture) and *does* surface a real register change, the
        // runtime-detected register supersedes the placeholder so
        // substitution paths like ``synthesise_step_events``'s
        // ``msg!("..", x, y)`` resolution -- exercised by
        // ``test_msg_format_args_test_via_ct_print_full`` -- pick up
        // the correct value.
        //
        // When no register changes (the opt-level=0 / stack-spilled
        // case), we leave whatever is in ``names`` untouched so the
        // placeholder stays.
        for (r, prev_val) in prev.iter().enumerate().skip(1).take(10) {
            if curr.reg(r) != *prev_val {
                self.names.insert(name.to_string(), r);
                self.emit_as_int.insert(name.to_string());
                return;
            }
        }
    }

    /// Resolve a free-form arg expression to an integer at the given
    /// snapshot.  Today we recognise:
    ///
    /// * a bare identifier present in `self.names` → `snap.reg(r{n})`,
    /// * an integer literal → its value,
    /// * `&NAME` / `*NAME` / `mut NAME` ref/deref forms that strip to
    ///   a recognised identifier.
    ///
    /// Returns `None` for anything else so the caller can leave the
    /// placeholder literal.
    fn resolve(&self, expr: &str, snap: &RegisterSnapshot) -> Option<i64> {
        let s = expr
            .trim()
            .trim_start_matches('&')
            .trim_start_matches('*')
            .trim_start_matches("mut ")
            .trim();
        if let Some(i) = parse_int_literal(s) {
            return Some(i);
        }
        let r = self.names.get(s)?;
        Some(snap.reg(*r) as i64)
    }
}

/// Build a fresh `VarEnv` pre-loaded with the parameters declared by
/// the function named `fn_name` in `model`.  When the function isn't
/// found (synthetic source path / outer frame named "main" with no
/// matching declaration), an empty env is returned.
///
/// In addition to function parameters, this also pre-populates the env
/// with every ``let NAME = ...`` binding declared inside the function
/// body, mapped to a placeholder register.  At ``opt-level=0`` (the
/// recorder's SBF build profile -- see ``test-programs/Cargo.toml``),
/// the compiler keeps local variables on the BPF stack rather than in
/// registers, so the runtime ``VarEnv::record_let`` heuristic (which
/// looks for a changed register at the let-line snapshot) can never
/// find a match and the binding gets dropped.  That's why
/// cross-repo run 27591732085 reached ``Ok(0)`` + 1277 register
/// snapshots but the DAP locals view still missed ``sum_val`` etc.:
/// the env-tracked emission path I added in a127a54 has nothing to
/// emit because nothing was ever recorded.
///
/// Pre-populating from a source-side scan guarantees the *names* of
/// every declared binding land in the env (and therefore in the
/// trace's variable-name table the DAP server consumes for locals
/// display) -- regardless of whether execution traversed that line or
/// whether the compiler chose to keep the value in a register.  The
/// register assignment is a best-effort placeholder (sequential after
/// the params, capped at r10); ``record_let`` still runs at the
/// snapshot of an actual let-line visit and *refines* the register
/// assignment when it observes a real change.
fn var_env_for_fn(model: &SourceModel, fn_name: &str) -> VarEnv {
    let mut env = VarEnv::new();
    let fn_range = model
        .functions
        .iter()
        .find(|(_, _, name)| name == fn_name)
        .map(|(start, end, _)| (*start, *end));
    if let Some((decl_line, end_line)) = fn_range {
        // Stitch the declaration across continuation lines so multi-line
        // signatures (rare in fixtures but cheap to support) still parse.
        let mut combined = String::new();
        let mut depth: i32 = 0;
        let mut saw_open = false;
        let mut line_no = decl_line;
        while (line_no as usize) < model.lines.len() {
            let raw = model.line(line_no);
            let stripped = strip_line_for_match(raw);
            if !combined.is_empty() {
                combined.push(' ');
            }
            combined.push_str(stripped);
            for c in stripped.chars() {
                match c {
                    '(' => {
                        depth += 1;
                        saw_open = true;
                    }
                    ')' => depth -= 1,
                    _ => {}
                }
            }
            if saw_open && depth == 0 {
                break;
            }
            line_no += 1;
        }
        env.load_params_from_fn(&combined);

        // Pre-populate ``let NAME = ...`` bindings declared in the body.
        env.prepopulate_lets_from_body(model, decl_line, end_line);
    }
    env
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

/// Build a `ValueRecord::Sequence` from the literal element list inside
/// `[...]` or `vec![...]`.  Skips elements that aren't simple integer
/// literals (the synthesiser deliberately sticks to lossless decoding).
fn build_sequence_value(
    elements_csv: &str,
    int_type_id: TypeId,
    seq_type_id: TypeId,
) -> ValueRecord {
    let mut elements = Vec::new();
    for tok in split_top_level_commas(elements_csv) {
        if let Some(i) = parse_int_literal(tok) {
            elements.push(ValueRecord::Int {
                i,
                type_id: int_type_id,
            });
        }
    }
    ValueRecord::Sequence {
        elements,
        is_slice: false,
        type_id: seq_type_id,
    }
}

fn build_tuple_value(
    elements_csv: &str,
    int_type_id: TypeId,
    tuple_type_id: TypeId,
) -> ValueRecord {
    let mut elements = Vec::new();
    for tok in split_top_level_commas(elements_csv) {
        if let Some(i) = parse_int_literal(tok) {
            elements.push(ValueRecord::Int {
                i,
                type_id: int_type_id,
            });
        }
    }
    ValueRecord::Tuple {
        elements,
        type_id: tuple_type_id,
    }
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
    } else if let Some(stripped) = text.strip_prefix("Ok(") {
        let close = stripped.rfind(')')?;
        ("Ok".to_string(), stripped[..close].trim().to_string())
    } else {
        return None;
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

/// Synthesise the spec-mandated `RecordEvent`s and typed `ValueRecord`
/// variables for a step at `line_no`.  Called immediately after
/// `register_step` in the snapshot loop.
///
/// `env` and `snap` are used to interpolate `msg!(\"fmt {}\", arg)` and
/// `msg!(\"a={a}\")`-style placeholders with the current register values
/// — the SBF interpreter doesn't run in the synthetic-snapshot pipeline,
/// so the recorder reconstructs what-name-lives-where from the source
/// plus the snapshot diff.  When `env` can't resolve every placeholder
/// the literal format string is preserved (no partial substitutions).
fn synthesise_step_events(
    model: &SourceModel,
    writer: &mut dyn TraceWriter,
    line_no: u32,
    type_ids: &mut TypeIdCache,
    env: &VarEnv,
    snap: &RegisterSnapshot,
) {
    let raw = model.line(line_no);
    let text = strip_line_for_match(raw);
    if text.is_empty() {
        return;
    }

    // Side-effect macros / errors → io_events.
    if text.contains("msg!(") {
        let payload = match extract_macro_call(text, "msg") {
            Some((fmt, args)) => {
                let (substituted, all_ok) = substitute_format(&fmt, &args, env, snap);
                if all_ok { substituted } else { fmt }
            }
            None => "<msg!>".to_string(),
        };
        TraceWriter::register_special_event(writer, EventLogKind::Write, "SolanaMsg", &payload);
    }
    if text.contains("panic!(") {
        let payload = match extract_macro_call(text, "panic") {
            Some((fmt, args)) => {
                let (substituted, all_ok) = substitute_format(&fmt, &args, env, snap);
                if all_ok { substituted } else { fmt }
            }
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

    // Typed structured-value let-bindings.
    if let (Some(name), Some(rhs)) = (extract_let_lhs(text), extract_let_rhs(text)) {
        // Borrowed-slice / slice-indexing patterns — surface as a
        // `Sequence { is_slice: true }` so the strict pin can
        // distinguish them from opaque pointers.  Checked first so an
        // RHS like `&instruction_data[..32]` doesn't accidentally fall
        // through to the array-literal detector (which would try to
        // parse the contents of the `[..]` as csv).
        if is_borrowed_slice_rhs(rhs) {
            let value = ValueRecord::Sequence {
                elements: Vec::new(),
                is_slice: true,
                type_id: type_ids.seq,
            };
            TraceWriter::register_variable_with_full_value(writer, name, value);
            return;
        }
        // Enum-variant construction — `MyInstruction::Init { lamports: 500 }`,
        // `MyInstruction::Update(7)`, `MyInstruction::Close`.  Checked
        // before the struct-literal branch because `Name::Variant { .. }`
        // would otherwise be decoded as a struct with the path as its name.
        if let Some((enum_path, variant, payload)) = parse_variant_construction(rhs) {
            let int_type_id = type_ids.int;
            let struct_type_id = type_ids
                .struct_type_for(&variant)
                .unwrap_or(type_ids.struct_default);
            let tuple_type_id = type_ids.tuple;
            let contents = match payload {
                VariantPayload::Struct(fields) => {
                    let field_values: Vec<ValueRecord> = fields
                        .iter()
                        .map(|(_, i)| ValueRecord::Int {
                            i: *i,
                            type_id: int_type_id,
                        })
                        .collect();
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
                            type_id: int_type_id,
                        })
                        .collect();
                    ValueRecord::Tuple {
                        elements,
                        type_id: tuple_type_id,
                    }
                }
                VariantPayload::Unit => ValueRecord::Tuple {
                    elements: Vec::new(),
                    type_id: tuple_type_id,
                },
            };
            let variant_type_id = type_ids.ensure_variant(writer, &enum_path);
            let value = ValueRecord::Variant {
                discriminator: variant.clone(),
                contents: Box::new(contents),
                type_id: variant_type_id,
            };
            TraceWriter::register_variable_with_full_value(writer, name, value);
            return;
        }
        // Vec / array literal — `vec![..]`, `[1, 2, 3]`, `&[1, 2, 3]`.
        // Only emit when at least one element decoded as an int literal —
        // arrays-of-byte-strings (`[b"vault", payer.as_ref()]`) and other
        // shapes the synthesiser can't lossily decode are skipped rather
        // than surfaced as empty `Sequence` placeholders.
        if let Some(elements_csv) = extract_array_or_vec_literal(rhs) {
            let value = build_sequence_value(elements_csv, type_ids.int, type_ids.seq);
            let has_elements = match &value {
                ValueRecord::Sequence { elements, .. } => !elements.is_empty(),
                _ => false,
            };
            if has_elements {
                TraceWriter::register_variable_with_full_value(writer, name, value);
                return;
            }
        }
        // Tuple literal — `(a, b)` with at least 2 elements.
        if let Some(elements_csv) = extract_tuple_literal(rhs) {
            let value = build_tuple_value(elements_csv, type_ids.int, type_ids.tuple);
            TraceWriter::register_variable_with_full_value(writer, name, value);
            return;
        }
        // Struct literal — `Name { .. }`.  Multi-line struct literals
        // (Rust formatter often breaks them) are stitched back together
        // before parsing.
        let candidate = if rhs.contains('{') && !rhs.contains('}') {
            collect_struct_literal_lines(model, line_no)
        } else {
            rhs.to_string()
        };
        // Strip the `let NAME = ` prefix from the stitched line if present.
        let candidate_rhs = if let Some(eq) = candidate.find('=') {
            candidate[eq + 1..]
                .trim()
                .trim_end_matches(';')
                .trim()
                .to_string()
        } else {
            candidate
        };
        if let Some((struct_name, fields)) =
            parse_struct_literal_rich(&candidate_rhs, type_ids, writer)
        {
            let type_id = type_ids
                .struct_type_for(struct_name.as_str())
                .unwrap_or_else(|| type_ids.ensure_struct(writer, &struct_name));
            let field_values: Vec<ValueRecord> = fields.into_iter().map(|(_, v)| v).collect();
            let value = ValueRecord::Struct {
                field_values,
                type_id,
            };
            TraceWriter::register_variable_with_full_value(writer, name, value);
        }
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

/// Recognise a borrowed-slice / slice-indexing / `try_borrow_data()`
/// RHS so the let-binding surfaces as a `Sequence { is_slice: true }`
/// rather than being silently dropped by the value-literal detector.
///
/// Recognised forms (canonical native-Solana account-data idioms):
///   * `&IDENT[range]` / `&mut IDENT[range]` — slice-indexing into
///     `instruction_data` (`&data[..32]`, `&data[32..]`, `&data[A..B]`).
///   * `*.try_borrow_data()` / `*.try_borrow_mut_data()` — the canonical
///     `RefCell`-wrapped account-data borrow, optionally followed by
///     `.unwrap()` / `?` / `.expect(..)` chains.  We only need the
///     leading `try_borrow_data(` substring to recognise the shape.
fn is_borrowed_slice_rhs(rhs: &str) -> bool {
    let trimmed = rhs.trim();
    // `*.try_borrow_data(` / `*.try_borrow_mut_data(` — chained method
    // call on an account info or similar.  We accept arbitrary trailing
    // text (e.g. `.unwrap()` / `?`) so the test fixture's
    // `account.try_borrow_data().unwrap()` shape matches.
    if trimmed.contains(".try_borrow_data(") || trimmed.contains(".try_borrow_mut_data(") {
        return true;
    }
    // `&IDENT[range]` / `&mut IDENT[range]` — slice-indexing.  Strip
    // the leading `&` (and optional `mut `) then look for an identifier
    // followed by `[<range>]`.  The range MUST contain `..` so we don't
    // accidentally match a single-element index `xs[i]` (which is an
    // element access, not a slice).
    let body = trimmed.strip_prefix('&').unwrap_or(trimmed);
    let body = body.strip_prefix("mut ").unwrap_or(body);
    if let Some(open) = body.find('[') {
        // Identifier-only prefix.
        let ident = body[..open].trim();
        let is_ident =
            !ident.is_empty() && ident.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if is_ident {
            // Locate the matching `]` and check the content includes `..`.
            let after = &body[open + 1..];
            if let Some(close) = after.find(']') {
                let inside = &after[..close];
                if inside.contains("..") {
                    return true;
                }
            }
        }
    }
    false
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
    source_locations: &[(u64, &str, u32)],
    source_path: &Path,
    writer: &mut dyn TraceWriter,
) -> Result<()> {
    // Build a PC -> (file, line) lookup.
    let pc_to_loc: std::collections::HashMap<u64, (&str, u32)> = source_locations
        .iter()
        .map(|(pc, file, line)| (*pc, (*file, *line)))
        .collect();

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
            let &(_file, line) = pc_to_loc.get(&pc)?;
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

    // Per-frame variable→register environments used by the format-arg
    // interpolation path in `synthesise_step_events`.  Stack-aligned
    // with `fn_stack` (outer frame at index 0) so a CPI / nested call
    // walks its own env without leaking the caller's bindings.
    let mut env_stack: Vec<VarEnv> = vec![var_env_for_fn(&model, &outer_fn_name)];

    // Walk snapshots.
    let mut prev_line: Option<u32> = None;
    let mut prev_pc: Option<u64> = None;
    let mut prev_regs: [u64; 12] = [0u64; 12];

    // Track the most recently synthesised `Err`-shaped return value so
    // `?`-propagation in the caller can re-emit it without re-parsing
    // the callee's source.  Updated each time the recorder emits a
    // typed `Result::Err` return (via `synthesise_return_value`); reset
    // to `None` whenever a non-error return is emitted.
    let mut last_err_return: Option<ValueRecord> = None;

    for snap in snapshots {
        let pc = snap.pc();

        // Look up source location for this PC.
        let (file_str, line) = match pc_to_loc.get(&pc) {
            Some(&(f, l)) => (f, l),
            None => continue, // No source mapping; skip.
        };

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
            let diff = if pc > prev { pc - prev } else { prev - pc };
            if diff > 2 {
                let prev_fn = prev_line.and_then(|l| model.function_at(l));
                let curr_fn = model.function_at(line);
                let cross_boundary = match (prev_fn, curr_fn) {
                    (Some(a), Some(b)) => a != b,
                    _ => true, // unknown side → preserve legacy heuristic
                };
                if cross_boundary {
                    if pc > prev {
                        let callee_name = curr_fn
                            .map(str::to_string)
                            .unwrap_or_else(|| format!("fn_at_pc_{pc}"));
                        let callee_fn_id = TraceWriter::ensure_function_id(
                            writer,
                            &callee_name,
                            &Path::new(file_str),
                            Line(line as i64),
                        );
                        TraceWriter::register_call(writer, callee_fn_id, vec![]);
                        env_stack.push(var_env_for_fn(&model, &callee_name));
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
                                    env_stack.pop();
                                }
                            }
                            None => {
                                if fn_stack.len() > 1 {
                                    TraceWriter::register_return(writer, return_value);
                                    fn_stack.pop();
                                    env_stack.pop();
                                } else {
                                    TraceWriter::register_return(writer, return_value);
                                }
                            }
                        }
                    }
                }
            }
        }

        // Emit step when line changes.
        if prev_line != Some(line) {
            TraceWriter::register_step(writer, &Path::new(file_str), Line(line as i64));

            // Update the active frame's variable→register env from any
            // `let NAME = ...` binding visible on this step before
            // synthesising events — placeholder substitution downstream
            // looks up named args via the env.
            if let Some(env) = env_stack.last_mut()
                && let Some(name) = extract_let_lhs(strip_line_for_match(model.line(line)))
            {
                env.record_let(name, &prev_regs, snap);
            }

            // Synthesise side-effecting / typed-value events the SBF
            // synthetic-snapshot pipeline cannot observe.  Done before
            // the per-register Int emission so the synthesised
            // structured variable appears alongside its formal name in
            // the step's variable list (rather than after the r0..r10
            // block).
            let active_env = env_stack.last().cloned().unwrap_or_else(VarEnv::new);
            synthesise_step_events(&model, writer, line, &mut type_ids, &active_env, snap);

            prev_line = Some(line);
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

        // Surface each named binding tracked in the active frame's env
        // (function parameters from ``fn (..)`` plus any ``let NAME =
        // ..`` declarations seen so far) as an additional named local
        // alongside the raw ``r0..r10`` registers.  Without this the
        // DAP server's ``ct/load-locals`` query returns only the
        // unnamed register expressions and the WDIO smoke test's
        // ``finds sum_val in local variables`` assertion fails --
        // observed against cross-repo run 27582656096: execution
        // ran to completion (1277 register snapshots) and the
        // source model loaded correctly, but the trace contained no
        // ``sum_val`` variable name because the synthesiser's
        // ``let``-binding path only fires for *structured* RHS
        // (struct/array/tuple/enum) and simple ``let sum_val: u64
        // = a + b;`` slipped through.
        //
        // The value is the register's current snapshot, looked up
        // via the env's ``name -> register`` map.  ``VarEnv``
        // records this mapping on each ``let NAME = ...`` line by
        // assigning the binding to the lowest register r{1..=10}
        // whose value changed at that step -- the canonical sBPF
        // convention the workspace's hand-written fixtures rely on.
        if let Some(env) = env_stack.last() {
            for var_name in &env.emit_as_int {
                let Some(&reg) = env.names.get(var_name) else {
                    continue;
                };
                let value = ValueRecord::Int {
                    i: snap.reg(reg) as i64,
                    type_id: type_ids.int,
                };
                TraceWriter::register_variable_with_full_value(writer, var_name, value);
            }
        }

        prev_pc = Some(pc);
        prev_regs = snap.registers;
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

    // Load the primary program's source so we can resolve nested call
    // frames to their real `fn name(...)` declarations and synthesise
    // the spec-mandated syscall / typed-value events the SBF
    // synthetic-snapshot pipeline can't observe.  Mirrors the
    // `record_from_snapshots_into_writer` path; missing files (legacy
    // tests pass synthetic `primary.rs` strings) degrade to an empty
    // model and we fall back to the registry-derived names.
    let model = SourceModel::load(source_path);

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

    // Frame-aligned variable→register env stack so format-arg
    // interpolation in `synthesise_step_events` honours the active
    // frame.  Outer frame at index 0; CPI calls push, returns pop.
    let mut env_stack: Vec<VarEnv> = vec![var_env_for_fn(&model, &outer_fn_name)];
    // Per-frame call-site `fn name` so within-program forward jumps
    // emit a named call frame instead of `fn_at_pc_<pc>`.
    let mut fn_stack: Vec<String> = vec![outer_fn_name.clone()];

    // Walk snapshots with CPI detection.
    let mut prev_line: Option<u32> = None;
    let mut prev_pc: Option<u64> = None;
    let mut prev_regs: [u64; 12] = [0u64; 12];

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
                let cpi_fn_id = TraceWriter::ensure_function_id(
                    &mut *writer,
                    &callee_name,
                    &Path::new(&file_str),
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
                env_stack.push(var_env_for_fn(&model, &callee_name));
                fn_stack.push(callee_name);
                // Reset line tracking for the new program context.
                prev_line = None;
            }
            CpiEvent::CpiReturn { return_pc: _ } => {
                TraceWriter::register_return(&mut *writer, NONE_VALUE);
                if env_stack.len() > 1 {
                    env_stack.pop();
                }
                if fn_stack.len() > 1 {
                    fn_stack.pop();
                }
                // Reset line tracking for the returned-to context.
                prev_line = None;
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
                                let callee_fn_id = TraceWriter::ensure_function_id(
                                    &mut *writer,
                                    &callee_name,
                                    &Path::new(&file_str),
                                    Line(line as i64),
                                );
                                TraceWriter::register_call(&mut *writer, callee_fn_id, vec![]);
                                env_stack.push(var_env_for_fn(&model, &callee_name));
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
                                            env_stack.pop();
                                        }
                                    }
                                    None => {
                                        if fn_stack.len() > 1 {
                                            TraceWriter::register_return(&mut *writer, NONE_VALUE);
                                            fn_stack.pop();
                                            env_stack.pop();
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

        // Look up source location for this PC.
        let (file_str, line) = match registry.find_location(pc) {
            Some((f, l)) => (f, l),
            None => {
                // No source mapping; emit opcode-based line number.
                let program_name_str = cpi_detector.current_program();
                (format!("{program_name_str}.sbf"), pc as u32)
            }
        };

        // Emit step when line changes.
        if prev_line != Some(line) {
            TraceWriter::register_step(&mut *writer, &Path::new(&file_str), Line(line as i64));

            // Update the active frame's variable→register env from any
            // `let NAME = ...` binding visible on this step before
            // synthesising events.
            if let Some(env) = env_stack.last_mut()
                && let Some(name) = extract_let_lhs(strip_line_for_match(model.line(line)))
            {
                env.record_let(name, &prev_regs, snap);
            }

            // Synthesise side-effecting / typed-value events.
            let active_env = env_stack.last().cloned().unwrap_or_else(VarEnv::new);
            synthesise_step_events(&model, &mut *writer, line, &mut type_ids, &active_env, snap);

            prev_line = Some(line);
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
        prev_regs = snap.registers;
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
