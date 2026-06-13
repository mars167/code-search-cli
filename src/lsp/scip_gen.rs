use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    generation_manifest::{
        hash_config_proof, hash_provider_version, hash_source_proof, mark_fresh, mark_missing,
        mark_partial, new_manifest, GenerationManifest, ManifestState, ProofHashes,
    },
    index,
    output::VerboseLogger,
    project_graph::{
        discover_project_graph, ProjectGraph, ProjectLanguage, ProjectRoot, SemanticFactPolicy,
    },
    scip,
    scip_index::native_db_path,
    semantic_facts::{
        write_scip_index, FactReliability, InternalRange, OccurrenceRole, ProviderProof,
        ProviderRange, RangeEncoding, SemanticOccurrence, SemanticSymbol, SymbolDescriptor,
        SymbolDescriptorKind, SymbolIdentity, SymbolKind, SymbolPackage,
    },
    semantic_provider::{ProviderCapabilities, SemanticProviderVersion},
    workspace::{FileRecord, Workspace},
};

use super::client::{DocumentSymbol, LspClient, LspPosition};
use super::provider::{collect_reference_locations, LSP_PROVIDER_NAME};
use super::registry::{file_path_to_uri, resolve_server, uri_to_relative_path, ServerSpec};

const DEFAULT_SEMANTIC_BUDGET_MS: u64 = 60_000;
const MAX_REFERENCE_PROBES: usize = 200;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticLanguageReport {
    pub language: String,
    pub root_id: String,
    pub provider: Option<String>,
    pub state: String,
    pub occurrence_count: usize,
    pub partial_reasons: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticBuildReport {
    pub attempted: bool,
    pub skipped: bool,
    pub skip_reason: Option<String>,
    pub languages: Vec<SemanticLanguageReport>,
}

impl SemanticBuildReport {
    pub fn skipped(reason: &str) -> Self {
        Self {
            attempted: false,
            skipped: true,
            skip_reason: Some(reason.to_string()),
            languages: Vec::new(),
        }
    }
}

pub fn generate_best_effort(
    workspace: &Workspace,
    records: &[FileRecord],
    verbose: VerboseLogger,
) -> Result<SemanticBuildReport> {
    let db_path = native_db_path(workspace);
    if scip::occurrence_db_fresh(&db_path, &workspace.snapshot_id, &workspace.root) {
        match fresh_occurrence_db_skip_reason(workspace) {
            Ok(Some(reason)) => {
                verbose.log("semantic: occurrence DB already fresh; skipping LSP phase");
                return Ok(SemanticBuildReport {
                    attempted: false,
                    skipped: true,
                    skip_reason: Some(reason),
                    languages: Vec::new(),
                });
            }
            Ok(None) => verbose.log(
                "semantic: occurrence DB is fresh but generation manifest is not fresh; rerunning LSP phase",
            ),
            Err(error) => verbose.log(format!(
                "semantic: occurrence DB is fresh but generation manifest could not be read ({error}); rerunning LSP phase"
            )),
        }
    }

    let graph = discover_project_graph(&workspace.root).unwrap_or_else(|_| ProjectGraph {
        schema_version: ProjectGraph::CURRENT_SCHEMA_VERSION,
        roots: Vec::new(),
        source_owners: Vec::new(),
        generated_sources: Vec::new(),
        config_edges: Vec::new(),
        environment_edges: Vec::new(),
        dependency_edges: Vec::new(),
        caveats: Vec::new(),
    });

    let budget_ms = semantic_budget_ms(&graph);
    let deadline = Instant::now() + Duration::from_millis(budget_ms);
    verbose.log(format!(
        "semantic: starting LSP bridge (budget={budget_ms}ms)"
    ));

    let file_contents = load_file_contents(workspace, records);
    let mut all_occurrences = Vec::new();
    let mut language_reports = Vec::new();
    let mut manifests = Vec::new();

    let groups = lsp_work_groups(workspace, &graph);
    for ((language, lsp_root_path), roots) in groups {
        if Instant::now() >= deadline {
            verbose.log("semantic: wall-clock budget exhausted");
            break;
        }

        let reports = index_lsp_group(
            workspace,
            language,
            &lsp_root_path,
            &roots,
            &file_contents,
            &mut all_occurrences,
            deadline,
            verbose,
        );
        for (root, files, report) in reports {
            language_reports.push(report.clone());
            manifests.push(build_manifest(workspace, root, &report, files, records));
        }
    }

    if all_occurrences.is_empty() {
        scip::invalidate_db(&db_path)
            .with_context(|| "failed to invalidate empty occurrence database")?;
        write_generation_manifests(workspace, &manifests)?;
        return Ok(SemanticBuildReport {
            attempted: true,
            skipped: false,
            skip_reason: None,
            languages: language_reports,
        });
    }

    let scip_index = write_scip_index(&all_occurrences, &workspace.root.to_string_lossy())?;
    fs::create_dir_all(index::scip_root(workspace))?;
    scip::build_occurrences_db(
        &scip_index,
        &db_path,
        &workspace.snapshot_id,
        &workspace.root,
    )
    .with_context(|| "failed to build occurrence database from LSP facts")?;

    write_generation_manifests(workspace, &manifests)?;
    verbose.log(format!(
        "semantic: wrote {} occurrences to {}",
        all_occurrences.len(),
        db_path.display()
    ));

    Ok(SemanticBuildReport {
        attempted: true,
        skipped: false,
        skip_reason: None,
        languages: language_reports,
    })
}

fn semantic_budget_ms(graph: &ProjectGraph) -> u64 {
    if let Some(value) = std::env::var("CODETRAIL_SEMANTIC_BUDGET_MS")
        .ok()
        .and_then(|value| value.parse().ok())
    {
        return value;
    }

    adaptive_semantic_budget_ms(graph)
}

fn adaptive_semantic_budget_ms(graph: &ProjectGraph) -> u64 {
    let java_root_count = graph
        .roots
        .iter()
        .filter(|root| root.language == ProjectLanguage::Java)
        .count() as u64;
    if java_root_count == 0 {
        return DEFAULT_SEMANTIC_BUDGET_MS;
    }

    let java_file_count = graph
        .source_owners
        .iter()
        .filter(|owner| {
            owner.language == ProjectLanguage::Java
                && owner.semantic_fact_policy == SemanticFactPolicy::PreciseEligible
        })
        .count() as u64;

    (180_000 + (java_root_count * 60_000) + (java_file_count * 1_000))
        .clamp(DEFAULT_SEMANTIC_BUDGET_MS, 3_600_000)
}

fn fresh_occurrence_db_skip_reason(workspace: &Workspace) -> Result<Option<String>> {
    let manifests = read_generation_manifests(workspace)?;
    if generation_manifests_allow_occurrence_skip(&manifests) {
        Ok(Some("occurrence_db_fresh".to_string()))
    } else {
        Ok(None)
    }
}

fn generation_manifests_allow_occurrence_skip(manifests: &[GenerationManifest]) -> bool {
    manifests.is_empty()
        || manifests.iter().all(|manifest| {
            manifest.state == ManifestState::Fresh && manifest.partial_reasons.is_empty()
        })
}

pub fn generation_manifests_allow_precise_use(workspace: &Workspace) -> Result<bool> {
    let manifests = read_generation_manifests(workspace)?;
    Ok(manifests.is_empty()
        || manifests
            .iter()
            .all(|manifest| !manifest.state.blocks_precise()))
}

fn load_file_contents(workspace: &Workspace, records: &[FileRecord]) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for record in records {
        let path = workspace.abs_path(&record.path);
        if let Ok(content) = fs::read_to_string(&path) {
            map.insert(record.path.clone(), content);
        }
    }
    map
}

