//! Rust rust-analyzer provider adapter protocol.
//!
//! Rust compiler internal APIs are unstable. rust-analyzer already handles
//! workspace, module, macro, trait resolution, and call hierarchy. The adapter
//! starts a rust-analyzer session per project root and consumes candidate
//! probes via LSP (`textDocument/definition`, `textDocument/references`,
//! `callHierarchy/incomingCalls`).
//!
//! Features, target cfg, rust toolchain, rust-src, proc macro, and build.rs
//! status enter the environment/config proof. Macro or proc-macro unavailability
//! must be reflected as partial reasons, not silent imprecision.

use serde::{Deserialize, Serialize};

use crate::semantic_provider::{ProviderCapabilities, SemanticProviderVersion};

pub const RUST_PROVIDER_NAME: &str = "rust-analyzer";
pub const RUST_PROTOCOL_VERSION: u32 = 1;

// ── Adapter configuration ───────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RustAdapterConfig {
    pub project_root: String,
    pub cargo_features: Vec<String>,
    pub target_triple: Option<String>,
    pub cfg_flags: Vec<String>,
    pub rust_toolchain: String,
    pub rust_src_available: bool,
    pub proc_macro_enabled: bool,
    pub build_rs_status: BuildRsStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildRsStatus {
    Ok,
    Failed(String),
    Skipped,
    NotPresent,
}

// ── Session lifecycle ───────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RustSessionState {
    Starting,
    WorkspaceLoading,
    Ready,
    Resolving,
    Stale,
    Partial,
    Shutdown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RustSessionReport {
    pub root_id: String,
    pub state: RustSessionState,
    pub crate_count: usize,
    pub module_count: usize,
    pub partial_reasons: Vec<String>,
    pub peak_rss_mb: Option<u64>,
}

// ── Symbol identity ─────────────────────────────────────────────────────────

/// Construct a stable Rust symbol ID.
///
/// Format: `rust:<crate_name>/<module_path>::<item_path>[(<trait>)]::<signature_disambiguator>`
///
/// Examples:
/// - `rust:tokio/sync::mpsc::channel::<T>#abc`
/// - `rust:serde/serde::de::Deserialize::deserialize#def`
/// - `rust:my_crate/src/lib::MyStruct::(Default)::default#ghi`
pub fn rust_symbol_id(
    crate_name: &str,
    module_path: &str,
    item_path: &str,
    impl_trait: Option<&str>,
    signature_hash: &str,
) -> String {
    let trait_segment = impl_trait.map(|t| format!("({t})::")).unwrap_or_default();
    format!(
        "rust:{}:{}::{}::{}{}#{}",
        crate_name, module_path, item_path, trait_segment, item_path, signature_hash
    )
}

// ── Environment hash ────────────────────────────────────────────────────────

pub fn rust_environment_hash(
    toolchain: &str,
    features: &[String],
    target: Option<&str>,
    cargo_lock_hash: &str,
) -> String {
    let mut feats = features.to_vec();
    feats.sort();
    let target_str = target.unwrap_or("host");
    let payload = format!(
        "rust:{toolchain}:{}:{target_str}:{cargo_lock_hash}",
        feats.join(",")
    );
    blake3::hash(payload.as_bytes()).to_hex().to_string()
}

// ── Provider capabilities ───────────────────────────────────────────────────

pub fn rust_provider_capabilities() -> ProviderCapabilities {
    ProviderCapabilities {
        language: crate::project_graph::ProjectLanguage::Rust,
        provider_version: SemanticProviderVersion {
            name: RUST_PROVIDER_NAME.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: RUST_PROTOCOL_VERSION,
        },
        supports_batch_resolve: true,
        supports_import_graph: true,
        supports_workspace_symbols: true,
        max_batch_size: 300,
        partial_reasons: vec![
            crate::semantic_provider::PartialReason::ProviderMissing,
            crate::semantic_provider::PartialReason::StartupFailed,
            crate::semantic_provider::PartialReason::Timeout,
            crate::semantic_provider::PartialReason::ResourceLimited,
            crate::semantic_provider::PartialReason::ProviderPartial,
            crate::semantic_provider::PartialReason::UnsupportedCapability,
            crate::semantic_provider::PartialReason::ResolveFailed,
        ],
    }
}

// ── Fixture design ──────────────────────────────────────────────────────────

/// Test fixtures needed for Rust provider validation:
///
/// 1. **Module reference**:
///    ```rust
///    // src/lib.rs
///    pub fn init() { }
///    // src/main.rs
///    use my_crate::init;
///    fn main() { init(); }
///    ```
///    Expected: one def `init`, one ref in `main.rs`.
///
/// 2. **Trait method**:
///    ```rust
///    trait Service { fn handle(&self); }
///    struct App;
///    impl Service for App { fn handle(&self) { } }
///    ```
///    Expected: `handle` on `App` resolves trait impl, not standalone.
///
/// 3. **Macro expansion limitation**:
///    ```rust
///    #[tokio::main]
///    async fn main() { }
///    ```
///    Expected: `proc_macro_enabled=false` → partial reason, not fake def.
///
/// 4. **Feature cfg**:
///    ```rust
///    #[cfg(feature = "serde")]
///    impl Serialize for MyType { }
///    ```
///    Expected: symbol only present when `serde` feature is active.
///
/// 5. **Workspace multi-crate**:
///    Multiple crates in a Cargo workspace.
///    Expected: cross-crate refs resolved through workspace layout.
///
/// 6. **Large workspace memory limit**:
///    Cargo workspace with 50+ crates.
///    Expected: `ResourceLimited` partial reason when RSS exceeds budget.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_symbol_id_includes_crate_and_module() {
        let id = rust_symbol_id("tokio", "sync::mpsc", "channel", None, "abc");
        assert!(id.starts_with("rust:tokio:"));
        assert!(id.contains("sync::mpsc"));
        assert!(id.contains("channel"));
    }

    #[test]
    fn rust_symbol_id_with_trait() {
        let id = rust_symbol_id(
            "serde",
            "serde::de",
            "Deserialize::deserialize",
            Some("Deserialize"),
            "def",
        );
        assert!(id.contains("(Deserialize)"));
    }

    #[test]
    fn rust_environment_hash_is_deterministic() {
        let a = rust_environment_hash("stable-x86_64", &["serde".into()], None, "aaa");
        let b = rust_environment_hash("stable-x86_64", &["serde".into()], None, "aaa");
        assert_eq!(a, b);
    }

    #[test]
    fn rust_environment_hash_differs_on_feature_change() {
        let a = rust_environment_hash("stable", &[], None, "aaa");
        let b = rust_environment_hash("stable", &["serde".into()], None, "aaa");
        assert_ne!(a, b);
    }

    #[test]
    fn rust_provider_capabilities_include_resolve_failed() {
        let caps = rust_provider_capabilities();
        let reasons: Vec<_> = caps
            .partial_reasons
            .iter()
            .map(|r| format!("{r:?}"))
            .collect();
        assert!(reasons.iter().any(|r| r.contains("ResolveFailed")));
        assert!(caps.language == crate::project_graph::ProjectLanguage::Rust);
    }
}
