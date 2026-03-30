//! DWARF debug info parsing for source mapping.
//!
//! This module parses DWARF information from compiled Solana ELF files
//! to map SBF instruction addresses back to Rust source locations.
//!
//! The mapping uses the formula:
//!     elf_addr = text_section_vaddr + (sbf_pc * 8)
//! because each SBF instruction is 8 bytes wide.

use eyre::{Result, eyre};

/// A resolved source location from DWARF debug info.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLocation {
    pub file: String,
    pub line: u32,
    pub column: Option<u32>,
}

/// A function boundary extracted from DWARF `.debug_info`.
///
/// Each entry corresponds to a `DW_TAG_subprogram` DIE that has both a name
/// and an address range.  The recorder uses these boundaries to emit accurate
/// Call/Return events at function entry and exit points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionBoundary {
    /// The function name from `DW_AT_name` (or `DW_AT_linkage_name`).
    pub name: String,
    /// First byte address of the function (inclusive).
    pub start_addr: u64,
    /// One-past-the-end byte address of the function (exclusive).
    pub end_addr: u64,
}

/// Parses DWARF debug information from a Solana program ELF file (in-memory).
pub struct DwarfParser {
    /// Virtual address of the `.text` section in the ELF.
    text_vaddr: u64,
    /// addr2line context for resolving addresses.
    context: addr2line::Context<gimli::EndianSlice<'static, gimli::RunTimeEndian>>,
}

impl DwarfParser {
    /// Create a new DWARF parser from raw ELF bytes.
    ///
    /// The `elf_data` slice is copied internally so the caller's reference
    /// does not need to outlive the parser.
    ///
    /// # Errors
    ///
    /// Returns an error if the data is not a valid ELF or lacks DWARF info.
    pub fn new(elf_data: &[u8]) -> Result<Self> {
        use object::{Object, ObjectSection};

        // We need the data to live as long as the context, so we leak a copy.
        let owned: &'static [u8] = Vec::leak(elf_data.to_vec());

        let obj = object::File::parse(owned)
            .map_err(|e| eyre!("failed to parse ELF: {e}"))?;

        // Find .text section virtual address.
        let text_vaddr = obj
            .section_by_name(".text")
            .map(|s| s.address())
            .unwrap_or(0);

        // Determine endianness.
        let endian = if obj.is_little_endian() {
            gimli::RunTimeEndian::Little
        } else {
            gimli::RunTimeEndian::Big
        };

        // Load DWARF sections from the object file.
        let dwarf = gimli::Dwarf::load(|section_id| -> std::result::Result<_, gimli::Error> {
            let data = obj
                .section_by_name(section_id.name())
                .and_then(|s| {
                    use object::ObjectSection;
                    s.uncompressed_data().ok()
                })
                .unwrap_or(std::borrow::Cow::Borrowed(&[]));
            Ok(gimli::EndianSlice::new(
                // Leak the section data so it has 'static lifetime.
                &*Vec::leak(data.into_owned()),
                endian,
            ))
        })
        .map_err(|e| eyre!("failed to load DWARF sections: {e}"))?;

        let ctx = addr2line::Context::from_dwarf(dwarf)
            .map_err(|e| eyre!("failed to build addr2line context: {e}"))?;

        Ok(Self {
            text_vaddr,
            context: ctx,
        })
    }

    /// The virtual address of the `.text` section.
    pub fn text_vaddr(&self) -> u64 {
        self.text_vaddr
    }

    /// Map an SBF program counter value to a source location.
    ///
    /// Uses the formula: `elf_addr = text_vaddr + (sbf_pc * 8)`.
    pub fn find_location(&self, sbf_pc: u64) -> Option<SourceLocation> {
        let elf_addr = self
            .text_vaddr
            .checked_add(sbf_pc.checked_mul(8)?)?;
        let loc = self.context.find_location(elf_addr).ok()??;
        Some(SourceLocation {
            file: loc.file?.to_string(),
            line: loc.line?,
            column: loc.column,
        })
    }
}

