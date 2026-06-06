//! Go compiler-helper provider protocol.
//!
//! Go's `go/packages` + `go/types` API provides batch access to package graphs,
//! definitions, uses, and type information. This is preferred over per-symbol
//! LSP `textDocument/references` for whole-repository indexing.
//!
//! The helper runs as an external process (Go binary or `gopls` in serve mode).
//! CodeTrail core sends candidate probes and receives normalized semantic facts.

use serde::{Deserialize, Serialize};

use crate::semantic_provider::{ProviderCapabilities, SemanticProviderVersion};

pub const GO_PROVIDER_NAME: &str = "codetrail-go-helper";
pub const GO_PROTOCOL_VERSION: u32 = 1;

// ── Request / response protocol ─────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoProviderRequest {
    pub protocol_version: u32,
    pub project_root: String,
    pub goos: Option<String>,
    pub goarch: Option<String>,
    pub build_tags: Vec<String>,
    pub probes: Vec<GoSymbolProbe>,
    pub budget: GoProviderBudget,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoSymbolProbe {
    pub file_path: String,
    pub range_start_line: u32,
    pub range_start_col: u32,
    pub range_end_line: u32,
    pub range_end_col: u32,
    pub probe_kind: GoProbeKind,
    pub candidate_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoProbeKind {
    Definition,
    Reference,
    CallTarget,
    MethodReceiver,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoProviderBudget {
    pub max_packages: usize,
    pub max_symbols_per_package: usize,
    pub timeout_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoProviderResponse {
    pub protocol_version: u32,
    pub go_version: String,
    pub environment_hash: String,
    pub packages: Vec<GoPackageResult>,
    pub partial_reasons: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoPackageResult {
    pub import_path: String,
    pub module_path: String,
    pub module_version: String,
    pub symbols: Vec<GoResolvedSymbol>,
    pub partial: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoResolvedSymbol {
    pub symbol_id: String,
    pub display_name: String,
    pub kind: GoSymbolKind,
    pub role: GoSymbolRole,
    pub file_path: String,
    pub range_start_line: u32,
    pub range_start_col: u32,
    pub range_end_line: u32,
    pub range_end_col: u32,
    pub receiver_type: Option<String>,
    pub is_exported: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoSymbolKind {
    Function,
    Method,
    Type,
    Interface,
    Variable,
    Constant,
    Package,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoSymbolRole {
    Definition,
    Reference,
    CallCandidate,
}

// ── Symbol identity ─────────────────────────────────────────────────────────

/// Construct a stable Go symbol ID: `go:<module_path>/<package_path>.<qualified_name>`
///
/// Examples:
/// - `go:github.com/gin-gonic/gin.context.Request`
/// - `go:std/net/http.ServeMux.Handle`
/// - `go:github.com/gin-gonic/gin.(*Context).Next`
pub fn go_symbol_id(
    module_path: &str,
    package_path: &str,
    qualified_name: &str,
    receiver: Option<&str>,
    signature_hash: &str,
) -> String {
    let receiver_segment = receiver.map(|r| format!("({r}).")).unwrap_or_default();
    format!(
        "go:{}:{}:{}{}#{}",
        module_path, package_path, receiver_segment, qualified_name, signature_hash
    )
}

// ── Environment hash ────────────────────────────────────────────────────────

/// Hash of the Go environment for freshness checks.
///
/// Inputs: `go version`, `GOOS`, `GOARCH`, build tags, `go.mod` content, `go.sum` hash.
pub fn go_environment_hash(
    go_version: &str,
    goos: &str,
    goarch: &str,
    build_tags: &[String],
    go_mod_hash: &str,
) -> String {
    let mut tags = build_tags.to_vec();
    tags.sort();
    let payload = format!(
        "go:{go_version}:{goos}:{goarch}:{}:{go_mod_hash}",
        tags.join(",")
    );
    blake3::hash(payload.as_bytes()).to_hex().to_string()
}

// ── Provider capabilities ───────────────────────────────────────────────────

pub fn go_provider_capabilities() -> ProviderCapabilities {
    ProviderCapabilities {
        language: crate::project_graph::ProjectLanguage::Go,
        provider_version: SemanticProviderVersion {
            name: GO_PROVIDER_NAME.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: GO_PROTOCOL_VERSION,
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
        ],
    }
}

// ── Fixture design ──────────────────────────────────────────────────────────

/// Test fixtures needed for Go provider validation:
///
/// 1. **Single package**:
///    ```go
///    // pkg/math.go
///    package math
///    func Add(a, b int) int { return a + b }
///    ```
///    Expected: one definition occurrence for `Add`, zero references.
///
/// 2. **Cross-file reference**:
///    ```go
///    // pkg/math.go
///    func Add(a, b int) int { return a + b }
///    // cmd/main.go
///    import "example/pkg"
///    func main() { pkg.Add(1, 2) }
///    ```
///    Expected: one def for `Add`, one ref in `cmd/main.go`.
///
/// 3. **Method receiver**:
///    ```go
///    type Server struct { port int }
///    func (s *Server) Start() error { return nil }
///    ```
///    Expected: `Start` has receiver `*Server`, symbol id includes `(*Server).Start`.
///
/// 4. **Import alias**:
///    ```go
///    import nethttp "net/http"
///    nethttp.ListenAndServe(":8080", nil)
///    ```
///    Expected: reference resolves to `net/http.ListenAndServe`.
///
/// 5. **Build tag variance**:
///    ```go
///    // +build linux
///    func PlatformInit() { ... }
///    ```
///    Expected: symbol only present when `linux` build tag is active.
///
/// 6. **Package load failure**:
///    Missing dependency → partial reason `ProviderPartial`, no fake precise facts.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_symbol_id_includes_module_package_and_qualified_name() {
        let id = go_symbol_id(
            "github.com/example/mod",
            "github.com/example/mod/pkg",
            "Handler.ServeHTTP",
            None,
            "abc123",
        );
        assert!(id.starts_with("go:github.com/example/mod:"));
        assert!(id.contains("pkg"));
        assert!(id.contains("Handler.ServeHTTP"));
    }

    #[test]
    fn go_symbol_id_with_receiver() {
        let id = go_symbol_id(
            "github.com/gin-gonic/gin",
            "github.com/gin-gonic/gin",
            "Next",
            Some("*Context"),
            "def456",
        );
        assert!(id.contains("(*Context).Next"));
    }

    #[test]
    fn go_environment_hash_is_deterministic() {
        let a = go_environment_hash("go1.22", "linux", "amd64", &[], "abc");
        let b = go_environment_hash("go1.22", "linux", "amd64", &[], "abc");
        assert_eq!(a, b);
    }

    #[test]
    fn go_environment_hash_differs_on_version_change() {
        let a = go_environment_hash("go1.22", "linux", "amd64", &[], "abc");
        let b = go_environment_hash("go1.23", "linux", "amd64", &[], "abc");
        assert_ne!(a, b);
    }

    #[test]
    fn go_provider_capabilities_cover_required_partial_reasons() {
        let caps = go_provider_capabilities();
        assert_eq!(caps.language, crate::project_graph::ProjectLanguage::Go);
        assert!(caps.supports_batch_resolve);
        assert!(caps.supports_import_graph);
        let reasons: Vec<_> = caps
            .partial_reasons
            .iter()
            .map(|r| format!("{r:?}"))
            .collect();
        assert!(reasons.iter().any(|r| r.contains("ProviderMissing")));
        assert!(reasons.iter().any(|r| r.contains("Timeout")));
    }
}
