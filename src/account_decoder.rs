//! Decode Borsh-serialized Solana account data using Anchor IDL definitions.
//!
//! Anchor programs use a standard IDL (Interface Description Language) JSON
//! format that describes account structures. This module parses that IDL and
//! uses it to decode raw account bytes into typed fields.
//!
//! # Borsh encoding
//!
//! - u8/u16/u32/u64/i64: little-endian fixed-width
//! - bool: 1 byte (0 = false, 1 = true)
//! - String: 4-byte LE length prefix + UTF-8 bytes
//! - Vec<T>: 4-byte LE length prefix + N encoded elements
//! - Pubkey: 32 raw bytes
//! - Struct: fields encoded in declaration order (no framing)
//!
//! Anchor accounts additionally have an 8-byte discriminator prefix that
//! the decoder skips before reading fields.

use codetracer_trace_types::{
    FieldTypeRecord, TypeId, TypeKind, TypeRecord, TypeSpecificInfo, ValueRecord,
};
use codetracer_trace_writer_nim::trace_writer::TraceWriter;
use eyre::{Result, bail, ensure};
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Anchor IDL types (parsed from JSON)
// ---------------------------------------------------------------------------

/// Parsed Anchor IDL.
#[derive(Debug, Clone, Deserialize)]
pub struct AnchorIdl {
    pub version: String,
    pub name: String,
    #[serde(default)]
    pub accounts: Vec<IdlAccount>,
    #[serde(default)]
    pub types: Vec<IdlTypeDef>,
}

/// An account definition in the IDL.
#[derive(Debug, Clone, Deserialize)]
pub struct IdlAccount {
    pub name: String,
    #[serde(rename = "type")]
    pub type_def: IdlTypeDefBody,
}

/// A named type definition (may be referenced by accounts or other types).
#[derive(Debug, Clone, Deserialize)]
pub struct IdlTypeDef {
    pub name: String,
    #[serde(rename = "type")]
    pub type_def: IdlTypeDefBody,
}

/// Body of a type definition.
#[derive(Debug, Clone, Deserialize)]
pub struct IdlTypeDefBody {
    pub kind: String,
    #[serde(default)]
    pub fields: Vec<IdlField>,
}

/// A single field inside a struct type.
#[derive(Debug, Clone, Deserialize)]
pub struct IdlField {
    pub name: String,
    #[serde(rename = "type")]
    pub field_type: IdlType,
}

/// IDL type representation — either a simple string or a compound type.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum IdlType {
    /// Simple type like `"u64"`, `"bool"`, `"string"`, `"publicKey"`.
    Simple(String),
    /// Compound type like `{"vec": "u64"}` or `{"defined": "MyStruct"}`.
    Compound(std::collections::HashMap<String, IdlType>),
}

