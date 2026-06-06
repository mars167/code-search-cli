use codetrail::{
    project_graph::ProjectLanguage,
    scip,
    scip_proto::proto,
    semantic_facts::{
        write_scip_index, AliasEdge, FactLayer, FactReliability, InternalRange, LayeredFactStore,
        LayeredFactTables, OccurrenceRole, ProviderProof, ProviderRange, RangeEncoding,
        SemanticCallEdge, SemanticFact, SemanticOccurrence, SemanticSymbol, SymbolDescriptor,
        SymbolDescriptorKind, SymbolIdentity, SymbolKind, SymbolPackage,
    },
    semantic_provider::SemanticProviderVersion,
};
use prost::Message;
use tempfile::tempdir;

fn provider() -> SemanticProviderVersion {
    SemanticProviderVersion {
        name: "rust-analyzer".to_string(),
        version: "2026.06.01".to_string(),
        protocol_version: 1,
    }
}

fn proof(reliability: FactReliability) -> ProviderProof {
    ProviderProof {
        provider_id: "ra:core".to_string(),
        provider_version: provider(),
        reliability,
        evidence: "fixture".to_string(),
    }
}

fn package() -> SymbolPackage {
    SymbolPackage {
        manager: "cargo".to_string(),
        name: "core".to_string(),
        version: "0.1.0".to_string(),
    }
}

fn symbol(kind: SymbolKind, qualified_name: &[&str]) -> SemanticSymbol {
    let mut descriptors = Vec::new();
    for (index, name) in qualified_name.iter().enumerate() {
        let is_leaf = index + 1 == qualified_name.len();
        descriptors.push(SymbolDescriptor {
            name: name.to_string(),
            kind: if is_leaf {
                SymbolDescriptorKind::from_symbol_kind(&kind)
            } else {
                SymbolDescriptorKind::Namespace
            },
        });
    }
    symbol_with_descriptors(kind, descriptors)
}

fn symbol_with_descriptors(kind: SymbolKind, descriptors: Vec<SymbolDescriptor>) -> SemanticSymbol {
    let display_name = descriptors
        .last()
        .map(|descriptor| descriptor.name.clone())
        .unwrap_or_default();
    SemanticSymbol {
        identity: SymbolIdentity {
            language: ProjectLanguage::Rust,
            project_id: "rust:core".to_string(),
            package: package(),
            descriptors,
            signature: None,
            disambiguator: None,
            provider_version: provider(),
            generated: false,
            local_id: None,
        },
        kind,
        display_name,
        documentation: Vec::new(),
    }
}

fn occurrence_for(path: &str, language: ProjectLanguage, display_name: &str) -> SemanticOccurrence {
    let mut symbol = symbol(SymbolKind::Function, &[display_name]);
    symbol.identity.language = language;
    symbol.display_name = display_name.to_string();
    SemanticOccurrence {
        file_path: path.to_string(),
        range: InternalRange {
            start_line: 0,
            start_column: 0,
            end_line: 0,
            end_column: display_name.len() as u32,
        },
        role: OccurrenceRole::Definition,
        symbol,
        proof: proof(FactReliability::ProviderConfirmed),
    }
}