fn source_files_for_root(graph: &ProjectGraph, root: &ProjectRoot) -> Vec<String> {
    graph
        .source_owners
        .iter()
        .filter(|owner| {
            owner.root_id == root.id
                && owner.semantic_fact_policy == SemanticFactPolicy::PreciseEligible
        })
        .map(|owner| owner.path.clone())
        .collect()
}

type LspWorkGroups<'a> = BTreeMap<(ProjectLanguage, PathBuf), Vec<(&'a ProjectRoot, Vec<String>)>>;

fn lsp_work_groups<'a>(workspace: &Workspace, graph: &'a ProjectGraph) -> LspWorkGroups<'a> {
    let mut groups = BTreeMap::new();
    for root in &graph.roots {
        let files = source_files_for_root(graph, root);
        if files.is_empty() {
            continue;
        }
        groups
            .entry((root.language.clone(), lsp_workspace_root(workspace, root)))
            .or_insert_with(Vec::new)
            .push((root, files));
    }
    groups
}

fn lsp_workspace_root(workspace: &Workspace, root: &ProjectRoot) -> PathBuf {
    if root.language == ProjectLanguage::Java {
        return workspace.root.clone();
    }
    if root.path == "." {
        workspace.root.clone()
    } else {
        workspace.root.join(&root.path)
    }
}

