//! Multi-program registry for CPI scenarios.
//!
//! When a transaction involves multiple programs (via CPI), each program
//! has its own address range and potentially its own DWARF debug info.
//! This module maps program counter values to the correct program and
//! its associated debug information.

use std::ops::Range;

use eyre::{Result, eyre};

use crate::dwarf::DwarfParser;

/// A registered program with its PC range and optional DWARF parser.
struct ProgramEntry {
    name: String,
    pc_range: Range<u64>,
    dwarf: Option<DwarfParser>,
    /// Synthetic source locations for testing (pc, file, line).
    synthetic_locations: Vec<(u64, String, u32)>,
}

/// Registry that maps program counter values to individual programs
/// and their associated DWARF parsers.
pub struct ProgramRegistry {
    programs: Vec<ProgramEntry>,
}

impl ProgramRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            programs: Vec::new(),
        }
    }

    /// Register a program from its ELF data.
    ///
    /// The `pc_range` defines the program counter range that belongs to
    /// this program. The `elf_data` is parsed for DWARF debug info.
    pub fn add_program(&mut self, name: &str, pc_range: Range<u64>, elf_data: &[u8]) -> Result<()> {
        let dwarf = DwarfParser::new(elf_data)
            .map_err(|e| eyre!("failed to parse DWARF for program '{name}': {e}"))?;

        self.programs.push(ProgramEntry {
            name: name.to_string(),
            pc_range,
            dwarf: Some(dwarf),
            synthetic_locations: Vec::new(),
        });
        Ok(())
    }

    /// Register a synthetic program for testing purposes.
    ///
    /// No real ELF/DWARF is required. Source locations are provided
    /// directly as `(pc, file, line)` tuples.
    pub fn add_synthetic_program(
        &mut self,
        name: &str,
        pc_range: Range<u64>,
        source_locations: Vec<(u64, String, u32)>,
    ) {
        self.programs.push(ProgramEntry {
            name: name.to_string(),
            pc_range,
            dwarf: None,
            synthetic_locations: source_locations,
        });
    }

    /// Look up which program owns a given PC value.
    ///
    /// Returns `Some((name, dwarf_parser))` if the PC falls within a
    /// registered program's range. The DWARF parser is `None` for
    /// synthetic programs.
    pub fn find_program(&self, pc: u64) -> Option<(&str, Option<&DwarfParser>)> {
        self.programs
            .iter()
            .find(|p| p.pc_range.contains(&pc))
            .map(|p| (p.name.as_str(), p.dwarf.as_ref()))
    }

    /// Look up a source location for a PC in any registered program.
    ///
    /// For programs with DWARF info, uses `DwarfParser::find_location`.
    /// For synthetic programs, does a direct lookup in the provided
    /// source location table.
    pub fn find_location(&self, pc: u64) -> Option<(String, u32)> {
        let entry = self.programs.iter().find(|p| p.pc_range.contains(&pc))?;

        if let Some(ref dwarf) = entry.dwarf {
            let loc = dwarf.find_location(pc)?;
            Some((loc.file, loc.line))
        } else {
            // Synthetic program: direct lookup.
            entry
                .synthetic_locations
                .iter()
                .find(|(loc_pc, _, _)| *loc_pc == pc)
                .map(|(_, file, line)| (file.clone(), *line))
        }
    }

    /// Get the name of the program that owns a given PC.
    pub fn program_name(&self, pc: u64) -> Option<&str> {
        self.programs
            .iter()
            .find(|p| p.pc_range.contains(&pc))
            .map(|p| p.name.as_str())
    }

    /// Get the PC range for a named program.
    pub fn program_range(&self, name: &str) -> Option<Range<u64>> {
        self.programs
            .iter()
            .find(|p| p.name == name)
            .map(|p| p.pc_range.clone())
    }
}

impl Default for ProgramRegistry {
    fn default() -> Self {
        Self::new()
    }
}