#[test]
fn symbol_identity_distinguishes_locals_fields_overloads_generics_aliases_and_generated() {
    let mut first_local = symbol(SymbolKind::LocalVariable, &["parse", "value"]);
    first_local.identity.local_id = Some("src_lib_rs_parse_value_1".to_string());
    first_local.identity.disambiguator = Some("scope:1".to_string());

    let mut second_local = symbol(SymbolKind::LocalVariable, &["parse", "value"]);
    second_local.identity.local_id = Some("src_lib_rs_parse_value_2".to_string());
    second_local.identity.disambiguator = Some("scope:2".to_string());

    let field = symbol(SymbolKind::Field, &["Config", "value"]);

    let mut int_overload = symbol(SymbolKind::Function, &["parse"]);
    int_overload.identity.signature = Some("fn parse(value: i32) -> Config".to_string());
    int_overload.identity.disambiguator = Some("i32".to_string());

    let mut str_overload = symbol(SymbolKind::Function, &["parse"]);
    str_overload.identity.signature = Some("fn parse(value: &str) -> Config".to_string());
    str_overload.identity.disambiguator = Some("str".to_string());

    let mut generic_alias = symbol(SymbolKind::TypeAlias, &["ResultAlias"]);
    generic_alias.identity.signature = Some("type ResultAlias<T> = Result<T, Error>".to_string());
    generic_alias.identity.disambiguator = Some("generic:T".to_string());
    let mut second_generic_alias = symbol(SymbolKind::TypeAlias, &["ResultAlias"]);
    second_generic_alias.identity.signature =
        Some("type ResultAlias<T, E> = Result<T, E>".to_string());
    second_generic_alias.identity.disambiguator = Some("generic:T,E".to_string());

    let mut generated = symbol(SymbolKind::Function, &["generated_helper"]);
    generated.identity.generated = true;

    let ids = [
        first_local.scip_symbol().unwrap(),
        second_local.scip_symbol().unwrap(),
        field.scip_symbol().unwrap(),
        int_overload.scip_symbol().unwrap(),
        str_overload.scip_symbol().unwrap(),
        generic_alias.scip_symbol().unwrap(),
        generated.scip_symbol().unwrap(),
    ];

    for id in ids {
        assert!(id.contains("rust"));
        assert!(id.contains("rust-analyzer"));
    }

    assert_ne!(
        first_local.scip_symbol().unwrap(),
        second_local.scip_symbol().unwrap()
    );
    assert_ne!(
        field.scip_symbol().unwrap(),
        first_local.scip_symbol().unwrap()
    );
    assert_ne!(
        int_overload.scip_symbol().unwrap(),
        str_overload.scip_symbol().unwrap()
    );
    assert_ne!(
        generic_alias.scip_symbol().unwrap(),
        second_generic_alias.scip_symbol().unwrap()
    );
    assert!(generic_alias.scip_symbol().unwrap().contains("ResultAlias"));
    assert!(generated.scip_symbol().unwrap().contains("generated"));
}

#[test]
fn ambiguous_local_symbol_cannot_be_used_as_precise_occurrence() {
    let ambiguous = symbol(SymbolKind::LocalVariable, &["parse", "value"]);

    assert!(ambiguous.scip_symbol().is_err());
}

#[test]
fn symbol_identity_preserves_container_descriptor_kind_and_rejects_empty_components() {
    let module_parent = symbol_with_descriptors(
        SymbolKind::Function,
        vec![
            SymbolDescriptor {
                name: "Foo".to_string(),
                kind: SymbolDescriptorKind::Namespace,
            },
            SymbolDescriptor {
                name: "bar".to_string(),
                kind: SymbolDescriptorKind::Method,
            },
        ],
    );
    let type_parent = symbol_with_descriptors(
        SymbolKind::Function,
        vec![
            SymbolDescriptor {
                name: "Foo".to_string(),
                kind: SymbolDescriptorKind::Type,
            },
            SymbolDescriptor {
                name: "bar".to_string(),
                kind: SymbolDescriptorKind::Method,
            },
        ],
    );

    let module_symbol = module_parent.scip_symbol().unwrap();
    let type_symbol = type_parent.scip_symbol().unwrap();
    assert_ne!(module_symbol, type_symbol);
    assert!(module_symbol.contains("Foo/bar"));
    assert!(type_symbol.contains("Foo#bar"));

    let empty_descriptor = symbol_with_descriptors(
        SymbolKind::Function,
        vec![SymbolDescriptor {
            name: "".to_string(),
            kind: SymbolDescriptorKind::Method,
        }],
    );
    assert!(empty_descriptor.scip_symbol().is_err());

    let mut empty_local = symbol(SymbolKind::LocalVariable, &["value"]);
    empty_local.identity.local_id = Some("".to_string());
    assert!(empty_local.scip_symbol().is_err());

    let mut empty_disambiguator = symbol(SymbolKind::Function, &["parse"]);
    empty_disambiguator.identity.disambiguator = Some("".to_string());
    assert!(empty_disambiguator.scip_symbol().is_err());
}

#[test]
fn range_conversion_normalizes_utf8_utf16_and_lsp_positions() {
    let source = "α🚀beta\nnext\n";
    let expected = InternalRange {
        start_line: 0,
        start_column: 6,
        end_line: 0,
        end_column: 10,
    };

    let utf8 = ProviderRange {
        start_line: 0,
        start_character: 6,
        end_line: 0,
        end_character: 10,
        encoding: RangeEncoding::Utf8ByteOffset,
    };
    let utf16 = ProviderRange {
        start_line: 0,
        start_character: 3,
        end_line: 0,
        end_character: 7,
        encoding: RangeEncoding::Utf16CodeUnit,
    };
    let lsp = ProviderRange {
        start_line: 0,
        start_character: 3,
        end_line: 0,
        end_character: 7,
        encoding: RangeEncoding::LspUtf16,
    };

    assert_eq!(utf8.to_internal_range(source).unwrap(), expected);
    assert_eq!(utf16.to_internal_range(source).unwrap(), expected);
    assert_eq!(lsp.to_internal_range(source).unwrap(), expected);
    assert_eq!(expected.to_scip_range(), vec![0, 6, 10]);
}