fn index_lsp_group<'a>(
    workspace: &Workspace,
    language: ProjectLanguage,
    lsp_root_path: &Path,
    roots: &'a [(&'a ProjectRoot, Vec<String>)],
    file_contents: &BTreeMap<String, String>,
    occurrences: &mut Vec<SemanticOccurrence>,
    deadline: Instant,
    verbose: VerboseLogger,
) -> Vec<(&'a ProjectRoot, &'a [String], SemanticLanguageReport)> {
    let Some(spec) = resolve_server(&language) else {
        return roots
            .iter()
            .map(|(root, files)| {
                (
                    *root,
                    files.as_slice(),
                    semantic_report(
                        root,
                        None,
                        "missing",
                        0,
                        vec!["semantic_provider_missing".to_string()],
                    ),
                )
            })
            .collect();
    };

    verbose.log(format!(
        "semantic: starting LSP group language={} workspace={} roots={}",
        language,
        lsp_root_path.display(),
        roots.len()
    ));

    let mut client = match LspClient::spawn(&spec, lsp_root_path) {
        Ok(client) => client,
        Err(error) => {
            return group_failure_reports(
                roots,
                &spec,
                format!("semantic_provider_startup_failed: {error}"),
            );
        }
    };

    let root_uri = match file_path_to_uri(lsp_root_path) {
        Ok(uri) => uri,
        Err(error) => {
            let _ = client.shutdown();
            return group_failure_reports(
                roots,
                &spec,
                format!("semantic_provider_startup_failed: {error}"),
            );
        }
    };

    if let Err(error) = client.initialize(&root_uri, &spec.readiness) {
        let _ = client.shutdown();
        return group_failure_reports(
            roots,
            &spec,
            format!("semantic_provider_startup_failed: {error}"),
        );
    }

    let language_id = lsp_language_id(&language);
    let provider_version = provider_version_from_client(&client);
    let mut reports = Vec::new();
    let root_count = roots.len();

    for (idx, (root, files)) in roots.iter().enumerate() {
        if Instant::now() >= deadline {
            reports.push((
                *root,
                files.as_slice(),
                semantic_report(
                    root,
                    Some(&spec.provider_id),
                    "partial",
                    0,
                    vec!["semantic_provider_partial: wall_clock_budget".to_string()],
                ),
            ));
            continue;
        }

        let started = Instant::now();
        verbose.log(format!(
            "semantic: indexing root {} ({}) via {}",
            root.id, language, spec.provider_id
        ));
        let report = index_root_with_client(
            workspace,
            root,
            files,
            file_contents,
            occurrences,
            deadline,
            verbose,
            &client,
            &spec.provider_id,
            &provider_version,
            lsp_root_path,
            language_id,
        );
        verbose.log(format!(
            "semantic: finished root {} state={} occurrences={} elapsed_ms={} remaining_roots={}",
            root.id,
            report.state,
            report.occurrence_count,
            started.elapsed().as_millis(),
            root_count.saturating_sub(idx + 1)
        ));
        reports.push((*root, files.as_slice(), report));
    }

    let _ = client.shutdown();
    reports
}

fn group_failure_reports<'a>(
    roots: &'a [(&'a ProjectRoot, Vec<String>)],
    spec: &ServerSpec,
    reason: String,
) -> Vec<(&'a ProjectRoot, &'a [String], SemanticLanguageReport)> {
    roots
        .iter()
        .map(|(root, files)| {
            (
                *root,
                files.as_slice(),
                semantic_report(
                    root,
                    Some(&spec.provider_id),
                    "partial",
                    0,
                    vec![reason.clone()],
                ),
            )
        })
        .collect()
}

fn semantic_report(
    root: &ProjectRoot,
    provider: Option<&str>,
    state: &str,
    occurrence_count: usize,
    partial_reasons: Vec<String>,
) -> SemanticLanguageReport {
    SemanticLanguageReport {
        language: root.language.to_string(),
        root_id: root.id.clone(),
        provider: provider.map(ToString::to_string),
        state: state.to_string(),
        occurrence_count,
        partial_reasons,
    }
}

