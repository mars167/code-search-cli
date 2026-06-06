//! TypeScript Compiler API provider protocol.
//!
//! TypeScript/JavaScript projects have project references, path aliases,
//! `allowJs`/`checkJs`, and `@types` dependencies. The TypeScript Compiler API
//! (`ts.createProgram` + `TypeChecker`) provides batch access to declarations,
//! references, alias resolution, and call candidates with fewer round-trips
//! than per-symbol LSP requests.
//!
//! The helper runs as a Node.js process. CodeTrail core sends candidate probes
//! and receives normalized semantic facts.

use serde::{Deserialize, Serialize};

use crate::semantic_provider::{ProviderCapabilities, SemanticProviderVersion};

pub const TS_PROVIDER_NAME: &str = "codetrail-ts-helper";
pub const TS_PROTOCOL_VERSION: u32 = 1;

// ── Request / response protocol ─────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TsProviderRequest {
    pub protocol_version: u32,
    pub project_root: String,
    pub tsconfig_paths: Vec<String>,
    pub probes: Vec<TsSymbolProbe>,
    pub budget: TsProviderBudget,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TsSymbolProbe {
    pub file_path: String,
    pub range_start_line: u32,
    pub range_start_col: u32,
    pub range_end_line: u32,
    pub range_end_col: u32,
    pub probe_kind: TsProbeKind,
    pub candidate_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TsProbeKind {
    Declaration,
    Reference,
    CallTarget,
    AliasResolution,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TsProviderBudget {
    pub max_files: usize,
    pub max_symbols_per_file: usize,
    pub timeout_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TsProviderResponse {
    pub protocol_version: u32,
    pub ts_version: String,
    pub environment_hash: String,
    pub files: Vec<TsFileResult>,
    pub partial_reasons: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TsFileResult {
    pub file_path: String,
    pub is_js: bool,
    pub symbols: Vec<TsResolvedSymbol>,
    pub partial: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TsResolvedSymbol {
    pub symbol_id: String,
    pub display_name: String,
    pub kind: TsSymbolKind,
    pub role: TsSymbolRole,
    pub file_path: String,
    pub range_start_line: u32,
    pub range_start_col: u32,
    pub range_end_line: u32,
    pub range_end_col: u32,
    pub container_name: Option<String>,
    pub is_default_export: bool,
    pub precision: TsPrecision,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TsSymbolKind {
    Function,
    Class,
    Interface,
    Type,
    Enum,
    Variable,
    Namespace,
    Method,
    Property,
    Parameter,
    Module,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TsSymbolRole {
    Declaration,
    Reference,
    CallCandidate,
    AliasTarget,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TsPrecision {
    /// Full TypeScript semantic resolution via TypeChecker.
    Precise,
    /// JavaScript file — no type information, name-based heuristics only.
    JsHeuristic,
    /// Declaration exists but resolution was blocked (e.g., missing @types).
    Partial,
}

// ── Symbol identity ─────────────────────────────────────────────────────────

/// Construct a stable TypeScript symbol ID.
///
/// Format: `ts:<package_name>/<file_path>.<container>.<name>[@<signature>]`
///
/// Examples:
/// - `ts:vue/core/packages/reactivity/src/reactive.ts.reactive#abc`
/// - `ts:@types/express/index.d.ts.Express.Request.get`
pub fn ts_symbol_id(
    package_name: &str,
    file_path: &str,
    container: Option<&str>,
    name: &str,
    signature_hash: &str,
) -> String {
    let container_segment = container.map(|c| format!(".{c}")).unwrap_or_default();
    format!(
        "ts:{}:{}{}.{}#{}",
        package_name, file_path, container_segment, name, signature_hash
    )
}

// ── Environment hash ────────────────────────────────────────────────────────

pub fn ts_environment_hash(
    ts_version: &str,
    tsconfig_hashes: &[String],
    package_manager: &str,
    lockfile_hash: &str,
) -> String {
    let mut configs = tsconfig_hashes.to_vec();
    configs.sort();
    let payload = format!(
        "ts:{ts_version}:{}:{package_manager}:{lockfile_hash}",
        configs.join(",")
    );
    blake3::hash(payload.as_bytes()).to_hex().to_string()
}

// ── Provider capabilities ───────────────────────────────────────────────────

pub fn ts_provider_capabilities() -> ProviderCapabilities {
    ProviderCapabilities {
        language: crate::project_graph::ProjectLanguage::TypeScript,
        provider_version: SemanticProviderVersion {
            name: TS_PROVIDER_NAME.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: TS_PROTOCOL_VERSION,
        },
        supports_batch_resolve: true,
        supports_import_graph: true,
        supports_workspace_symbols: true,
        max_batch_size: 500,
        partial_reasons: vec![
            crate::semantic_provider::PartialReason::ProviderMissing,
            crate::semantic_provider::PartialReason::StartupFailed,
            crate::semantic_provider::PartialReason::Timeout,
            crate::semantic_provider::PartialReason::ResourceLimited,
            crate::semantic_provider::PartialReason::ProviderPartial,
            crate::semantic_provider::PartialReason::UnsupportedCapability,
        ],
    }
}

// ── Fixture design ──────────────────────────────────────────────────────────

/// Test fixtures needed for TypeScript provider validation:
///
/// 1. **Basic declaration and reference**:
///    ```ts
///    // src/math.ts
///    export function add(a: number, b: number): number { return a + b; }
///    // src/main.ts
///    import { add } from "./math"; add(1, 2);
///    ```
///    Expected: `add` declaration at math.ts:1, reference at main.ts:1.
///
/// 2. **Default and named export**:
///    ```ts
///    // src/defaults.ts
///    export default class App { }
///    export const version = "1.0";
///    // src/index.ts
///    import App, { version } from "./defaults";
///    ```
///    Expected: `App` has `is_default_export: true`.
///
/// 3. **Path alias**:
///    ```json
///    // tsconfig.json
///    { "compilerOptions": { "paths": { "@lib/*": ["./src/lib/*"] } } }
///    ```
///    ```ts
///    import { helper } from "@lib/helpers";
///    ```
///    Expected: reference resolves to `src/lib/helpers.ts`.
///
/// 4. **Project references**:
///    Multi-package monorepo: `packages/a` and `packages/b`.
///    Expected: cross-package references resolved through project references.
///
/// 5. **allowJs / checkJs**:
///    JavaScript files with JSDoc annotations.
///    Expected: `TsPrecision::JsHeuristic` for `.js`, `Precise` for `.ts`.
///
/// 6. **Re-export chain**:
///    ```ts
///    // src/internal.ts: export function fn() {}
///    // src/index.ts: export { fn } from "./internal";
///    ```
///    Expected: `refs fn` from `index.ts` resolves through the re-export.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ts_symbol_id_includes_package_and_file() {
        let id = ts_symbol_id(
            "vue",
            "packages/reactivity/src/reactive.ts",
            None,
            "reactive",
            "abc",
        );
        assert!(id.starts_with("ts:vue:"));
        assert!(id.contains("reactive.ts"));
        assert!(id.contains(".reactive#"));
    }

    #[test]
    fn ts_symbol_id_with_container() {
        let id = ts_symbol_id(
            "my-app",
            "src/services.ts",
            Some("ApiService"),
            "fetch",
            "def456",
        );
        assert!(id.contains(".ApiService.fetch#"));
    }

    #[test]
    fn ts_environment_hash_is_deterministic() {
        let a = ts_environment_hash("5.3", &[], "npm", "aaa");
        let b = ts_environment_hash("5.3", &[], "npm", "aaa");
        assert_eq!(a, b);
    }

    #[test]
    fn ts_environment_hash_differs_on_package_manager() {
        let a = ts_environment_hash("5.3", &[], "npm", "aaa");
        let b = ts_environment_hash("5.3", &[], "pnpm", "aaa");
        assert_ne!(a, b);
    }

    #[test]
    fn ts_provider_capabilities_include_unsupported() {
        let caps = ts_provider_capabilities();
        let reasons: Vec<_> = caps
            .partial_reasons
            .iter()
            .map(|r| format!("{r:?}"))
            .collect();
        assert!(reasons.iter().any(|r| r.contains("UnsupportedCapability")));
        assert!(caps.language == crate::project_graph::ProjectLanguage::TypeScript);
    }

    #[test]
    fn ts_precision_variants_cover_js_and_partial() {
        // Verify all precision levels are representable
        let levels = [
            TsPrecision::Precise,
            TsPrecision::JsHeuristic,
            TsPrecision::Partial,
        ];
        assert_eq!(levels.len(), 3);
    }
}