#[test]
fn scip_writer_round_trips_only_provider_confirmed_precise_occurrences() {
    let parse = symbol(SymbolKind::Function, &["parse"]);
    let fallback = symbol(SymbolKind::Function, &["fallback_only"]);
    let facts = vec![
        SemanticOccurrence {
            file_path: "src/lib.rs".to_string(),
            range: InternalRange {
                start_line: 0,
                start_column: 3,
                end_line: 0,
                end_column: 8,
            },
            role: OccurrenceRole::Definition,
            symbol: parse.clone(),
            proof: proof(FactReliability::ProviderConfirmed),
        },
        SemanticOccurrence {
            file_path: "src/lib.rs".to_string(),
            range: InternalRange {
                start_line: 1,
                start_column: 11,
                end_line: 1,
                end_column: 16,
            },
            role: OccurrenceRole::Reference,
            symbol: parse.clone(),
            proof: proof(FactReliability::ProviderConfirmed),
        },
        SemanticOccurrence {
            file_path: "src/lib.rs".to_string(),
            range: InternalRange {
                start_line: 2,
                start_column: 0,
                end_line: 2,
                end_column: 13,
            },
            role: OccurrenceRole::Definition,
            symbol: fallback,
            proof: proof(FactReliability::ParserFallback),
        },
    ];

    let index = write_scip_index(&facts, "file:///workspace").unwrap();
    assert_eq!(index.documents.len(), 1);
    assert_eq!(index.documents[0].text, "");
    assert_eq!(
        index.documents[0].position_encoding,
        proto::PositionEncoding::Utf8CodeUnitOffsetFromLineStart as i32
    );
    assert_eq!(index.documents[0].occurrences.len(), 2);
    assert_eq!(index.documents[0].symbols.len(), 1);
    assert_eq!(index.documents[0].symbols[0].display_name, "parse");

    let mut buf = Vec::new();
    index.encode(&mut buf).unwrap();
    let decoded = scip::parser::parse_native_scip_from_bytes(&buf).unwrap();
    assert_eq!(decoded.documents[0].occurrences.len(), 2);

    let dir = tempdir().unwrap();
    let db_path = dir.path().join("occurrences.db");
    scip::build_occurrences_db(&decoded, &db_path, "snapshot-v1", dir.path()).unwrap();

    let defs = scip::query_defs(&db_path, "parse").unwrap();
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].role, "definition");
    assert!(scip::query_defs(&db_path, "fallback_only")
        .unwrap()
        .is_empty());
}

#[test]
fn scip_writer_rejects_invalid_relative_paths_and_mixed_language_documents() {
    for path in [
        "",
        "/abs.rs",
        "src/../lib.rs",
        "src\\lib.rs",
        "src//lib.rs",
        "./src/lib.rs",
        "src/./lib.rs",
    ] {
        let err = write_scip_index(
            &[occurrence_for(path, ProjectLanguage::Rust, "parse")],
            "file:///workspace",
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("relative_path"),
            "unexpected error for {path:?}: {err}"
        );
    }

    let mixed_language = vec![
        occurrence_for("src/lib.rs", ProjectLanguage::Rust, "parse"),
        occurrence_for("src/lib.rs", ProjectLanguage::TypeScript, "parseTs"),
    ];
    let err = write_scip_index(&mixed_language, "file:///workspace").unwrap_err();
    assert!(err.to_string().contains("mixed languages"));

    let same_language = vec![
        occurrence_for("src/lib.rs", ProjectLanguage::Rust, "parse"),
        occurrence_for("src/lib.rs", ProjectLanguage::Rust, "format"),
    ];
    let index = write_scip_index(&same_language, "file:///workspace").unwrap();
    assert_eq!(index.documents.len(), 1);
    assert_eq!(index.documents[0].relative_path, "src/lib.rs");
    assert_eq!(index.documents[0].occurrences.len(), 2);
}

