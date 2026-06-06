use codetrail::{
    project_graph::{ProjectLanguage, ProjectRoot, ProjectRootKind},
    semantic_provider::{
        NormalizedSemanticFact, PartialReason, ProviderBudget, ProviderCapabilities,
        ProviderFailure, ProviderFailureReason, ProviderRootState, ProviderSession,
        ProviderSessionInput, QueuePriority, SemanticBatchResult, SemanticProbe, SemanticProbeKind,
        SemanticProvider, SemanticProviderVersion, SemanticRange, SemanticScheduler,
    },
};

#[derive(Clone, Debug)]
enum FakeBehavior {
    Ok,
    StartFailure,
    Timeout,
    Partial(PartialReason),
    ResourceLimited,
}

#[derive(Clone, Debug)]
struct FakeProvider {
    id: String,
    language: ProjectLanguage,
    budget: ProviderBudget,
    behavior: FakeBehavior,
    started_roots: Vec<String>,
    resolved_batches: Vec<Vec<SemanticProbe>>,
    shutdown_roots: Vec<String>,
}

impl FakeProvider {
    fn new(id: &str, language: ProjectLanguage, behavior: FakeBehavior) -> Self {
        Self {
            id: id.to_string(),
            language,
            behavior,
            budget: ProviderBudget {
                max_concurrent_roots: 2,
                max_concurrent_resolves_per_root: 2,
                single_symbol_timeout_ms: 25,
                idle_shutdown_ms: 1_000,
                max_memory_mb: 512,
            },
            started_roots: Vec::new(),
            resolved_batches: Vec::new(),
            shutdown_roots: Vec::new(),
        }
    }

    fn with_budget(mut self, budget: ProviderBudget) -> Self {
        self.budget = budget;
        self
    }
}

impl SemanticProvider for FakeProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn language(&self) -> ProjectLanguage {
        self.language.clone()
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            language: self.language.clone(),
            provider_version: SemanticProviderVersion {
                name: self.id.clone(),
                version: "0.0.0-test".to_string(),
                protocol_version: 1,
            },
            supports_batch_resolve: true,
            supports_import_graph: true,
            supports_workspace_symbols: false,
            max_batch_size: 64,
            partial_reasons: vec![
                PartialReason::Timeout,
                PartialReason::ProviderMissing,
                PartialReason::StartupFailed,
                PartialReason::ResourceLimited,
                PartialReason::ProviderPartial,
            ],
        }
    }

    fn budget(&self) -> ProviderBudget {
        self.budget.clone()
    }

    fn start_session(
        &mut self,
        input: ProviderSessionInput,
    ) -> Result<ProviderSession, ProviderFailure> {
        self.started_roots.push(input.root.id.clone());
        match self.behavior {
            FakeBehavior::StartFailure => Err(ProviderFailure {
                root_id: input.root.id,
                provider_id: self.id.clone(),
                reason: ProviderFailureReason::StartupFailed,
                message: "fake start failure".to_string(),
            }),
            _ => Ok(ProviderSession {
                root_id: input.root.id,
                provider_id: self.id.clone(),
                language: self.language.clone(),
                state: ProviderRootState::Ready,
            }),
        }
    }

    fn resolve_batch(
        &mut self,
        session: &ProviderSession,
        probes: &[SemanticProbe],
    ) -> SemanticBatchResult {
        self.resolved_batches.push(probes.to_vec());
        match &self.behavior {
            FakeBehavior::Timeout => SemanticBatchResult::partial(
                session,
                probes,
                PartialReason::Timeout,
                "fake timeout",
            ),
            FakeBehavior::Partial(reason) => {
                SemanticBatchResult::partial(session, probes, reason.clone(), "fake partial")
            }
            FakeBehavior::ResourceLimited => SemanticBatchResult::partial(
                session,
                probes,
                PartialReason::ResourceLimited,
                "fake memory budget",
            ),
            FakeBehavior::Ok | FakeBehavior::StartFailure => SemanticBatchResult {
                root_id: session.root_id.clone(),
                provider_id: session.provider_id.clone(),
                facts: probes
                    .iter()
                    .map(|probe| NormalizedSemanticFact {
                        root_id: probe.root_id.clone(),
                        language: probe.language.clone(),
                        file: probe.file.clone(),
                        range: probe.range.clone(),
                        symbol: probe.symbol.clone(),
                        kind: probe.kind.clone(),
                        provider_id: session.provider_id.clone(),
                    })
                    .collect(),
                partial: Vec::new(),
            },
        }
    }

    fn shutdown_idle(&mut self, root_id: &str) {
        self.shutdown_roots.push(root_id.to_string());
    }
}