fn index_root_with_client(
    workspace: &Workspace,
    root: &ProjectRoot,
    files: &[String],
    file_contents: &BTreeMap<String, String>,
    occurrences: &mut Vec<SemanticOccurrence>,
    deadline: Instant,
    _verbose: VerboseLogger,
    client: &LspClient,
    provider_id: &str,
    provider_version: &SemanticProviderVersion,
    lsp_root_path: &Path,
    language_id: &str,
) -> SemanticLanguageReport {
    let package = package_for_root(root);
    let ctx = OccurrenceBuildCtx {
        workspace,
        root,
        package: &package,
        provider_version,
        provider_id,
        encoding: client.position_encoding(),
    };
    let mut root_occurrences = Vec::new();
    let mut partial_reasons = Vec::new();

    for path in files {
        if Instant::now() >= deadline {
            partial_reasons.push("semantic_provider_partial: wall_clock_budget".to_string());
            break;
        }
        let Some(lsp_path) = lsp_relative_path(workspace, lsp_root_path, path) else {
            partial_reasons.push(format!(
                "semantic_provider_partial: path_outside_root:{path}"
            ));
            continue;
        };
        let content = file_contents.get(path).cloned().unwrap_or_default();
        if client.did_open(&lsp_path, language_id, &content).is_err() {
            continue;
        }
        let symbols = match client.document_symbol(&lsp_path) {
            Ok(symbols) => symbols,
            Err(error) => {
                partial_reasons.push(format!("semantic_provider_partial: {error}"));
                continue;
            }
        };
        flatten_symbol_occurrences(&ctx, path, &symbols, &content, &mut root_occurrences);
    }

    let mut reference_budget = reference_probe_limit_for_root(root, files);
    if Instant::now() < deadline && reference_budget > 0 {
        let probes = unique_probe_positions_from_occurrences(&root_occurrences, reference_budget);
        for probe in probes {
            if Instant::now() >= deadline || reference_budget == 0 {
                partial_reasons.push("semantic_provider_partial: reference_budget".to_string());
                break;
            }
            let Some(lsp_path) = lsp_relative_path(workspace, lsp_root_path, &probe.path) else {
                partial_reasons.push(format!(
                    "semantic_provider_partial: path_outside_root:{}",
                    probe.path
                ));
                continue;
            };
            let locations = collect_reference_locations(&client, &lsp_path, &probe.position, 32);
            for location in locations {
                let Some(path) = uri_to_relative_path(&workspace.root, &location.uri) else {
                    continue;
                };
                let Some(content) = file_contents.get(&path) else {
                    continue;
                };
                if let Some(occurrence) = reference_occurrence_from_lsp(
                    &ctx,
                    &path,
                    &location.range,
                    content,
                    &probe.symbol,
                ) {
                    root_occurrences.push(occurrence);
                }
            }
            reference_budget = reference_budget.saturating_sub(1);
        }
    }

    let count = root_occurrences.len();
    occurrences.append(&mut root_occurrences);

    let state = if partial_reasons.is_empty() {
        "fresh"
    } else {
        "partial"
    };
    semantic_report(root, Some(provider_id), state, count, partial_reasons)
}

fn provider_version_from_client(client: &LspClient) -> SemanticProviderVersion {
    if let Some(info) = client.server_info() {
        SemanticProviderVersion {
            name: info.name.clone(),
            version: info.version.clone(),
            protocol_version: 1,
        }
    } else {
        SemanticProviderVersion {
            name: LSP_PROVIDER_NAME.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: 1,
        }
    }
}

fn package_for_root(root: &ProjectRoot) -> SymbolPackage {
    SymbolPackage {
        manager: format!("{:?}", root.kind).to_ascii_lowercase(),
        name: root.path.clone(),
        version: "0.0.0".to_string(),
    }
}

fn lsp_language_id(language: &ProjectLanguage) -> &'static str {
    match language {
        ProjectLanguage::Go => "go",
        ProjectLanguage::Rust => "rust",
        ProjectLanguage::Java => "java",
        ProjectLanguage::TypeScript => "typescript",
    }
}