#[test]
fn alias_and_call_edges_carry_provider_proof_without_becoming_scip_occurrences() {
    let caller = symbol(SymbolKind::Function, &["main"]);
    let callee = symbol(SymbolKind::Function, &["parse"]);
    let alias = symbol(SymbolKind::ImportAlias, &["parse_config"]);

    let call = SemanticCallEdge {
        caller: caller.clone(),
        callee: callee.clone(),
        call_site: InternalRange {
            start_line: 4,
            start_column: 8,
            end_line: 4,
            end_column: 13,
        },
        proof: proof(FactReliability::ProviderConfirmed),
    };
    let alias_edge = AliasEdge {
        alias,
        target: callee,
        import_path: Some("crate::parse".to_string()),
        proof: proof(FactReliability::ProviderConfirmed),
    };

    assert!(call.proof.reliability.is_provider_confirmed());
    assert!(alias_edge.proof.reliability.is_provider_confirmed());
    assert_eq!(alias_edge.import_path.as_deref(), Some("crate::parse"));
}

#[test]
fn layered_fact_store_keeps_precise_parser_config_and_source_buckets_separate() {
    let precise = SemanticOccurrence {
        file_path: "src/lib.rs".to_string(),
        range: InternalRange {
            start_line: 0,
            start_column: 3,
            end_line: 0,
            end_column: 8,
        },
        role: OccurrenceRole::Definition,
        symbol: symbol(SymbolKind::Function, &["parse"]),
        proof: proof(FactReliability::ProviderConfirmed),
    };
    let parser = SemanticOccurrence {
        file_path: "src/lib.rs".to_string(),
        range: InternalRange {
            start_line: 2,
            start_column: 4,
            end_line: 2,
            end_column: 12,
        },
        role: OccurrenceRole::Reference,
        symbol: symbol(SymbolKind::Function, &["fallback_parse"]),
        proof: proof(FactReliability::ParserFallback),
    };
    let config = AliasEdge {
        alias: symbol(SymbolKind::ImportAlias, &["parse_alias"]),
        target: symbol(SymbolKind::Function, &["parse"]),
        import_path: Some("config:aliases.parse_alias".to_string()),
        proof: proof(FactReliability::ConfigFact),
    };
    let source = SemanticCallEdge {
        caller: symbol(SymbolKind::Function, &["main"]),
        callee: symbol(SymbolKind::Function, &["parse"]),
        call_site: InternalRange {
            start_line: 9,
            start_column: 8,
            end_line: 9,
            end_column: 13,
        },
        proof: proof(FactReliability::SourceFact),
    };

    let store = LayeredFactStore::from_facts([
        SemanticFact::Occurrence(precise.clone()),
        SemanticFact::Occurrence(parser.clone()),
        SemanticFact::Alias(config.clone()),
        SemanticFact::Call(source.clone()),
    ]);
    let tables = store.tables();

    assert_eq!(tables.precise_provider_facts.len(), 1);
    assert_eq!(tables.parser_facts.len(), 1);
    assert_eq!(tables.config_facts.len(), 1);
    assert_eq!(tables.source_facts.len(), 1);
    assert_eq!(
        tables.table(FactLayer::PreciseProvider),
        &[SemanticFact::Occurrence(precise.clone())]
    );
    assert_eq!(
        tables.table(FactLayer::Parser),
        &[SemanticFact::Occurrence(parser.clone())]
    );
    assert_eq!(
        tables.table(FactLayer::Config),
        &[SemanticFact::Alias(config)]
    );
    assert_eq!(
        tables.table(FactLayer::Source),
        &[SemanticFact::Call(source)]
    );

    let encoded = serde_json::to_string(tables).unwrap();
    let decoded: LayeredFactTables = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded.table(FactLayer::Parser).len(), 1);
    assert_eq!(decoded.table(FactLayer::Config).len(), 1);
    assert_eq!(decoded.table(FactLayer::Source).len(), 1);

    let mut polluted = LayeredFactTables::default();
    polluted
        .precise_provider_facts
        .push(SemanticFact::Occurrence(parser));
    assert!(polluted.precise_occurrences().is_empty());

    let precise_occurrences = tables.precise_occurrences();
    let index = write_scip_index(&precise_occurrences, "file:///workspace").unwrap();
    assert_eq!(index.documents[0].occurrences.len(), 1);
    assert_eq!(index.documents[0].symbols[0].display_name, "parse");
}
