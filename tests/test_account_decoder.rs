//! Tests for the account decoder module (M6).

use codetracer_solana_recorder::account_decoder::{
    AnchorIdl, BorshDecoder, DecodedField, DecodedValue, TypeIds, decoded_to_value_record,
};
use codetracer_trace_types::ValueRecord;
use codetracer_trace_writer_nim::trace_writer::TraceWriter;
use codetracer_trace_writer_nim::{TraceEventsFileFormat, create_trace_writer};

// ---------------------------------------------------------------------------
// Borsh decoding tests
// ---------------------------------------------------------------------------

#[test]
fn test_borsh_decode_u64() {
    let bytes = 42u64.to_le_bytes();
    let mut decoder = BorshDecoder::new(&bytes);
    let val = decoder.read_u64().unwrap();
    assert_eq!(val, 42);
    assert!(decoder.remaining().is_empty());
}

#[test]
fn test_borsh_decode_string() {
    // Borsh string: 4-byte LE length + UTF-8 bytes
    let text = "hello";
    let len = (text.len() as u32).to_le_bytes();
    let mut data = Vec::new();
    data.extend_from_slice(&len);
    data.extend_from_slice(text.as_bytes());

    let mut decoder = BorshDecoder::new(&data);
    let val = decoder.read_string().unwrap();
    assert_eq!(val, "hello");
    assert!(decoder.remaining().is_empty());
}

#[test]
fn test_borsh_decode_bool() {
    // false = 0
    let mut decoder = BorshDecoder::new(&[0u8]);
    assert!(!decoder.read_bool().unwrap());

    // true = 1
    let mut decoder = BorshDecoder::new(&[1u8]);
    assert!(decoder.read_bool().unwrap());

    // non-zero also true
    let mut decoder = BorshDecoder::new(&[42u8]);
    assert!(decoder.read_bool().unwrap());
}

#[test]
fn test_borsh_decode_pubkey() {
    let mut key_bytes = [0u8; 32];
    key_bytes[0] = 1;
    key_bytes[31] = 0xFF;

    let mut decoder = BorshDecoder::new(&key_bytes);
    let pubkey = decoder.read_pubkey().unwrap();
    assert_eq!(pubkey[0], 1);
    assert_eq!(pubkey[31], 0xFF);
    assert!(decoder.remaining().is_empty());
}

// ---------------------------------------------------------------------------
// Anchor IDL parsing tests
// ---------------------------------------------------------------------------

#[test]
fn test_anchor_idl_parsing() {
    let idl_json = r#"{
        "version": "0.1.0",
        "name": "my_program",
        "accounts": [
            {
                "name": "Counter",
                "type": {
                    "kind": "struct",
                    "fields": [
                        { "name": "count", "type": "u64" },
                        { "name": "authority", "type": "publicKey" },
                        { "name": "is_initialized", "type": "bool" }
                    ]
                }
            },
            {
                "name": "Config",
                "type": {
                    "kind": "struct",
                    "fields": [
                        { "name": "admin", "type": "publicKey" },
                        { "name": "label", "type": "string" }
                    ]
                }
            }
        ],
        "types": []
    }"#;

    let idl = AnchorIdl::from_json(idl_json).unwrap();
    assert_eq!(idl.name, "my_program");
    assert_eq!(idl.version, "0.1.0");
    assert_eq!(idl.accounts.len(), 2);

    let counter = &idl.accounts[0];
    assert_eq!(counter.name, "Counter");
    assert_eq!(counter.type_def.kind, "struct");
    assert_eq!(counter.type_def.fields.len(), 3);
    assert_eq!(counter.type_def.fields[0].name, "count");
    assert_eq!(counter.type_def.fields[2].name, "is_initialized");

    let config = &idl.accounts[1];
    assert_eq!(config.name, "Config");
    assert_eq!(config.type_def.fields.len(), 2);
}

// ---------------------------------------------------------------------------
// Full account decoding test
// ---------------------------------------------------------------------------