fn configured_reference_probe_limit() -> Option<usize> {
    std::env::var("CODETRAIL_LSP_REFERENCE_PROBES")
        .ok()
        .and_then(|value| value.parse().ok())
}

fn reference_probe_limit_for_root(root: &ProjectRoot, files: &[String]) -> usize {
    if let Some(limit) = configured_reference_probe_limit() {
        return limit;
    }
    if root.language == ProjectLanguage::Java {
        return MAX_REFERENCE_PROBES.max(files.len().saturating_mul(32).min(5_000));
    }
    MAX_REFERENCE_PROBES
}

fn lsp_relative_path(workspace: &Workspace, lsp_root_path: &Path, path: &str) -> Option<String> {
    let abs_path = workspace.abs_path(path);
    let relative = abs_path.strip_prefix(lsp_root_path).ok()?;
    Some(relative.to_string_lossy().replace('\\', "/"))
}

struct OccurrenceBuildCtx<'a> {
    workspace: &'a Workspace,
    root: &'a ProjectRoot,
    package: &'a SymbolPackage,
    provider_version: &'a SemanticProviderVersion,
    provider_id: &'a str,
    encoding: &'a str,
}

struct ReferenceProbe {
    path: String,
    position: LspPosition,
    symbol: SemanticSymbol,
}

fn flatten_symbol_occurrences(
    ctx: &OccurrenceBuildCtx<'_>,
    path: &str,
    symbols: &[DocumentSymbol],
    content: &str,
    out: &mut Vec<SemanticOccurrence>,
) {
    let mut parents = Vec::new();
    flatten_symbol_occurrences_with_parents(ctx, path, symbols, content, &mut parents, out);
}

fn flatten_symbol_occurrences_with_parents(
    ctx: &OccurrenceBuildCtx<'_>,
    path: &str,
    symbols: &[DocumentSymbol],
    content: &str,
    parents: &mut Vec<SymbolDescriptor>,
    out: &mut Vec<SemanticOccurrence>,
) {
    for symbol in symbols {
        if let Some(occurrence) =
            definition_occurrence_from_lsp(ctx, path, symbol, content, parents)
        {
            out.push(occurrence);
        }
        parents.push(symbol_descriptor_from_lsp(symbol));
        flatten_symbol_occurrences_with_parents(ctx, path, &symbol.children, content, parents, out);
        parents.pop();
    }
}

fn definition_occurrence_from_lsp(
    ctx: &OccurrenceBuildCtx<'_>,
    path: &str,
    symbol: &DocumentSymbol,
    content: &str,
    parents: &[SymbolDescriptor],
) -> Option<SemanticOccurrence> {
    let range = lsp_range_to_internal(
        &symbol.selection_range.start,
        &symbol.selection_range.end,
        ctx.encoding,
        content,
    )
    .ok()?;
    Some(SemanticOccurrence {
        file_path: path.to_string(),
        range,
        role: OccurrenceRole::Definition,
        symbol: semantic_symbol_from_lsp(
            ctx.root,
            symbol,
            parents,
            ctx.package,
            ctx.provider_version,
        ),
        proof: ProviderProof {
            provider_id: ctx.provider_id.to_string(),
            provider_version: ctx.provider_version.clone(),
            reliability: FactReliability::ProviderConfirmed,
            evidence: format!("lsp:documentSymbol:{}", ctx.workspace.snapshot_id),
        },
    })
}

fn reference_occurrence_from_lsp(
    ctx: &OccurrenceBuildCtx<'_>,
    path: &str,
    range: &super::client::LspRange,
    content: &str,
    symbol: &SemanticSymbol,
) -> Option<SemanticOccurrence> {
    let internal = lsp_range_to_internal(&range.start, &range.end, ctx.encoding, content).ok()?;
    Some(SemanticOccurrence {
        file_path: path.to_string(),
        range: internal,
        role: OccurrenceRole::Reference,
        symbol: symbol.clone(),
        proof: ProviderProof {
            provider_id: ctx.provider_id.to_string(),
            provider_version: ctx.provider_version.clone(),
            reliability: FactReliability::ProviderConfirmed,
            evidence: format!("lsp:references:{}", ctx.workspace.snapshot_id),
        },
    })
}

