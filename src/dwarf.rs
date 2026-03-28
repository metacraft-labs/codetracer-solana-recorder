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
        let elf_addr = self.text_vaddr + sbf_pc * 8;
        let loc = self.context.find_location(elf_addr).ok()??;
        Some(SourceLocation {
            file: loc.file?.to_string(),
            line: loc.line?,
            column: loc.column,
        })
    }
}
