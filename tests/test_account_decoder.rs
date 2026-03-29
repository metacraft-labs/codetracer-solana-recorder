//! Tests for the account decoder module (M6).

use codetracer_solana_recorder::account_decoder::{
    AnchorIdl, BorshDecoder, DecodedField, DecodedValue, TypeIds, decoded_to_value_record,
};
use codetracer_trace_types::ValueRecord;
use codetracer_trace_writer::{TraceEventsFileFormat, create_trace_writer};
use codetracer_trace_writer::trace_writer::TraceWriter;

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
    let mut writer = create_trace_writer("test", &[], TraceEventsFileFormat::Json);

    // We need to call start before registering types.
    let tmp = tempfile::tempdir().unwrap();
    let events_path = tmp.path().join("trace.bin");
    let meta_path = tmp.path().join("trace_metadata.json");
    let paths_path = tmp.path().join("trace_paths.json");

    TraceWriter::begin_writing_trace_events(&mut *writer, &events_path).unwrap();
    TraceWriter::begin_writing_trace_metadata(&mut *writer, &meta_path).unwrap();
    TraceWriter::begin_writing_trace_paths(&mut *writer, &paths_path).unwrap();

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