fn root(id: &str, path: &str, language: ProjectLanguage) -> ProjectRoot {
    let kind = match language {
        ProjectLanguage::Go => ProjectRootKind::GoModule,
        ProjectLanguage::Rust => ProjectRootKind::RustCargo,
        ProjectLanguage::Java => ProjectRootKind::JavaMaven,
        ProjectLanguage::TypeScript => ProjectRootKind::TypeScriptConfig,
    };
    ProjectRoot {
        id: id.to_string(),
        path: path.to_string(),
        language,
        kind,
        markers: Vec::new(),
    }
}

fn probe(
    root_id: &str,
    language: ProjectLanguage,
    file: &str,
    priority: QueuePriority,
) -> SemanticProbe {
    SemanticProbe {
        root_id: root_id.to_string(),
        language,
        file: file.to_string(),
        range: SemanticRange {
            start_line: 1,
            start_column: 0,
            end_line: 1,
            end_column: 4,
        },
        kind: SemanticProbeKind::Definition,
        symbol: "main".to_string(),
        priority,
        preferred_provider_id: None,
    }
}

#[test]
fn marks_root_missing_when_provider_is_not_registered() {
    let roots = vec![root("go:api", "api", ProjectLanguage::Go)];
    let probes = vec![probe(
        "go:api",
        ProjectLanguage::Go,
        "api/main.go",
        QueuePriority::CurrentQuery,
    )];

    let mut scheduler = SemanticScheduler::default();
    let report = scheduler.resolve(roots, probes);

    assert_eq!(report.facts.len(), 0);
    assert_eq!(
        report.root_state("go:api"),
        Some(&ProviderRootState::Missing)
    );
    assert_eq!(
        report.partial_reasons("go:api"),
        vec![PartialReason::ProviderMissing]
    );
}

#[test]
fn start_failure_marks_only_that_root_partial() {
    let roots = vec![
        root("go:api", "api", ProjectLanguage::Go),
        root("rust:core", "core", ProjectLanguage::Rust),
    ];
    let probes = vec![
        probe(
            "go:api",
            ProjectLanguage::Go,
            "api/main.go",
            QueuePriority::CurrentQuery,
        ),
        probe(
            "rust:core",
            ProjectLanguage::Rust,
            "core/src/lib.rs",
            QueuePriority::CurrentQuery,
        ),
    ];

    let mut scheduler = SemanticScheduler::default();
    scheduler.register_provider(Box::new(FakeProvider::new(
        "gopls-fake",
        ProjectLanguage::Go,
        FakeBehavior::StartFailure,
    )));
    scheduler.register_provider(Box::new(FakeProvider::new(
        "rust-analyzer-fake",
        ProjectLanguage::Rust,
        FakeBehavior::Ok,
    )));

    let report = scheduler.resolve(roots, probes);

    assert_eq!(
        report.root_state("go:api"),
        Some(&ProviderRootState::Partial)
    );
    assert_eq!(
        report.root_state("rust:core"),
        Some(&ProviderRootState::Ready)
    );
    assert_eq!(report.facts.len(), 1);
    assert_eq!(report.facts[0].root_id, "rust:core");
}

