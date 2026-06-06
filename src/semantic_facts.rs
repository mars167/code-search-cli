//! Normalized semantic facts and precise SCIP writing.
//!
//! Language providers feed CodeTrail facts through this module before anything
//! is allowed into a precise SCIP index. Parser/config/source facts remain
//! represented as facts, but `write_scip_index` only serializes provider
//! confirmed occurrences.

use std::collections::{btree_map::Entry, BTreeMap, BTreeSet};

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};

use crate::{
    project_graph::ProjectLanguage, scip_proto::proto, semantic_provider::SemanticProviderVersion,
};

const ROLE_DEFINITION: i32 = 0x1;
const ROLE_IMPORT: i32 = 0x2;
const ROLE_WRITE_ACCESS: i32 = 0x4;
const ROLE_READ_ACCESS: i32 = 0x8;
const ROLE_GENERATED: i32 = 0x10;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolPackage {
    pub manager: String,
    pub name: String,
    pub version: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    Function,
    Method,
    Constructor,
    Class,
    Struct,
    Interface,
    Enum,
    Trait,
    TypeAlias,
    Module,
    Namespace,
    Field,
    Variable,
    LocalVariable,
    Parameter,
    TypeParameter,
    ImportAlias,
    Constant,
    Property,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolDescriptorKind {
    Namespace,
    Type,
    Term,
    Method,
    TypeParameter,
    Parameter,
    Meta,
    Macro,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolDescriptor {
    pub name: String,
    pub kind: SymbolDescriptorKind,
}

impl SymbolDescriptorKind {
    pub fn from_symbol_kind(kind: &SymbolKind) -> Self {
        match kind {
            SymbolKind::Function | SymbolKind::Method | SymbolKind::Constructor => {
                SymbolDescriptorKind::Method
            }
            SymbolKind::Class
            | SymbolKind::Struct
            | SymbolKind::Interface
            | SymbolKind::Enum
            | SymbolKind::Trait
            | SymbolKind::TypeAlias => SymbolDescriptorKind::Type,
            SymbolKind::Module | SymbolKind::Namespace => SymbolDescriptorKind::Namespace,
            SymbolKind::Parameter => SymbolDescriptorKind::Parameter,
            SymbolKind::TypeParameter => SymbolDescriptorKind::TypeParameter,
            SymbolKind::ImportAlias => SymbolDescriptorKind::Meta,
            SymbolKind::Field
            | SymbolKind::Variable
            | SymbolKind::LocalVariable
            | SymbolKind::Constant
            | SymbolKind::Property
            | SymbolKind::Unknown => SymbolDescriptorKind::Term,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolIdentity {
    pub language: ProjectLanguage,
    pub project_id: String,
    pub package: SymbolPackage,
    pub descriptors: Vec<SymbolDescriptor>,
    pub signature: Option<String>,
    pub disambiguator: Option<String>,
    pub provider_version: SemanticProviderVersion,
    pub generated: bool,
    pub local_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticSymbol {
    pub identity: SymbolIdentity,
    pub kind: SymbolKind,
    pub display_name: String,
    pub documentation: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OccurrenceRole {
    Definition,
    Reference,
    Import,
    Write,
    Read,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactReliability {
    ProviderConfirmed,
    ParserFallback,
    ConfigFact,
    SourceFact,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactLayer {
    PreciseProvider,
    Parser,
    Config,
    Source,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderProof {
    pub provider_id: String,
    pub provider_version: SemanticProviderVersion,
    pub reliability: FactReliability,
    pub evidence: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RangeEncoding {
    Utf8ByteOffset,
    Utf16CodeUnit,
    LspUtf16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRange {
    pub start_line: u32,
    pub start_character: u32,
    pub end_line: u32,
    pub end_character: u32,
    pub encoding: RangeEncoding,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InternalRange {
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticOccurrence {
    pub file_path: String,
    pub range: InternalRange,
    pub role: OccurrenceRole,
    pub symbol: SemanticSymbol,
    pub proof: ProviderProof,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticCallEdge {
    pub caller: SemanticSymbol,
    pub callee: SemanticSymbol,
    pub call_site: InternalRange,
    pub proof: ProviderProof,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AliasEdge {
    pub alias: SemanticSymbol,
    pub target: SemanticSymbol,
    pub import_path: Option<String>,
    pub proof: ProviderProof,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum SemanticFact {
    Occurrence(SemanticOccurrence),
    Call(SemanticCallEdge),
    Alias(AliasEdge),
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LayeredFactTables {
    pub precise_provider_facts: Vec<SemanticFact>,
    pub parser_facts: Vec<SemanticFact>,
    pub config_facts: Vec<SemanticFact>,
    pub source_facts: Vec<SemanticFact>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LayeredFactStore {
    tables: LayeredFactTables,
}

impl SemanticFact {
    pub fn proof(&self) -> &ProviderProof {
        match self {
            SemanticFact::Occurrence(fact) => &fact.proof,
            SemanticFact::Call(fact) => &fact.proof,
            SemanticFact::Alias(fact) => &fact.proof,
        }
    }

    pub fn layer(&self) -> FactLayer {
        self.proof().reliability.layer()
    }
}

impl LayeredFactTables {
    pub fn table(&self, layer: FactLayer) -> &[SemanticFact] {
        match layer {
            FactLayer::PreciseProvider => &self.precise_provider_facts,
            FactLayer::Parser => &self.parser_facts,
            FactLayer::Config => &self.config_facts,
            FactLayer::Source => &self.source_facts,
        }
    }

    pub fn precise_occurrences(&self) -> Vec<SemanticOccurrence> {
        self.precise_provider_facts
            .iter()
            .filter_map(|fact| match fact {
                SemanticFact::Occurrence(occurrence)
                    if occurrence.proof.reliability.is_provider_confirmed() =>
                {
                    Some(occurrence.clone())
                }
                SemanticFact::Call(_) | SemanticFact::Alias(_) => None,
                SemanticFact::Occurrence(_) => None,
            })
            .collect()
    }
}

impl LayeredFactStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_facts(facts: impl IntoIterator<Item = SemanticFact>) -> Self {
        let mut store = Self::new();
        store.extend(facts);
        store
    }

    pub fn insert(&mut self, fact: SemanticFact) {
        match fact.layer() {
            FactLayer::PreciseProvider => self.tables.precise_provider_facts.push(fact),
            FactLayer::Parser => self.tables.parser_facts.push(fact),
            FactLayer::Config => self.tables.config_facts.push(fact),
            FactLayer::Source => self.tables.source_facts.push(fact),
        }
    }

    pub fn extend(&mut self, facts: impl IntoIterator<Item = SemanticFact>) {
        for fact in facts {
            self.insert(fact);
        }
    }

    pub fn tables(&self) -> &LayeredFactTables {
        &self.tables
    }

    pub fn into_tables(self) -> LayeredFactTables {
        self.tables
    }
}

pub fn split_facts_by_layer(facts: impl IntoIterator<Item = SemanticFact>) -> LayeredFactTables {
    LayeredFactStore::from_facts(facts).into_tables()
}

impl FactReliability {
    pub fn is_provider_confirmed(&self) -> bool {
        matches!(self, FactReliability::ProviderConfirmed)
    }

    pub fn layer(&self) -> FactLayer {
        match self {
            FactReliability::ProviderConfirmed => FactLayer::PreciseProvider,
            FactReliability::ParserFallback => FactLayer::Parser,
            FactReliability::ConfigFact => FactLayer::Config,
            FactReliability::SourceFact => FactLayer::Source,
        }
    }
}

impl SemanticSymbol {
    pub fn scip_symbol(&self) -> Result<String> {
        self.validate_precise_identity()?;

        let identity = &self.identity;
        let provider = &identity.provider_version;
        let manager = scip_package_component(&identity.package.manager);
        let package_name = scip_package_component(&format!(
            "{}:{}:{}",
            identity.language, identity.project_id, identity.package.name
        ));
        let package_version = scip_package_component(&format!(
            "{}@{}:{}:p{}",
            identity.package.version, provider.name, provider.version, provider.protocol_version
        ));

        let mut descriptors = Vec::new();
        descriptors.push(format!(
            "{}/",
            scip_identifier(&format!("project:{}", identity.project_id))
        ));
        for descriptor in &identity.descriptors {
            descriptors.push(descriptor.scip_descriptor(&self.identity));
        }
        if let Some(local_id) = &identity.local_id {
            descriptors.push(format!(
                "{}:",
                scip_identifier(&format!("local:{}", local_id))
            ));
        }
        if let Some(disambiguator) = &identity.disambiguator {
            descriptors.push(format!(
                "{}:",
                scip_identifier(&format!(
                    "disambiguator:{}",
                    stable_disambiguator(disambiguator)
                ))
            ));
        }
        if let Some(signature) = &identity.signature {
            descriptors.push(format!(
                "{}:",
                scip_identifier(&format!("signature:{}", stable_disambiguator(signature)))
            ));
        }
        if identity.generated {
            descriptors.push("generated:".to_string());
        }

        Ok(format!(
            "codetrail {} {} {} {}",
            manager,
            package_name,
            package_version,
            descriptors.join("")
        ))
    }

    fn validate_precise_identity(&self) -> Result<()> {
        let identity = &self.identity;
        if identity.project_id.trim().is_empty() {
            bail!("semantic symbol is missing project identity");
        }
        if identity.descriptors.is_empty() {
            bail!("semantic symbol is missing qualified name");
        }
        for descriptor in &identity.descriptors {
            if descriptor.name.trim().is_empty() {
                bail!("semantic symbol contains an empty descriptor name");
            }
        }
        if matches!(identity.local_id.as_deref(), Some(local_id) if local_id.trim().is_empty()) {
            bail!("semantic symbol contains an empty local_id");
        }
        if matches!(identity.disambiguator.as_deref(), Some(disambiguator) if disambiguator.trim().is_empty())
        {
            bail!("semantic symbol contains an empty disambiguator");
        }
        if identity.provider_version.name.trim().is_empty()
            || identity.provider_version.version.trim().is_empty()
        {
            bail!("semantic symbol is missing provider version");
        }
        if matches!(self.kind, SymbolKind::LocalVariable | SymbolKind::Parameter)
            && identity.local_id.is_none()
            && identity.disambiguator.is_none()
        {
            bail!("local semantic symbol requires local_id or disambiguator");
        }
        Ok(())
    }
}

impl SymbolDescriptor {
    fn scip_descriptor(&self, identity: &SymbolIdentity) -> String {
        let escaped_name = scip_identifier(&self.name);
        match self.kind {
            SymbolDescriptorKind::Namespace => format!("{}/", escaped_name),
            SymbolDescriptorKind::Type => format!("{}#", escaped_name),
            SymbolDescriptorKind::Term => format!("{}.", escaped_name),
            SymbolDescriptorKind::Method => method_descriptor(&escaped_name, identity),
            SymbolDescriptorKind::TypeParameter => format!("[{}]", escaped_name),
            SymbolDescriptorKind::Parameter => format!("({})", escaped_name),
            SymbolDescriptorKind::Meta => format!("{}:", escaped_name),
            SymbolDescriptorKind::Macro => format!("{}!", escaped_name),
        }
    }
}

impl ProviderRange {
    pub fn to_internal_range(&self, source: &str) -> Result<InternalRange> {
        let start_column = provider_character_to_utf8(
            source,
            self.start_line,
            self.start_character,
            self.encoding,
        )?;
        let end_column =
            provider_character_to_utf8(source, self.end_line, self.end_character, self.encoding)?;
        let range = InternalRange {
            start_line: self.start_line,
            start_column,
            end_line: self.end_line,
            end_column,
        };
        range.validate()?;
        Ok(range)
    }
}

impl InternalRange {
    pub fn to_scip_range(&self) -> Vec<i32> {
        if self.start_line == self.end_line {
            vec![
                self.start_line as i32,
                self.start_column as i32,
                self.end_column as i32,
            ]
        } else {
            vec![
                self.start_line as i32,
                self.start_column as i32,
                self.end_line as i32,
                self.end_column as i32,
            ]
        }
    }

    fn validate(&self) -> Result<()> {
        if self.start_line > self.end_line
            || (self.start_line == self.end_line && self.start_column > self.end_column)
        {
            bail!("semantic range end precedes start");
        }
        Ok(())
    }
}

pub fn write_scip_index(
    occurrences: &[SemanticOccurrence],
    project_root: &str,
) -> Result<proto::Index> {
    let mut documents: BTreeMap<String, (String, Vec<&SemanticOccurrence>)> = BTreeMap::new();
    for occurrence in occurrences {
        if !occurrence.proof.reliability.is_provider_confirmed() {
            continue;
        }
        occurrence.range.validate()?;
        let path = validate_scip_relative_path(&occurrence.file_path)?;
        let language = occurrence.symbol.identity.language.to_string();
        match documents.entry(path) {
            Entry::Vacant(entry) => {
                entry.insert((language, vec![occurrence]));
            }
            Entry::Occupied(mut entry) => {
                let relative_path = entry.key().clone();
                let (existing_language, facts) = entry.get_mut();
                if existing_language != &language {
                    bail!(
                        "SCIP document relative_path {} has mixed languages: {} and {}",
                        relative_path,
                        existing_language,
                        language
                    );
                }
                facts.push(occurrence);
            }
        }
    }

    let documents = documents
        .into_iter()
        .map(|(relative_path, (language, mut facts))| {
            facts.sort_by_key(|fact| {
                (
                    fact.range.start_line,
                    fact.range.start_column,
                    fact.range.end_line,
                    fact.range.end_column,
                    fact.symbol.display_name.clone(),
                )
            });

            let mut symbols = BTreeMap::new();
            let mut seen_occurrences = BTreeSet::new();
            let mut scip_occurrences = Vec::new();

            for fact in facts {
                let symbol = fact.symbol.scip_symbol()?;
                let range = fact.range.to_scip_range();
                let occurrence_key = (range.clone(), symbol.clone(), fact.role.clone());
                if !seen_occurrences.insert(occurrence_key) {
                    continue;
                }

                symbols
                    .entry(symbol.clone())
                    .or_insert_with(|| proto::SymbolInformation {
                        symbol: symbol.clone(),
                        documentation: fact.symbol.documentation.clone(),
                        relationships: Vec::new(),
                        kind: symbol_kind_to_scip(&fact.symbol.kind) as i32,
                        display_name: fact.symbol.display_name.clone(),
                        signature_documentation: fact.symbol.identity.signature.as_ref().map(
                            |signature| proto::Signature {
                                language: language.clone(),
                                text: signature.clone(),
                                occurrences: Vec::new(),
                            },
                        ),
                        enclosing_symbol: String::new(),
                    });

                scip_occurrences.push(proto::Occurrence {
                    range,
                    symbol,
                    symbol_roles: symbol_roles(fact),
                    syntax_kind: syntax_kind(fact) as i32,
                    ..Default::default()
                });
            }

            Ok(proto::Document {
                relative_path,
                occurrences: scip_occurrences,
                symbols: symbols.into_values().collect(),
                language,
                text: String::new(),
                position_encoding: proto::PositionEncoding::Utf8CodeUnitOffsetFromLineStart as i32,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(proto::Index {
        metadata: Some(proto::Metadata {
            version: proto::ProtocolVersion::UnspecifiedProtocolVersion as i32,
            tool_info: Some(proto::ToolInfo {
                name: "codetrail-semantic-facts".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                arguments: Vec::new(),
            }),
            project_root: project_root.to_string(),
            text_document_encoding: proto::TextEncoding::Utf8 as i32,
        }),
        documents,
        external_symbols: Vec::new(),
    })
}

fn symbol_roles(fact: &SemanticOccurrence) -> i32 {
    let mut roles = match fact.role {
        OccurrenceRole::Definition => ROLE_DEFINITION,
        OccurrenceRole::Reference => 0,
        OccurrenceRole::Import => ROLE_IMPORT,
        OccurrenceRole::Write => ROLE_WRITE_ACCESS,
        OccurrenceRole::Read => ROLE_READ_ACCESS,
    };
    if fact.symbol.identity.generated {
        roles |= ROLE_GENERATED;
    }
    roles
}

fn validate_scip_relative_path(path: &str) -> Result<String> {
    if path.is_empty() {
        bail!("SCIP Document.relative_path must be non-empty");
    }
    if path.starts_with('/') || path.as_bytes().get(1) == Some(&b':') {
        bail!("SCIP Document.relative_path must be relative: {}", path);
    }
    if path.contains('\\') {
        bail!(
            "SCIP Document.relative_path must use '/' separators: {}",
            path
        );
    }
    for component in path.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            bail!(
                "SCIP Document.relative_path must be canonical without empty, '.', or '..' components: {}",
                path
            );
        }
    }
    Ok(path.to_string())
}

fn symbol_kind_to_scip(kind: &SymbolKind) -> proto::symbol_information::Kind {
    match kind {
        SymbolKind::Function => proto::symbol_information::Kind::Function,
        SymbolKind::Method => proto::symbol_information::Kind::Method,
        SymbolKind::Constructor => proto::symbol_information::Kind::Constructor,
        SymbolKind::Class => proto::symbol_information::Kind::Class,
        SymbolKind::Struct => proto::symbol_information::Kind::Struct,
        SymbolKind::Interface => proto::symbol_information::Kind::Interface,
        SymbolKind::Enum => proto::symbol_information::Kind::Enum,
        SymbolKind::Trait => proto::symbol_information::Kind::Trait,
        SymbolKind::TypeAlias => proto::symbol_information::Kind::TypeAlias,
        SymbolKind::Module => proto::symbol_information::Kind::Module,
        SymbolKind::Namespace => proto::symbol_information::Kind::Namespace,
        SymbolKind::Field => proto::symbol_information::Kind::Field,
        SymbolKind::Variable | SymbolKind::LocalVariable => {
            proto::symbol_information::Kind::Variable
        }
        SymbolKind::Parameter => proto::symbol_information::Kind::Parameter,
        SymbolKind::TypeParameter => proto::symbol_information::Kind::TypeParameter,
        SymbolKind::ImportAlias => proto::symbol_information::Kind::MethodAlias,
        SymbolKind::Constant => proto::symbol_information::Kind::Constant,
        SymbolKind::Property => proto::symbol_information::Kind::Property,
        SymbolKind::Unknown => proto::symbol_information::Kind::UnspecifiedKind,
    }
}

fn syntax_kind(fact: &SemanticOccurrence) -> proto::SyntaxKind {
    match (&fact.role, &fact.symbol.kind) {
        (OccurrenceRole::Definition, SymbolKind::Function | SymbolKind::Method) => {
            proto::SyntaxKind::IdentifierFunctionDefinition
        }
        (_, SymbolKind::Function | SymbolKind::Method) => proto::SyntaxKind::IdentifierFunction,
        (_, SymbolKind::Parameter) => proto::SyntaxKind::IdentifierParameter,
        (_, SymbolKind::LocalVariable) => proto::SyntaxKind::IdentifierLocal,
        (_, SymbolKind::TypeAlias | SymbolKind::Class | SymbolKind::Struct | SymbolKind::Trait) => {
            proto::SyntaxKind::IdentifierType
        }
        (_, SymbolKind::Module | SymbolKind::Namespace) => proto::SyntaxKind::IdentifierNamespace,
        _ => proto::SyntaxKind::Identifier,
    }
}

fn method_descriptor(name: &str, identity: &SymbolIdentity) -> String {
    let disambiguator = identity
        .disambiguator
        .as_deref()
        .or(identity.signature.as_deref())
        .map(stable_disambiguator)
        .unwrap_or_default();
    format!("{}({}).", name, disambiguator)
}

fn stable_disambiguator(value: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("d{:016x}", hash)
}

fn provider_character_to_utf8(
    source: &str,
    line_number: u32,
    character: u32,
    encoding: RangeEncoding,
) -> Result<u32> {
    let line = source
        .split('\n')
        .nth(line_number as usize)
        .ok_or_else(|| anyhow!("line {} is outside source", line_number))?;
    match encoding {
        RangeEncoding::Utf8ByteOffset => {
            let column = character as usize;
            if column > line.len() || !line.is_char_boundary(column) {
                bail!("UTF-8 offset {} is not a character boundary", character);
            }
            Ok(character)
        }
        RangeEncoding::Utf16CodeUnit | RangeEncoding::LspUtf16 => {
            utf16_code_units_to_utf8_offset(line, character)
        }
    }
}

fn utf16_code_units_to_utf8_offset(line: &str, target_units: u32) -> Result<u32> {
    let mut units = 0u32;
    for (byte_offset, ch) in line.char_indices() {
        if units == target_units {
            return Ok(byte_offset as u32);
        }
        units += ch.len_utf16() as u32;
        if units > target_units {
            bail!("UTF-16 offset {} splits a scalar value", target_units);
        }
    }
    if units == target_units {
        Ok(line.len() as u32)
    } else {
        bail!("UTF-16 offset {} is outside line", target_units)
    }
}

fn scip_package_component(value: &str) -> String {
    if value.is_empty() {
        ".".to_string()
    } else {
        value.replace(' ', "  ")
    }
}

fn scip_identifier(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '+' | '-' | '$'))
    {
        value.to_string()
    } else {
        format!("`{}`", value.replace('`', "``"))
    }
}