fn semantic_symbol_from_lsp(
    root: &ProjectRoot,
    symbol: &DocumentSymbol,
    parents: &[SymbolDescriptor],
    package: &SymbolPackage,
    provider_version: &SemanticProviderVersion,
) -> SemanticSymbol {
    let kind = lsp_kind_to_symbol_kind(symbol.kind);
    let mut descriptors = parents.to_vec();
    descriptors.push(symbol_descriptor_from_lsp(symbol));
    SemanticSymbol {
        identity: SymbolIdentity {
            language: root.language.clone(),
            project_id: root.id.clone(),
            package: package.clone(),
            descriptors,
            signature: None,
            disambiguator: None,
            provider_version: provider_version.clone(),
            generated: false,
            local_id: None,
        },
        kind,
        display_name: symbol.name.clone(),
        documentation: Vec::new(),
    }
}

fn symbol_descriptor_from_lsp(symbol: &DocumentSymbol) -> SymbolDescriptor {
    let kind = lsp_kind_to_symbol_kind(symbol.kind);
    SymbolDescriptor {
        name: symbol.name.clone(),
        kind: SymbolDescriptorKind::from_symbol_kind(&kind),
    }
}

fn lsp_kind_to_symbol_kind(kind: u32) -> SymbolKind {
    match kind {
        5 => SymbolKind::Class,
        6 => SymbolKind::Method,
        10 => SymbolKind::Enum,
        11 => SymbolKind::Interface,
        12 => SymbolKind::Function,
        23 => SymbolKind::Struct,
        13 => SymbolKind::Variable,
        22 => SymbolKind::Constant,
        4 => SymbolKind::Module,
        _ => SymbolKind::Unknown,
    }
}

fn lsp_range_to_internal(
    start: &LspPosition,
    end: &LspPosition,
    encoding: &str,
    content: &str,
) -> Result<InternalRange> {
    let range_encoding = if encoding == "utf-8" {
        RangeEncoding::Utf8ByteOffset
    } else {
        RangeEncoding::LspUtf16
    };
    ProviderRange {
        start_line: start.line,
        start_character: start.character,
        end_line: end.line,
        end_character: end.character,
        encoding: range_encoding,
    }
    .to_internal_range(content)
}

fn unique_probe_positions_from_occurrences(
    occurrences: &[SemanticOccurrence],
    limit: usize,
) -> Vec<ReferenceProbe> {
    let mut seen = BTreeSet::new();
    let mut probes = Vec::new();
    for occurrence in occurrences {
        if occurrence.role != OccurrenceRole::Definition {
            continue;
        }
        let key = format!(
            "{}:{}:{}",
            occurrence.file_path, occurrence.range.start_line, occurrence.range.start_column
        );
        if !seen.insert(key) {
            continue;
        }
        probes.push(ReferenceProbe {
            path: occurrence.file_path.clone(),
            position: LspPosition {
                line: occurrence.range.start_line,
                character: occurrence.range.start_column,
            },
            symbol: occurrence.symbol.clone(),
        });
        if probes.len() >= limit {
            break;
        }
    }
    probes
}

fn build_manifest(
    workspace: &Workspace,
    root: &ProjectRoot,
    report: &SemanticLanguageReport,
    files: &[String],
    records: &[FileRecord],
) -> GenerationManifest {
    let caps = ProviderCapabilities {
        language: root.language.clone(),
        provider_version: SemanticProviderVersion {
            name: report
                .provider
                .clone()
                .unwrap_or_else(|| LSP_PROVIDER_NAME.to_string()),
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: 1,
        },
        supports_batch_resolve: true,
        supports_import_graph: false,
        supports_workspace_symbols: true,
        max_batch_size: reference_probe_limit_for_root(root, files),
        partial_reasons: Vec::new(),
    };
    let file_set: BTreeSet<_> = files.iter().cloned().collect();
    let proofs: Vec<(String, String)> = records
        .iter()
        .filter(|record| file_set.contains(&record.path))
        .map(|record| {
            (
                record.path.clone(),
                record
                    .hash
                    .strip_prefix("blake3:")
                    .unwrap_or(&record.hash)
                    .to_string(),
            )
        })
        .collect();
    let hashes = ProofHashes {
        provider_version_hash: hash_provider_version(&caps),
        environment_hash: environment_hash(report),
        source_proof_hash: hash_source_proof(&proofs),
        config_proof_hash: hash_config_proof(&[]),
    };
    let mut manifest = new_manifest(
        root,
        report.provider.as_deref().unwrap_or(LSP_PROVIDER_NAME),
        &hashes,
    );
    match report.state.as_str() {
        "fresh" => mark_fresh(&mut manifest, &hashes),
        "missing" => mark_missing(&mut manifest),
        _ => mark_partial(&mut manifest, report.partial_reasons.clone()),
    }
    let _ = workspace;
    manifest
}