#[test]
fn deduplicates_requests_and_resolves_high_priority_first() {
    let roots = vec![root("rust:core", "core", ProjectLanguage::Rust)];
    let low = probe(
        "rust:core",
        ProjectLanguage::Rust,
        "core/src/lib.rs",
        QueuePriority::BackgroundRefresh,
    );
    let high = SemanticProbe {
        file: "core/src/main.rs".to_string(),
        priority: QueuePriority::CurrentQuery,
        ..low.clone()
    };
    let duplicate_high = high.clone();

    let mut scheduler = SemanticScheduler::default();
    scheduler.register_provider(Box::new(FakeProvider::new(
        "rust-analyzer-fake",
        ProjectLanguage::Rust,
        FakeBehavior::Ok,
    )));

    let report = scheduler.resolve(roots, vec![low, high, duplicate_high]);

    assert_eq!(report.facts.len(), 2);
    assert_eq!(report.facts[0].file, "core/src/main.rs");
    assert_eq!(report.facts[1].file, "core/src/lib.rs");
}

#[test]
fn timeout_and_partial_results_are_reported_as_root_partial() {
    let roots = vec![
        root("rust:core", "core", ProjectLanguage::Rust),
        root("typescript:web", "web", ProjectLanguage::TypeScript),
    ];
    let probes = vec![
        probe(
            "rust:core",
            ProjectLanguage::Rust,
            "core/src/lib.rs",
            QueuePriority::CurrentQuery,
        ),
        probe(
            "typescript:web",
            ProjectLanguage::TypeScript,
            "web/src/app.ts",
            QueuePriority::CurrentQuery,
        ),
    ];

    let mut scheduler = SemanticScheduler::default();
    scheduler.register_provider(Box::new(FakeProvider::new(
        "rust-analyzer-fake",
        ProjectLanguage::Rust,
        FakeBehavior::Timeout,
    )));
    scheduler.register_provider(Box::new(FakeProvider::new(
        "tsserver-fake",
        ProjectLanguage::TypeScript,
        FakeBehavior::Partial(PartialReason::ProviderPartial),
    )));

    let report = scheduler.resolve(roots, probes);

    assert_eq!(
        report.root_state("rust:core"),
        Some(&ProviderRootState::Partial)
    );
    assert_eq!(
        report.partial_reasons("rust:core"),
        vec![PartialReason::Timeout]
    );
    assert_eq!(
        report.partial_reasons("typescript:web"),
        vec![PartialReason::ProviderPartial]
    );
}

#[test]
fn provider_budget_limits_batch_size_and_reports_resource_limit() {
    let roots = vec![root("rust:core", "core", ProjectLanguage::Rust)];
    let budget = ProviderBudget {
        max_concurrent_roots: 1,
        max_concurrent_resolves_per_root: 1,
        single_symbol_timeout_ms: 25,
        idle_shutdown_ms: 1_000,
        max_memory_mb: 64,
    };
    let probes = vec![
        probe(
            "rust:core",
            ProjectLanguage::Rust,
            "core/src/a.rs",
            QueuePriority::CurrentQuery,
        ),
        probe(
            "rust:core",
            ProjectLanguage::Rust,
            "core/src/b.rs",
            QueuePriority::DirtyRoot,
        ),
    ];

    let mut scheduler = SemanticScheduler::default();
    scheduler.register_provider(Box::new(
        FakeProvider::new(
            "rust-analyzer-fake",
            ProjectLanguage::Rust,
            FakeBehavior::ResourceLimited,
        )
        .with_budget(budget),
    ));

    let report = scheduler.resolve(roots, probes);

    assert_eq!(
        report.root_state("rust:core"),
        Some(&ProviderRootState::Partial)
    );
    assert_eq!(
        report.partial_reasons("rust:core"),
        vec![PartialReason::ResourceLimited]
    );
    assert_eq!(report.resolved_probe_count, 1);
    assert_eq!(report.deferred_probe_count, 1);
}