/// Extract function boundaries from the `.debug_info` section of an ELF binary.
///
/// Iterates over all compilation units and their DIEs, collecting every
/// `DW_TAG_subprogram` entry that has a name and a valid address range
/// (`DW_AT_low_pc` / `DW_AT_high_pc`).
///
/// Functions without address ranges (e.g. declarations, or fully-inlined
/// functions with no out-of-line copy) are silently skipped.
///
/// # Errors
///
/// Returns an error if `elf_data` is not a valid ELF or DWARF parsing fails.
pub fn find_functions(elf_data: &[u8]) -> Result<Vec<FunctionBoundary>> {
    use object::Object;

    let owned: &'static [u8] = Vec::leak(elf_data.to_vec());
    let obj = object::File::parse(owned)
        .map_err(|e| eyre!("failed to parse ELF: {e}"))?;

    let endian = if obj.is_little_endian() {
        gimli::RunTimeEndian::Little
    } else {
        gimli::RunTimeEndian::Big
    };

    let dwarf = gimli::Dwarf::load(|section_id| -> std::result::Result<_, gimli::Error> {
        let data = obj
            .section_by_name(section_id.name())
            .and_then(|s| {
                use object::ObjectSection;
                s.uncompressed_data().ok()
            })
            .unwrap_or(std::borrow::Cow::Borrowed(&[]));
        Ok(gimli::EndianSlice::new(
            &*Vec::leak(data.into_owned()),
            endian,
        ))
    })
    .map_err(|e| eyre!("failed to load DWARF sections: {e}"))?;

    let mut functions = Vec::new();
    let mut units = dwarf.units();

    while let Some(unit_header) = units.next().map_err(|e| eyre!("DWARF unit iteration: {e}"))? {
        let unit = dwarf
            .unit(unit_header)
            .map_err(|e| eyre!("failed to parse DWARF unit: {e}"))?;

        let mut entries = unit.entries();
        while let Some((_, entry)) = entries.next_dfs().map_err(|e| eyre!("DIE iteration: {e}"))? {
            if entry.tag() != gimli::DW_TAG_subprogram {
                continue;
            }

            // --- name -----------------------------------------------------------
            let name = extract_subprogram_name(entry, &dwarf, &unit);
            let name = match name {
                Some(n) => n,
                None => continue, // unnamed subprogram — skip
            };

            // --- address range --------------------------------------------------
            let low_pc = match entry.attr_value(gimli::DW_AT_low_pc)
                .map_err(|e| eyre!("DW_AT_low_pc: {e}"))?
            {
                Some(gimli::AttributeValue::Addr(addr)) => addr,
                _ => continue, // no low_pc — declaration or abstract origin
            };

            let high_pc = match entry.attr_value(gimli::DW_AT_high_pc)
                .map_err(|e| eyre!("DW_AT_high_pc: {e}"))?
            {
                Some(gimli::AttributeValue::Addr(addr)) => addr,
                Some(gimli::AttributeValue::Udata(len)) => low_pc + len,
                _ => continue, // no high_pc — skip
            };

            if high_pc <= low_pc {
                continue; // degenerate range
            }

            functions.push(FunctionBoundary {
                name,
                start_addr: low_pc,
                end_addr: high_pc,
            });
        }
    }

    Ok(functions)
}

/// Try to extract a human-readable name for a `DW_TAG_subprogram` DIE.
///
/// Prefers `DW_AT_name`; falls back to `DW_AT_linkage_name`.
fn extract_subprogram_name<R: gimli::Reader>(
    entry: &gimli::DebuggingInformationEntry<R>,
    dwarf: &gimli::Dwarf<R>,
    unit: &gimli::Unit<R>,
) -> Option<String> {
    // Try DW_AT_name first.
    if let Ok(Some(attr)) = entry.attr(gimli::DW_AT_name) {
        if let Ok(s) = dwarf.attr_string(unit, attr.value()) {
            if let Ok(name) = s.to_string() {
                return Some(name.to_string());
            }
        }
    }
    // Fall back to DW_AT_linkage_name.
    if let Ok(Some(attr)) = entry.attr(gimli::DW_AT_linkage_name) {
        if let Ok(s) = dwarf.attr_string(unit, attr.value()) {
            if let Ok(name) = s.to_string() {
                return Some(name.to_string());
            }
        }
    }
    None
}