fn environment_hash(report: &SemanticLanguageReport) -> String {
    let payload = format!(
        "{}:{}",
        report.provider.as_deref().unwrap_or("missing"),
        report.language
    );
    blake3::hash(payload.as_bytes()).to_hex().to_string()
}

pub fn generation_manifest_path(workspace: &Workspace) -> std::path::PathBuf {
    index::scip_root(workspace).join("generation.json")
}

pub fn read_generation_manifests(workspace: &Workspace) -> Result<Vec<GenerationManifest>> {
    let path = generation_manifest_path(workspace);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let data = fs::read(&path)?;
    Ok(serde_json::from_slice(&data)?)
}

fn write_generation_manifests(
    workspace: &Workspace,
    manifests: &[GenerationManifest],
) -> Result<()> {
    if manifests.is_empty() {
        return Ok(());
    }
    let path = generation_manifest_path(workspace);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, serde_json::to_vec_pretty(manifests)?)?;
    Ok(())
}

pub fn semantic_summary_json(report: &SemanticBuildReport) -> Value {
    json!({
        "attempted": report.attempted,
        "skipped": report.skipped,
        "skipReason": report.skip_reason,
        "languages": report.languages.iter().map(|lang| json!({
            "language": lang.language,
            "rootId": lang.root_id,
            "provider": lang.provider,
            "state": lang.state,
            "occurrenceCount": lang.occurrence_count,
            "partialReasons": lang.partial_reasons,
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_graph::ProjectLanguage;

    fn manifest(state: ManifestState, partial_reasons: Vec<String>) -> GenerationManifest {
        GenerationManifest {
            schema_version: 1,
            generation_id: "test".to_string(),
            root_id: "java:app".to_string(),
            language: ProjectLanguage::Java,
            provider_name: "jdtls".to_string(),
            provider_version_hash: "provider".to_string(),
            environment_hash: "env".to_string(),
            source_proof_hash: "source".to_string(),
            config_proof_hash: "config".to_string(),
            state,
            partial_reasons,
            created_at_epoch_ms: 1,
            updated_at_epoch_ms: 1,
        }
    }

    #[test]
    fn occurrence_db_skip_requires_fresh_generation_manifests() {
        assert!(generation_manifests_allow_occurrence_skip(&[]));
        assert!(generation_manifests_allow_occurrence_skip(&[manifest(
            ManifestState::Fresh,
            Vec::new()
        )]));
        assert!(!generation_manifests_allow_occurrence_skip(&[manifest(
            ManifestState::Partial,
            vec!["semantic_provider_partial: wall_clock_budget".to_string()]
        )]));
        assert!(!generation_manifests_allow_occurrence_skip(&[manifest(
            ManifestState::Fresh,
            vec!["semantic_provider_partial: reference_budget".to_string()]
        )]));
        assert!(!generation_manifests_allow_occurrence_skip(&[manifest(
            ManifestState::Missing,
            vec!["semantic_provider_missing".to_string()]
        )]));
    }

    #[test]
    fn precise_use_blocks_missing_stale_and_updating_manifests() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::discover(dir.path()).unwrap();
        assert!(generation_manifests_allow_precise_use(&workspace).unwrap());

        let path = generation_manifest_path(&workspace);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        for state in [
            ManifestState::Missing,
            ManifestState::Stale,
            ManifestState::Updating,
        ] {
            fs::write(
                &path,
                serde_json::to_vec_pretty(&vec![manifest(state, Vec::new())]).unwrap(),
            )
            .unwrap();
            assert!(!generation_manifests_allow_precise_use(&workspace).unwrap());
        }

        fs::write(
            &path,
            serde_json::to_vec_pretty(&vec![manifest(
                ManifestState::Partial,
                vec!["semantic_provider_partial: reference_budget".to_string()],
            )])
            .unwrap(),
        )
        .unwrap();
        assert!(generation_manifests_allow_precise_use(&workspace).unwrap());
    }
}