impl IdlType {
    /// Return a human-readable type name.
    pub fn type_name(&self) -> String {
        match self {
            IdlType::Simple(s) => s.clone(),
            IdlType::Compound(map) => {
                if let Some(inner) = map.get("vec") {
                    format!("Vec<{}>", inner.type_name())
                } else if let Some(IdlType::Simple(name)) = map.get("defined") {
                    name.clone()
                } else {
                    "unknown".to_string()
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Decoded output types
// ---------------------------------------------------------------------------

/// A decoded field with name, type, and value.
#[derive(Debug, Clone)]
pub struct DecodedField {
    pub name: String,
    pub type_name: String,
    pub value: DecodedValue,
}

/// Decoded value variants.
#[derive(Debug, Clone)]
pub enum DecodedValue {
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    I64(i64),
    Bool(bool),
    String(String),
    Pubkey([u8; 32]),
    Vec(Vec<DecodedValue>),
    Struct(Vec<DecodedField>),
}

impl DecodedValue {
    /// Format as a display string suitable for trace output.
    pub fn display(&self) -> String {
        match self {
            DecodedValue::U8(v) => v.to_string(),
            DecodedValue::U16(v) => v.to_string(),
            DecodedValue::U32(v) => v.to_string(),
            DecodedValue::U64(v) => v.to_string(),
            DecodedValue::I64(v) => v.to_string(),
            DecodedValue::Bool(v) => v.to_string(),
            DecodedValue::String(v) => format!("\"{v}\""),
            DecodedValue::Pubkey(bytes) => bs58_encode(bytes),
            DecodedValue::Vec(elems) => {
                let inner: Vec<String> = elems.iter().map(|e| e.display()).collect();
                format!("[{}]", inner.join(", "))
            }
            DecodedValue::Struct(fields) => {
                let inner: Vec<String> = fields
                    .iter()
                    .map(|f| format!("{}: {}", f.name, f.value.display()))
                    .collect();
                format!("{{ {} }}", inner.join(", "))
            }
        }
    }
}

/// Minimal base58 encoder (no external dependency needed).
fn bs58_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

    if bytes.is_empty() {
        return String::new();
    }

    // Count leading zeros.
    let leading_zeros = bytes.iter().take_while(|&&b| b == 0).count();

    // Convert to base58 using big-integer division.
    let mut digits: Vec<u8> = Vec::new();
    for &byte in bytes {
        let mut carry = byte as u32;
        for d in digits.iter_mut() {
            carry += (*d as u32) * 256;
            *d = (carry % 58) as u8;
            carry /= 58;
        }
        while carry > 0 {
            digits.push((carry % 58) as u8);
            carry /= 58;
        }
    }

    let mut result = String::with_capacity(leading_zeros + digits.len());
    for _ in 0..leading_zeros {
        result.push(ALPHABET[0] as char);
    }
    for &d in digits.iter().rev() {
        result.push(ALPHABET[d as usize] as char);
    }
    result
}

// ---------------------------------------------------------------------------
// Borsh decoder
// ---------------------------------------------------------------------------

/// Manual Borsh decoder that reads sequentially from a byte slice.
pub struct BorshDecoder<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BorshDecoder<'a> {
    /// Create a new decoder positioned at the start of `data`.
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Return the remaining unconsumed bytes.
    pub fn remaining(&self) -> &'a [u8] {
        &self.data[self.pos..]
    }

    /// Read a single `u8`.
    pub fn read_u8(&mut self) -> Result<u8> {
        ensure!(self.pos < self.data.len(), "not enough data for u8");
        let v = self.data[self.pos];
        self.pos += 1;
        Ok(v)
    }

    /// Read a `u16` (2 bytes LE).
    pub fn read_u16(&mut self) -> Result<u16> {
        ensure!(self.pos + 2 <= self.data.len(), "not enough data for u16");
        let v = u16::from_le_bytes(self.data[self.pos..self.pos + 2].try_into().unwrap());
        self.pos += 2;
        Ok(v)
    }

    /// Read a `u32` (4 bytes LE).
    pub fn read_u32(&mut self) -> Result<u32> {
        ensure!(self.pos + 4 <= self.data.len(), "not enough data for u32");
        let v = u32::from_le_bytes(self.data[self.pos..self.pos + 4].try_into().unwrap());
        self.pos += 4;
        Ok(v)
    }

    /// Read a `u64` (8 bytes LE).
    pub fn read_u64(&mut self) -> Result<u64> {
        ensure!(self.pos + 8 <= self.data.len(), "not enough data for u64");
        let v = u64::from_le_bytes(self.data[self.pos..self.pos + 8].try_into().unwrap());
        self.pos += 8;
        Ok(v)
    }

    /// Read an `i64` (8 bytes LE, signed).
    pub fn read_i64(&mut self) -> Result<i64> {
        ensure!(self.pos + 8 <= self.data.len(), "not enough data for i64");
        let v = i64::from_le_bytes(self.data[self.pos..self.pos + 8].try_into().unwrap());
        self.pos += 8;
        Ok(v)
    }

    /// Read a `bool` (1 byte: 0 = false, 1 = true).
    pub fn read_bool(&mut self) -> Result<bool> {
        let v = self.read_u8()?;
        Ok(v != 0)
    }

    /// Read a Borsh-encoded `String` (4-byte LE length + UTF-8).
    pub fn read_string(&mut self) -> Result<String> {
        let len = self.read_u32()? as usize;
        ensure!(
            self.pos + len <= self.data.len(),
            "not enough data for string of length {len}"
        );
        let s = std::str::from_utf8(&self.data[self.pos..self.pos + len])?.to_string();
        self.pos += len;
        Ok(s)
    }

    /// Read a 32-byte public key.
    pub fn read_pubkey(&mut self) -> Result<[u8; 32]> {
        ensure!(
            self.pos + 32 <= self.data.len(),
            "not enough data for pubkey"
        );
        let mut key = [0u8; 32];
        key.copy_from_slice(&self.data[self.pos..self.pos + 32]);
        self.pos += 32;
        Ok(key)
    }

    /// Read a `Vec<T>` where each element is decoded according to `inner_type`.
    pub fn read_vec(&mut self, inner_type: &IdlType) -> Result<Vec<DecodedValue>> {
        let len = self.read_u32()? as usize;
        let mut elements = Vec::with_capacity(len);
        for _ in 0..len {
            elements.push(self.decode_type(inner_type)?);
        }
        Ok(elements)
    }

    /// Skip `n` bytes (e.g. the 8-byte Anchor discriminator).
    pub fn skip(&mut self, n: usize) -> Result<()> {
        ensure!(
            self.pos + n <= self.data.len(),
            "not enough data to skip {n} bytes"
        );
        self.pos += n;
        Ok(())
    }

    /// Decode a value of the given IDL type.
    pub fn decode_type(&mut self, idl_type: &IdlType) -> Result<DecodedValue> {
        match idl_type {
            IdlType::Simple(s) => match s.as_str() {
                "u8" => Ok(DecodedValue::U8(self.read_u8()?)),
                "u16" => Ok(DecodedValue::U16(self.read_u16()?)),
                "u32" => Ok(DecodedValue::U32(self.read_u32()?)),
                "u64" => Ok(DecodedValue::U64(self.read_u64()?)),
                "i64" => Ok(DecodedValue::I64(self.read_i64()?)),
                "bool" => Ok(DecodedValue::Bool(self.read_bool()?)),
                "string" => Ok(DecodedValue::String(self.read_string()?)),
                "publicKey" => Ok(DecodedValue::Pubkey(self.read_pubkey()?)),
                other => bail!("unsupported simple IDL type: {other}"),
            },
            IdlType::Compound(map) => {
                if let Some(inner) = map.get("vec") {
                    Ok(DecodedValue::Vec(self.read_vec(inner)?))
                } else {
                    bail!("unsupported compound IDL type: {map:?}")
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// AnchorIdl implementation
// ---------------------------------------------------------------------------

impl AnchorIdl {
    /// Parse an Anchor IDL from JSON.
    pub fn from_json(json: &str) -> Result<Self> {
        let idl: AnchorIdl = serde_json::from_str(json)?;
        Ok(idl)
    }

    /// Find an account definition by name.
    fn find_account(&self, account_name: &str) -> Option<&IdlAccount> {
        self.accounts.iter().find(|a| a.name == account_name)
    }

    /// Decode Borsh-serialized account data (including the 8-byte Anchor
    /// discriminator prefix) into typed fields.
    pub fn decode_account(&self, account_name: &str, data: &[u8]) -> Result<Vec<DecodedField>> {
        let account = self
            .find_account(account_name)
            .ok_or_else(|| eyre::eyre!("account '{account_name}' not found in IDL"))?;

        ensure!(
            account.type_def.kind == "struct",
            "account '{}' is not a struct (kind={})",
            account_name,
            account.type_def.kind
        );

        let mut decoder = BorshDecoder::new(data);

        // Skip the 8-byte Anchor discriminator.
        decoder.skip(8)?;

        // Decode each field.
        let mut fields = Vec::new();
        for field_def in &account.type_def.fields {
            let value = decoder.decode_type(&field_def.field_type)?;
            fields.push(DecodedField {
                name: field_def.name.clone(),
                type_name: field_def.field_type.type_name(),
                value,
            });
        }

        Ok(fields)
    }
}

// ---------------------------------------------------------------------------
// TypeIds helper
// ---------------------------------------------------------------------------

/// Holds pre-registered CodeTracer type IDs for account-related types.
pub struct TypeIds {
    pub u8_type_id: TypeId,
    pub u16_type_id: TypeId,
    pub u32_type_id: TypeId,
    pub u64_type_id: TypeId,
    pub i64_type_id: TypeId,
    pub bool_type_id: TypeId,
    pub string_type_id: TypeId,
    pub pubkey_type_id: TypeId,
    pub bytes_type_id: TypeId,
}

impl TypeIds {
    /// Register all account-related types with the trace writer and return
    /// the resulting IDs.
    pub fn register(writer: &mut dyn TraceWriter) -> Self {
        Self {
            u8_type_id: TraceWriter::ensure_type_id(writer, TypeKind::Int, "u8"),
            u16_type_id: TraceWriter::ensure_type_id(writer, TypeKind::Int, "u16"),
            u32_type_id: TraceWriter::ensure_type_id(writer, TypeKind::Int, "u32"),
            u64_type_id: TraceWriter::ensure_type_id(writer, TypeKind::Int, "u64"),
            i64_type_id: TraceWriter::ensure_type_id(writer, TypeKind::Int, "i64"),
            bool_type_id: TraceWriter::ensure_type_id(writer, TypeKind::Bool, "bool"),
            string_type_id: TraceWriter::ensure_type_id(writer, TypeKind::String, "String"),
            pubkey_type_id: TraceWriter::ensure_type_id(writer, TypeKind::String, "Pubkey"),
            bytes_type_id: TraceWriter::ensure_type_id(writer, TypeKind::Seq, "Vec<u8>"),
        }
    }
}

// ---------------------------------------------------------------------------
// Conversion to ValueRecord
// ---------------------------------------------------------------------------

/// Convert a [`DecodedField`] to a CodeTracer [`ValueRecord`].
///
/// Structs are represented as `ValueRecord::Struct` with a dynamically
/// registered struct type. Pubkeys are rendered as base58 strings.
pub fn decoded_to_value_record(field: &DecodedField, type_ids: &TypeIds) -> ValueRecord {
    decoded_value_to_record(&field.value, &field.type_name, type_ids)
}

fn decoded_value_to_record(
    value: &DecodedValue,
    _type_hint: &str,
    type_ids: &TypeIds,
) -> ValueRecord {
    match value {
        DecodedValue::U8(v) => ValueRecord::Int {
            i: *v as i64,
            type_id: type_ids.u8_type_id,
        },
        DecodedValue::U16(v) => ValueRecord::Int {
            i: *v as i64,
            type_id: type_ids.u16_type_id,
        },
        DecodedValue::U32(v) => ValueRecord::Int {
            i: *v as i64,
            type_id: type_ids.u32_type_id,
        },
        DecodedValue::U64(v) => ValueRecord::Int {
            i: *v as i64,
            type_id: type_ids.u64_type_id,
        },
        DecodedValue::I64(v) => ValueRecord::Int {
            i: *v,
            type_id: type_ids.i64_type_id,
        },
        DecodedValue::Bool(v) => ValueRecord::Bool {
            b: *v,
            type_id: type_ids.bool_type_id,
        },
        DecodedValue::String(v) => ValueRecord::String {
            text: v.clone(),
            type_id: type_ids.string_type_id,
        },
        DecodedValue::Pubkey(bytes) => ValueRecord::String {
            text: bs58_encode(bytes),
            type_id: type_ids.pubkey_type_id,
        },
        DecodedValue::Vec(elems) => {
            let elements: Vec<ValueRecord> = elems
                .iter()
                .map(|e| decoded_value_to_record(e, "element", type_ids))
                .collect();
            ValueRecord::Sequence {
                elements,
                is_slice: false,
                type_id: type_ids.bytes_type_id,
            }
        }
        DecodedValue::Struct(_fields) => {
            // Use a Raw record to display the struct since we cannot register
            // a proper struct type without the writer. The display string
            // gives a readable representation.
            ValueRecord::Raw {
                r: value.display(),
                type_id: type_ids.u64_type_id, // placeholder
            }
        }
    }
}

/// Register a struct type in the trace writer for a decoded account and
/// return a [`ValueRecord::Struct`] with proper field types.
pub fn decoded_fields_to_struct_record(
    account_name: &str,
    fields: &[DecodedField],
    type_ids: &TypeIds,
    writer: &mut dyn TraceWriter,
) -> ValueRecord {
    // Register field type records.
    let field_type_records: Vec<FieldTypeRecord> = fields
        .iter()
        .map(|f| {
            let tid = match &f.value {
                DecodedValue::U8(_) => type_ids.u8_type_id,
                DecodedValue::U16(_) => type_ids.u16_type_id,
                DecodedValue::U32(_) => type_ids.u32_type_id,
                DecodedValue::U64(_) => type_ids.u64_type_id,
                DecodedValue::I64(_) => type_ids.i64_type_id,
                DecodedValue::Bool(_) => type_ids.bool_type_id,
                DecodedValue::String(_) => type_ids.string_type_id,
                DecodedValue::Pubkey(_) => type_ids.pubkey_type_id,
                DecodedValue::Vec(_) => type_ids.bytes_type_id,
                DecodedValue::Struct(_) => type_ids.u64_type_id, // placeholder
            };
            FieldTypeRecord {
                name: f.name.clone(),
                type_id: tid,
            }
        })
        .collect();

    // Register the struct type.
    let struct_type = TypeRecord {
        kind: TypeKind::Struct,
        lang_type: account_name.to_string(),
        specific_info: TypeSpecificInfo::Struct {
            fields: field_type_records,
        },
    };
    let struct_type_id = TraceWriter::ensure_raw_type_id(writer, struct_type);

    // Build field values.
    let field_values: Vec<ValueRecord> = fields
        .iter()
        .map(|f| decoded_to_value_record(f, type_ids))
        .collect();

    ValueRecord::Struct {
        field_values,
        type_id: struct_type_id,
    }
}
