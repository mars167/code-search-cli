pub mod parser;
pub mod store;

pub use parser::parse_native_scip;
pub use store::{
    build_occurrences_db, invalidate_db, occurrence_db_fresh, occurrence_to_json, query_defs,
    query_refs, query_refs_by_symbol_key, query_symbols, symbol_to_json,
};

/// Write a minimal SCIP test index (protobuf) to the given path for integration tests.
#[doc(hidden)]
pub fn write_minimal_test_index(path: &std::path::Path) -> anyhow::Result<()> {
    use crate::scip_proto::proto;
    use prost::Message;

    let index = proto::Index {
        metadata: Some(proto::Metadata {
            version: proto::ProtocolVersion::UnspecifiedProtocolVersion as i32,
            tool_info: Some(proto::ToolInfo {
                name: "test-indexer".to_string(),
                version: "0.1.0".to_string(),
                arguments: vec![],
            }),
            project_root: "file:///test".to_string(),
            text_document_encoding: proto::TextEncoding::Utf8 as i32,
        }),
        documents: vec![proto::Document {
            language: "rust".to_string(),
            relative_path: "src/lib.rs".to_string(),
            occurrences: vec![
                proto::Occurrence {
                    range: vec![0, 3, 0, 9],
                    symbol: "local 1".to_string(),
                    symbol_roles: 1, // Definition
                    ..Default::default()
                },
                proto::Occurrence {
                    range: vec![1, 12, 1, 18],
                    symbol: "local 1".to_string(),
                    symbol_roles: 0, // Reference
                    ..Default::default()
                },
            ],
            symbols: vec![proto::SymbolInformation {
                symbol: "local 1".to_string(),
                kind: proto::symbol_information::Kind::Function as i32,
                display_name: "needle".to_string(),
                ..Default::default()
            }],
            position_encoding: proto::PositionEncoding::Utf8CodeUnitOffsetFromLineStart as i32,
            ..Default::default()
        }],
        ..Default::default()
    };

    let mut buf = Vec::new();
    index.encode(&mut buf)?;
    std::fs::write(path, &buf)?;
    Ok(())
}