#[test]
fn test_anchor_account_decode() {
    let idl_json = r#"{
        "version": "0.1.0",
        "name": "counter",
        "accounts": [
            {
                "name": "Counter",
                "type": {
                    "kind": "struct",
                    "fields": [
                        { "name": "count", "type": "u64" },
                        { "name": "is_initialized", "type": "bool" }
                    ]
                }
            }
        ]
    }"#;

    let idl = AnchorIdl::from_json(idl_json).unwrap();

    // Build account data: 8-byte discriminator + u64 (count=7) + bool (true)
    let mut data = Vec::new();
    // 8-byte discriminator (arbitrary).
    data.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD, 0x11, 0x22, 0x33, 0x44]);
    // count = 7 as u64 LE.
    data.extend_from_slice(&7u64.to_le_bytes());
    // is_initialized = true.
    data.push(1);

    let fields = idl.decode_account("Counter", &data).unwrap();
    assert_eq!(fields.len(), 2);

    assert_eq!(fields[0].name, "count");
    assert_eq!(fields[0].type_name, "u64");
    match &fields[0].value {
        DecodedValue::U64(v) => assert_eq!(*v, 7),
        other => panic!("expected U64, got {:?}", other),
    }

    assert_eq!(fields[1].name, "is_initialized");
    assert_eq!(fields[1].type_name, "bool");
    match &fields[1].value {
        DecodedValue::Bool(v) => assert!(*v),
        other => panic!("expected Bool, got {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// Conversion to ValueRecord test
// ---------------------------------------------------------------------------

#[test]
fn test_decoded_to_value_record() {
    // Create a trace writer to register types.
    let mut writer = create_trace_writer("test", &[], TraceEventsFileFormat::Ctfs);

    // We need to call start before registering types.  Legacy
    // metadata/paths sidecar begin calls were no-ops on the Nim side and
    // were retired with the v3 CTFS rollout (follow-up #254 phase 2).
    let tmp = tempfile::tempdir().unwrap();
    let events_path = tmp.path().join("trace.json");

    TraceWriter::begin_writing_trace_events(&mut *writer, &events_path).unwrap();

    TraceWriter::start(
        &mut *writer,
        std::path::Path::new("test.rs"),
        codetracer_trace_types::Line(1),
    );

    let type_ids = TypeIds::register(&mut *writer);

    // Test u64 field.
    let field_u64 = DecodedField {
        name: "count".to_string(),
        type_name: "u64".to_string(),
        value: DecodedValue::U64(42),
    };
    let vr = decoded_to_value_record(&field_u64, &type_ids);
    match &vr {
        ValueRecord::Int { i, type_id } => {
            assert_eq!(*i, 42);
            assert_eq!(*type_id, type_ids.u64_type_id);
        }
        other => panic!("expected Int, got {:?}", other),
    }

    // Test bool field.
    let field_bool = DecodedField {
        name: "flag".to_string(),
        type_name: "bool".to_string(),
        value: DecodedValue::Bool(true),
    };
    let vr = decoded_to_value_record(&field_bool, &type_ids);
    match &vr {
        ValueRecord::Bool { b, type_id } => {
            assert!(*b);
            assert_eq!(*type_id, type_ids.bool_type_id);
        }
        other => panic!("expected Bool, got {:?}", other),
    }

    // Test string field.
    let field_str = DecodedField {
        name: "label".to_string(),
        type_name: "string".to_string(),
        value: DecodedValue::String("hello".to_string()),
    };
    let vr = decoded_to_value_record(&field_str, &type_ids);
    match &vr {
        ValueRecord::String { text, type_id } => {
            assert_eq!(text, "hello");
            assert_eq!(*type_id, type_ids.string_type_id);
        }
        other => panic!("expected String, got {:?}", other),
    }

    // Test pubkey field — should be rendered as base58 string.
    let mut key = [0u8; 32];
    key[0] = 1;
    let field_pubkey = DecodedField {
        name: "authority".to_string(),
        type_name: "publicKey".to_string(),
        value: DecodedValue::Pubkey(key),
    };
    let vr = decoded_to_value_record(&field_pubkey, &type_ids);
    match &vr {
        ValueRecord::String { text, type_id } => {
            // base58 of a key starting with 1 and rest zeros
            assert!(!text.is_empty());
            assert_eq!(*type_id, type_ids.pubkey_type_id);
        }
        other => panic!("expected String (base58 pubkey), got {:?}", other),
    }

    // Cleanup.
    TraceWriter::finish_writing_trace_events(&mut *writer).unwrap();
    TraceWriter::finish_writing_trace_metadata(&mut *writer).unwrap();
    TraceWriter::finish_writing_trace_paths(&mut *writer).unwrap();
}

// ---------------------------------------------------------------------------
// Realistic program account decoding tests (M4 deliverable)
//
// These tests use Borsh serialization that matches what real Solana programs
// produce, rather than hand-crafted synthetic byte arrays. Each test defines
// a struct mirroring a real program's account layout, serializes it using
// standard Borsh encoding rules, and verifies the decoder extracts correct
// field names, types, and values.
// ---------------------------------------------------------------------------

/// Helper: Borsh-serialize fields into a Vec<u8> with an 8-byte Anchor
/// discriminator prefix, exactly as Anchor programs store account data on-chain.
fn borsh_serialize_account(discriminator: [u8; 8], fields: &[BorshField]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&discriminator);
    for field in fields {
        field.serialize_into(&mut buf);
    }
    buf
}

/// Represents a typed field value for Borsh serialization, mirroring how real
/// Solana programs encode their account state.
enum BorshField {
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    I64(i64),
    Bool(bool),
    String(String),
    Pubkey([u8; 32]),
    VecU64(Vec<u64>),
}

impl BorshField {
    fn serialize_into(&self, buf: &mut Vec<u8>) {
        match self {
            BorshField::U8(v) => buf.push(*v),
            BorshField::U16(v) => buf.extend_from_slice(&v.to_le_bytes()),
            BorshField::U32(v) => buf.extend_from_slice(&v.to_le_bytes()),
            BorshField::U64(v) => buf.extend_from_slice(&v.to_le_bytes()),
            BorshField::I64(v) => buf.extend_from_slice(&v.to_le_bytes()),
            BorshField::Bool(v) => buf.push(if *v { 1 } else { 0 }),
            BorshField::String(s) => {
                buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
                buf.extend_from_slice(s.as_bytes());
            }
            BorshField::Pubkey(bytes) => buf.extend_from_slice(bytes),
            BorshField::VecU64(elems) => {
                buf.extend_from_slice(&(elems.len() as u32).to_le_bytes());
                for e in elems {
                    buf.extend_from_slice(&e.to_le_bytes());
                }
            }
        }
    }
}

/// Compute the Anchor 8-byte account discriminator the same way Anchor does:
/// SHA-256("account:<AccountName>")[..8].
fn anchor_discriminator(account_name: &str) -> [u8; 8] {
    // Minimal SHA-256 not available without a dep; use a fixed discriminator
    // for test purposes. Real programs compute SHA-256("account:<Name>")[..8],
    // but the decoder only skips these 8 bytes, so the value doesn't matter
    // for decoding correctness.
    let mut disc = [0u8; 8];
    let name_bytes = account_name.as_bytes();
    for (i, &b) in name_bytes.iter().take(8).enumerate() {
        disc[i] = b;
    }
    disc
}

// ---------------------------------------------------------------------------
// Test: GreetingAccount — the classic Solana "hello world" program account
// ---------------------------------------------------------------------------

#[test]
fn test_realistic_greeting_account() {
    // This mirrors the GreetingAccount struct from Solana's hello-world example:
    //   pub struct GreetingAccount { pub counter: u32 }
    let idl_json = r#"{
        "version": "0.1.0",
        "name": "hello_world",
        "accounts": [
            {
                "name": "GreetingAccount",
                "type": {
                    "kind": "struct",
                    "fields": [
                        { "name": "counter", "type": "u32" }
                    ]
                }
            }
        ]
    }"#;

    let idl = AnchorIdl::from_json(idl_json).unwrap();

    // Serialize as a real program would: discriminator + Borsh-encoded u32.
    let data = borsh_serialize_account(
        anchor_discriminator("GreetingAccount"),
        &[BorshField::U32(42)],
    );

    let fields = idl.decode_account("GreetingAccount", &data).unwrap();
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].name, "counter");
    assert_eq!(fields[0].type_name, "u32");
    match &fields[0].value {
        DecodedValue::U32(v) => assert_eq!(*v, 42),
        other => panic!("expected U32(42), got {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// Test: Token-like account with multiple field types
// ---------------------------------------------------------------------------

#[test]
fn test_realistic_token_account() {
    // Mirrors a simplified SPL Token account layout:
    //   pub struct TokenAccount {
    //       pub mint: Pubkey,       // 32 bytes
    //       pub owner: Pubkey,      // 32 bytes
    //       pub amount: u64,        // 8 bytes
    //       pub is_frozen: bool,    // 1 byte
    //   }
    let idl_json = r#"{
        "version": "0.1.0",
        "name": "spl_token",
        "accounts": [
            {
                "name": "TokenAccount",
                "type": {
                    "kind": "struct",
                    "fields": [
                        { "name": "mint", "type": "publicKey" },
                        { "name": "owner", "type": "publicKey" },
                        { "name": "amount", "type": "u64" },
                        { "name": "is_frozen", "type": "bool" }
                    ]
                }
            }
        ]
    }"#;

    let idl = AnchorIdl::from_json(idl_json).unwrap();

    // Realistic pubkeys: mint and owner addresses.
    let mut mint_pubkey = [0u8; 32];
    mint_pubkey[0] = 0x06; // Resembles Token program prefix
    mint_pubkey[1] = 0xDD;
    mint_pubkey[31] = 0x01;

    let mut owner_pubkey = [0u8; 32];
    owner_pubkey[0] = 0xAB;
    owner_pubkey[15] = 0xCD;
    owner_pubkey[31] = 0xEF;

    let amount: u64 = 1_000_000_000; // 1 SOL in lamports
    let is_frozen = false;

    let data = borsh_serialize_account(
        anchor_discriminator("TokenAccount"),
        &[
            BorshField::Pubkey(mint_pubkey),
            BorshField::Pubkey(owner_pubkey),
            BorshField::U64(amount),
            BorshField::Bool(is_frozen),
        ],
    );

    let fields = idl.decode_account("TokenAccount", &data).unwrap();
    assert_eq!(fields.len(), 4);

    // Verify mint pubkey.
    assert_eq!(fields[0].name, "mint");
    assert_eq!(fields[0].type_name, "publicKey");
    match &fields[0].value {
        DecodedValue::Pubkey(bytes) => {
            assert_eq!(bytes[0], 0x06);
            assert_eq!(bytes[1], 0xDD);
            assert_eq!(bytes[31], 0x01);
        }
        other => panic!("expected Pubkey, got {:?}", other),
    }

    // Verify owner pubkey.
    assert_eq!(fields[1].name, "owner");
    assert_eq!(fields[1].type_name, "publicKey");
    match &fields[1].value {
        DecodedValue::Pubkey(bytes) => {
            assert_eq!(bytes[0], 0xAB);
            assert_eq!(bytes[15], 0xCD);
            assert_eq!(bytes[31], 0xEF);
        }
        other => panic!("expected Pubkey, got {:?}", other),
    }

    // Verify amount (1 SOL = 1_000_000_000 lamports).
    assert_eq!(fields[2].name, "amount");
    assert_eq!(fields[2].type_name, "u64");
    match &fields[2].value {
        DecodedValue::U64(v) => assert_eq!(*v, 1_000_000_000),
        other => panic!("expected U64, got {:?}", other),
    }

    // Verify frozen flag.
    assert_eq!(fields[3].name, "is_frozen");
    assert_eq!(fields[3].type_name, "bool");
    match &fields[3].value {
        DecodedValue::Bool(v) => assert!(!v),
        other => panic!("expected Bool(false), got {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// Test: Escrow-like account with string + vec + multiple numeric types
// ---------------------------------------------------------------------------

#[test]
fn test_realistic_escrow_account() {
    // Mirrors a realistic escrow/marketplace program account:
    //   pub struct Escrow {
    //       pub seller: Pubkey,
    //       pub buyer: Pubkey,
    //       pub price: u64,
    //       pub item_count: u32,
    //       pub description: String,
    //       pub is_finalized: bool,
    //   }
    let idl_json = r#"{
        "version": "0.1.0",
        "name": "escrow",
        "accounts": [
            {
                "name": "Escrow",
                "type": {
                    "kind": "struct",
                    "fields": [
                        { "name": "seller", "type": "publicKey" },
                        { "name": "buyer", "type": "publicKey" },
                        { "name": "price", "type": "u64" },
                        { "name": "item_count", "type": "u32" },
                        { "name": "description", "type": "string" },
                        { "name": "is_finalized", "type": "bool" }
                    ]
                }
            }
        ]
    }"#;

    let idl = AnchorIdl::from_json(idl_json).unwrap();

    let mut seller = [0u8; 32];
    seller[0] = 0x11;
    seller[31] = 0x22;

    let mut buyer = [0u8; 32];
    buyer[0] = 0x33;
    buyer[31] = 0x44;

    let data = borsh_serialize_account(
        anchor_discriminator("Escrow"),
        &[
            BorshField::Pubkey(seller),
            BorshField::Pubkey(buyer),
            BorshField::U64(5_000_000_000), // 5 SOL
            BorshField::U32(3),
            BorshField::String("Rare NFT bundle".to_string()),
            BorshField::Bool(false),
        ],
    );

    let fields = idl.decode_account("Escrow", &data).unwrap();
    assert_eq!(fields.len(), 6);

    assert_eq!(fields[0].name, "seller");
    match &fields[0].value {
        DecodedValue::Pubkey(bytes) => assert_eq!(bytes[0], 0x11),
        other => panic!("expected Pubkey, got {:?}", other),
    }

    assert_eq!(fields[2].name, "price");
    match &fields[2].value {
        DecodedValue::U64(v) => assert_eq!(*v, 5_000_000_000),
        other => panic!("expected U64, got {:?}", other),
    }

    assert_eq!(fields[3].name, "item_count");
    match &fields[3].value {
        DecodedValue::U32(v) => assert_eq!(*v, 3),
        other => panic!("expected U32, got {:?}", other),
    }

    assert_eq!(fields[4].name, "description");
    assert_eq!(fields[4].type_name, "string");
    match &fields[4].value {
        DecodedValue::String(s) => assert_eq!(s, "Rare NFT bundle"),
        other => panic!("expected String, got {:?}", other),
    }

    assert_eq!(fields[5].name, "is_finalized");
    match &fields[5].value {
        DecodedValue::Bool(v) => assert!(!v),
        other => panic!("expected Bool(false), got {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// Test: Staking-like account with Vec<u64> field
// ---------------------------------------------------------------------------

#[test]
fn test_realistic_staking_account_with_vec() {
    // Mirrors a staking program that tracks deposit history:
    //   pub struct StakeAccount {
    //       pub owner: Pubkey,
    //       pub total_staked: u64,
    //       pub deposit_history: Vec<u64>,
    //       pub is_active: bool,
    //   }
    let idl_json = r#"{
        "version": "0.1.0",
        "name": "staking",
        "accounts": [
            {
                "name": "StakeAccount",
                "type": {
                    "kind": "struct",
                    "fields": [
                        { "name": "owner", "type": "publicKey" },
                        { "name": "total_staked", "type": "u64" },
                        { "name": "deposit_history", "type": { "vec": "u64" } },
                        { "name": "is_active", "type": "bool" }
                    ]
                }
            }
        ]
    }"#;

    let idl = AnchorIdl::from_json(idl_json).unwrap();

    let mut owner = [0u8; 32];
    owner[0] = 0xFF;

    let deposits: Vec<u64> = vec![1_000_000_000, 2_500_000_000, 500_000_000];
    let total: u64 = deposits.iter().sum();

    let data = borsh_serialize_account(
        anchor_discriminator("StakeAccount"),
        &[
            BorshField::Pubkey(owner),
            BorshField::U64(total),
            BorshField::VecU64(deposits.clone()),
            BorshField::Bool(true),
        ],
    );

    let fields = idl.decode_account("StakeAccount", &data).unwrap();
    assert_eq!(fields.len(), 4);

    // Verify total_staked matches sum of deposits.
    assert_eq!(fields[1].name, "total_staked");
    match &fields[1].value {
        DecodedValue::U64(v) => assert_eq!(*v, 4_000_000_000),
        other => panic!("expected U64, got {:?}", other),
    }

    // Verify deposit history Vec<u64>.
    assert_eq!(fields[2].name, "deposit_history");
    assert_eq!(fields[2].type_name, "Vec<u64>");
    match &fields[2].value {
        DecodedValue::Vec(elems) => {
            assert_eq!(elems.len(), 3);
            match &elems[0] {
                DecodedValue::U64(v) => assert_eq!(*v, 1_000_000_000),
                other => panic!("expected U64 in vec, got {:?}", other),
            }
            match &elems[1] {
                DecodedValue::U64(v) => assert_eq!(*v, 2_500_000_000),
                other => panic!("expected U64 in vec, got {:?}", other),
            }
            match &elems[2] {
                DecodedValue::U64(v) => assert_eq!(*v, 500_000_000),
                other => panic!("expected U64 in vec, got {:?}", other),
            }
        }
        other => panic!("expected Vec, got {:?}", other),
    }

    assert_eq!(fields[3].name, "is_active");
    match &fields[3].value {
        DecodedValue::Bool(v) => assert!(v),
        other => panic!("expected Bool(true), got {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// Test: Large values and boundary conditions
// ---------------------------------------------------------------------------

#[test]
fn test_realistic_large_values() {
    // Test with max/boundary values that real programs might store.
    let idl_json = r#"{
        "version": "0.1.0",
        "name": "vault",
        "accounts": [
            {
                "name": "Vault",
                "type": {
                    "kind": "struct",
                    "fields": [
                        { "name": "max_supply", "type": "u64" },
                        { "name": "min_balance", "type": "u64" },
                        { "name": "fee_bps", "type": "u16" },
                        { "name": "version", "type": "u8" },
                        { "name": "nonce", "type": "u8" },
                        { "name": "last_update", "type": "i64" }
                    ]
                }
            }
        ]
    }"#;

    let idl = AnchorIdl::from_json(idl_json).unwrap();

    let data = borsh_serialize_account(
        anchor_discriminator("Vault"),
        &[
            BorshField::U64(u64::MAX),       // max supply at u64::MAX
            BorshField::U64(0),              // zero balance
            BorshField::U16(250),            // 2.5% fee in basis points
            BorshField::U8(1),               // version 1
            BorshField::U8(255),             // nonce at u8::MAX
            BorshField::I64(-1_700_000_000), // negative Unix timestamp (testing i64)
        ],
    );

    let fields = idl.decode_account("Vault", &data).unwrap();
    assert_eq!(fields.len(), 6);

    // u64::MAX
    assert_eq!(fields[0].name, "max_supply");
    match &fields[0].value {
        DecodedValue::U64(v) => assert_eq!(*v, u64::MAX),
        other => panic!("expected U64(MAX), got {:?}", other),
    }

    // Zero.
    assert_eq!(fields[1].name, "min_balance");
    match &fields[1].value {
        DecodedValue::U64(v) => assert_eq!(*v, 0),
        other => panic!("expected U64(0), got {:?}", other),
    }

    // u16 fee.
    assert_eq!(fields[2].name, "fee_bps");
    match &fields[2].value {
        DecodedValue::U16(v) => assert_eq!(*v, 250),
        other => panic!("expected U16(250), got {:?}", other),
    }

    // u8 values.
    match &fields[3].value {
        DecodedValue::U8(v) => assert_eq!(*v, 1),
        other => panic!("expected U8(1), got {:?}", other),
    }
    match &fields[4].value {
        DecodedValue::U8(v) => assert_eq!(*v, 255),
        other => panic!("expected U8(255), got {:?}", other),
    }

    // Negative i64.
    assert_eq!(fields[5].name, "last_update");
    assert_eq!(fields[5].type_name, "i64");
    match &fields[5].value {
        DecodedValue::I64(v) => assert_eq!(*v, -1_700_000_000),
        other => panic!("expected I64(-1_700_000_000), got {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// Test: Empty/minimal account (only discriminator, zero fields)
// ---------------------------------------------------------------------------

#[test]
fn test_realistic_empty_account() {
    // Some programs have marker accounts with no data fields.
    let idl_json = r#"{
        "version": "0.1.0",
        "name": "marker",
        "accounts": [
            {
                "name": "Marker",
                "type": {
                    "kind": "struct",
                    "fields": []
                }
            }
        ]
    }"#;

    let idl = AnchorIdl::from_json(idl_json).unwrap();

    // Just the 8-byte discriminator, no field data.
    let data = borsh_serialize_account(anchor_discriminator("Marker"), &[]);

    let fields = idl.decode_account("Marker", &data).unwrap();
    assert_eq!(
        fields.len(),
        0,
        "empty account should decode to zero fields"
    );
}

// ---------------------------------------------------------------------------
// Test: Account data too short for declared fields
// ---------------------------------------------------------------------------

#[test]
fn test_realistic_truncated_account_data() {
    let idl_json = r#"{
        "version": "0.1.0",
        "name": "counter",
        "accounts": [
            {
                "name": "Counter",
                "type": {
                    "kind": "struct",
                    "fields": [
                        { "name": "count", "type": "u64" },
                        { "name": "bump", "type": "u8" }
                    ]
                }
            }
        ]
    }"#;

    let idl = AnchorIdl::from_json(idl_json).unwrap();

    // Only provide the discriminator + partial data (not enough for u64).
    let mut data = Vec::new();
    data.extend_from_slice(&anchor_discriminator("Counter"));
    data.extend_from_slice(&[0x01, 0x02, 0x03]); // only 3 bytes, u64 needs 8

    let result = idl.decode_account("Counter", &data);
    assert!(result.is_err(), "decoding truncated data should fail");
}

// ---------------------------------------------------------------------------
// Test: Account not found in IDL
// ---------------------------------------------------------------------------

#[test]
fn test_realistic_unknown_account_name() {
    let idl_json = r#"{
        "version": "0.1.0",
        "name": "my_program",
        "accounts": [
            {
                "name": "Known",
                "type": { "kind": "struct", "fields": [] }
            }
        ]
    }"#;

    let idl = AnchorIdl::from_json(idl_json).unwrap();
    let data = borsh_serialize_account(anchor_discriminator("Unknown"), &[]);

    let result = idl.decode_account("Unknown", &data);
    assert!(result.is_err(), "decoding unknown account name should fail");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("not found"),
        "error should mention 'not found', got: {err_msg}"
    );
}

// ---------------------------------------------------------------------------
// Test: Display formatting for realistic decoded values
// ---------------------------------------------------------------------------

#[test]
fn test_realistic_display_formatting() {
    // Verify that display() produces readable output for realistic values,
    // as would appear in CodeTracer trace output.

    let amount = DecodedValue::U64(1_500_000_000);
    assert_eq!(amount.display(), "1500000000");

    let label = DecodedValue::String("My NFT Collection".to_string());
    assert_eq!(label.display(), "\"My NFT Collection\"");

    let flag = DecodedValue::Bool(true);
    assert_eq!(flag.display(), "true");

    // Vec display.
    let prices = DecodedValue::Vec(vec![
        DecodedValue::U64(100),
        DecodedValue::U64(200),
        DecodedValue::U64(300),
    ]);
    assert_eq!(prices.display(), "[100, 200, 300]");

    // Empty vec.
    let empty_vec = DecodedValue::Vec(vec![]);
    assert_eq!(empty_vec.display(), "[]");

    // Pubkey display should produce a non-empty base58 string.
    let mut key = [0u8; 32];
    key[0] = 0x06;
    key[1] = 0xDD;
    let pubkey = DecodedValue::Pubkey(key);
    let display = pubkey.display();
    assert!(!display.is_empty(), "pubkey display should not be empty");
    // Base58 uses only alphanumeric chars from the base58 alphabet.
    assert!(
        display.chars().all(|c| c.is_ascii_alphanumeric()),
        "pubkey display should be base58: {display}"
    );
}

// ---------------------------------------------------------------------------
// Test: Multiple accounts in one IDL (program with several account types)
// ---------------------------------------------------------------------------

#[test]
fn test_realistic_multi_account_program() {
    // A realistic Anchor program often has several account types.
    // This tests that the decoder correctly resolves different accounts
    // from the same IDL.
    let idl_json = r#"{
        "version": "0.1.0",
        "name": "marketplace",
        "accounts": [
            {
                "name": "Listing",
                "type": {
                    "kind": "struct",
                    "fields": [
                        { "name": "seller", "type": "publicKey" },
                        { "name": "price", "type": "u64" },
                        { "name": "is_active", "type": "bool" }
                    ]
                }
            },
            {
                "name": "Bid",
                "type": {
                    "kind": "struct",
                    "fields": [
                        { "name": "bidder", "type": "publicKey" },
                        { "name": "listing", "type": "publicKey" },
                        { "name": "amount", "type": "u64" }
                    ]
                }
            }
        ]
    }"#;

    let idl = AnchorIdl::from_json(idl_json).unwrap();
    assert_eq!(idl.accounts.len(), 2);

    // Decode a Listing account.
    let mut seller = [0u8; 32];
    seller[0] = 0xAA;
    let listing_data = borsh_serialize_account(
        anchor_discriminator("Listing"),
        &[
            BorshField::Pubkey(seller),
            BorshField::U64(2_000_000_000),
            BorshField::Bool(true),
        ],
    );
    let listing_fields = idl.decode_account("Listing", &listing_data).unwrap();
    assert_eq!(listing_fields.len(), 3);
    assert_eq!(listing_fields[0].name, "seller");
    assert_eq!(listing_fields[1].name, "price");
    match &listing_fields[1].value {
        DecodedValue::U64(v) => assert_eq!(*v, 2_000_000_000),
        other => panic!("expected U64, got {:?}", other),
    }

    // Decode a Bid account from the same IDL.
    let mut bidder = [0u8; 32];
    bidder[0] = 0xBB;
    let mut listing_key = [0u8; 32];
    listing_key[0] = 0xCC;
    let bid_data = borsh_serialize_account(
        anchor_discriminator("Bid"),
        &[
            BorshField::Pubkey(bidder),
            BorshField::Pubkey(listing_key),
            BorshField::U64(1_500_000_000),
        ],
    );
    let bid_fields = idl.decode_account("Bid", &bid_data).unwrap();
    assert_eq!(bid_fields.len(), 3);
    assert_eq!(bid_fields[0].name, "bidder");
    assert_eq!(bid_fields[1].name, "listing");
    assert_eq!(bid_fields[2].name, "amount");
    match &bid_fields[2].value {
        DecodedValue::U64(v) => assert_eq!(*v, 1_500_000_000),
        other => panic!("expected U64, got {:?}", other),
    }
}
