use std::{
    cell::Cell,
    rc::Rc,
    sync::{Arc, Mutex},
};

use codetrail::{
    project_graph::{ProjectLanguage, ProjectRoot, ProjectRootKind},
    semantic_provider::{
        DeferredReason, NormalizedSemanticFact, PartialReason, ProviderBudget,
        ProviderCapabilities, ProviderFailure, ProviderFailureReason, ProviderRootState,
        ProviderSession, ProviderSessionInput, QueuePriority, SchedulerClock, SemanticBatchResult,
        SemanticProbe, SemanticProbeKind, SemanticProvider, SemanticProviderVersion, SemanticRange,
        SemanticScheduler,
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
    elapsed_ms: u64,
    state: Arc<Mutex<FakeProviderState>>,
}

#[derive(Clone, Debug, Default)]
struct FakeProviderState {
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
            elapsed_ms: 0,
            state: Arc::new(Mutex::new(FakeProviderState::default())),
        }
    }

    fn with_budget(mut self, budget: ProviderBudget) -> Self {
        self.budget = budget;
        self
    }

    fn with_elapsed_ms(mut self, elapsed_ms: u64) -> Self {
        self.elapsed_ms = elapsed_ms;
        self
    }

    fn state(&self) -> Arc<Mutex<FakeProviderState>> {
        Arc::clone(&self.state)
    }
}

#[derive(Clone, Debug)]
struct ManualClock {
    now_ms: Rc<Cell<u64>>,
}

impl ManualClock {
    fn new(now_ms: u64) -> Self {
        Self {
            now_ms: Rc::new(Cell::new(now_ms)),
        }
    }

    fn set(&self, now_ms: u64) {
        self.now_ms.set(now_ms);
    }

    fn advance(&self, elapsed_ms: u64) {
        self.now_ms.set(self.now_ms.get() + elapsed_ms);
    }
}

impl SchedulerClock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.now_ms.get()
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
        self.state
            .lock()
            .unwrap()
            .started_roots
            .push(input.root.id.clone());
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
        self.state
            .lock()
            .unwrap()
            .resolved_batches
            .push(probes.to_vec());
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
        self.state
            .lock()
            .unwrap()
            .shutdown_roots
            .push(root_id.to_string());
    }
}

#[derive(Clone, Debug)]
struct TimedFakeProvider {
    inner: FakeProvider,
    clock: ManualClock,
}

impl TimedFakeProvider {
    fn new(inner: FakeProvider, clock: ManualClock) -> Self {
        Self { inner, clock }
    }
}

impl SemanticProvider for TimedFakeProvider {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn language(&self) -> ProjectLanguage {
        self.inner.language()
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.inner.capabilities()
    }

    fn budget(&self) -> ProviderBudget {
        self.inner.budget()
    }

    fn start_session(
        &mut self,
        input: ProviderSessionInput,
    ) -> Result<ProviderSession, ProviderFailure> {
        self.inner.start_session(input)
    }

    fn resolve_batch(
        &mut self,
        session: &ProviderSession,
        probes: &[SemanticProbe],
    ) -> SemanticBatchResult {
        let result = self.inner.resolve_batch(session, probes);
        self.clock.advance(self.inner.elapsed_ms);
        result
    }

    fn shutdown_idle(&mut self, root_id: &str) {
        self.inner.shutdown_idle(root_id);
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

#[test]
fn provider_budget_limits_roots_per_pass_and_defers_the_rest() {
    let roots = vec![
        root("rust:a", "a", ProjectLanguage::Rust),
        root("rust:b", "b", ProjectLanguage::Rust),
        root("rust:c", "c", ProjectLanguage::Rust),
    ];
    let budget = ProviderBudget {
        max_concurrent_roots: 1,
        max_concurrent_resolves_per_root: 8,
        single_symbol_timeout_ms: 25,
        idle_shutdown_ms: 1_000,
        max_memory_mb: 512,
    };
    let provider = FakeProvider::new(
        "rust-analyzer-fake",
        ProjectLanguage::Rust,
        FakeBehavior::Ok,
    )
    .with_budget(budget);
    let provider_state = provider.state();

    let mut scheduler = SemanticScheduler::default();
    scheduler.register_provider(Box::new(provider));
    let report = scheduler.resolve(
        roots,
        vec![
            probe(
                "rust:a",
                ProjectLanguage::Rust,
                "a/src/lib.rs",
                QueuePriority::CurrentQuery,
            ),
            probe(
                "rust:b",
                ProjectLanguage::Rust,
                "b/src/lib.rs",
                QueuePriority::DirtyRoot,
            ),
            probe(
                "rust:c",
                ProjectLanguage::Rust,
                "c/src/lib.rs",
                QueuePriority::BackgroundRefresh,
            ),
        ],
    );

    let state = provider_state.lock().unwrap();
    assert_eq!(state.started_roots, vec!["rust:a"]);
    assert_eq!(report.resolved_probe_count, 1);
    assert_eq!(report.deferred_probe_count, 2);
    assert_eq!(report.deferred.len(), 2);
    assert!(report
        .deferred
        .iter()
        .all(|deferred| deferred.reason == DeferredReason::MaxConcurrentRoots));
}

#[test]
fn scheduler_reports_timeout_when_elapsed_time_exceeds_provider_budget() {
    let clock = ManualClock::new(10);
    let roots = vec![root("rust:core", "core", ProjectLanguage::Rust)];
    let budget = ProviderBudget {
        max_concurrent_roots: 1,
        max_concurrent_resolves_per_root: 8,
        single_symbol_timeout_ms: 25,
        idle_shutdown_ms: 1_000,
        max_memory_mb: 512,
    };
    let provider = FakeProvider::new(
        "rust-analyzer-fake",
        ProjectLanguage::Rust,
        FakeBehavior::Ok,
    )
    .with_budget(budget)
    .with_elapsed_ms(40);

    let mut scheduler = SemanticScheduler::with_clock(Box::new(clock.clone()));
    scheduler.register_provider(Box::new(TimedFakeProvider::new(provider, clock)));

    let report = scheduler.resolve(
        roots,
        vec![probe(
            "rust:core",
            ProjectLanguage::Rust,
            "core/src/lib.rs",
            QueuePriority::CurrentQuery,
        )],
    );

    assert_eq!(
        report.root_state("rust:core"),
        Some(&ProviderRootState::Partial)
    );
    assert_eq!(
        report.partial_reasons("rust:core"),
        vec![PartialReason::Timeout]
    );
    assert_eq!(report.facts.len(), 0);
}

#[test]
fn idle_shutdown_uses_provider_budget_threshold() {
    let clock = ManualClock::new(100);
    let roots = vec![root("rust:core", "core", ProjectLanguage::Rust)];
    let budget = ProviderBudget {
        max_concurrent_roots: 1,
        max_concurrent_resolves_per_root: 8,
        single_symbol_timeout_ms: 25,
        idle_shutdown_ms: 1_000,
        max_memory_mb: 512,
    };
    let provider = FakeProvider::new(
        "rust-analyzer-fake",
        ProjectLanguage::Rust,
        FakeBehavior::Ok,
    )
    .with_budget(budget);
    let provider_state = provider.state();

    let mut scheduler = SemanticScheduler::with_clock(Box::new(clock.clone()));
    scheduler.register_provider(Box::new(provider));
    let report = scheduler.resolve(
        roots,
        vec![probe(
            "rust:core",
            ProjectLanguage::Rust,
            "core/src/lib.rs",
            QueuePriority::CurrentQuery,
        )],
    );
    assert_eq!(
        report.root_state("rust:core"),
        Some(&ProviderRootState::Ready)
    );

    clock.set(1_099);
    assert_eq!(scheduler.shutdown_idle(), 0);
    assert!(provider_state.lock().unwrap().shutdown_roots.is_empty());

    clock.set(1_101);
    assert_eq!(scheduler.shutdown_idle(), 1);
    assert_eq!(
        provider_state.lock().unwrap().shutdown_roots,
        vec!["rust:core"]
    );
}

#[test]
fn warm_session_report_keeps_provider_and_ready_state_history() {
    let roots = vec![root("rust:core", "core", ProjectLanguage::Rust)];
    let provider = FakeProvider::new("shared-id", ProjectLanguage::Rust, FakeBehavior::Ok);
    let provider_state = provider.state();

    let mut scheduler = SemanticScheduler::default();
    scheduler.register_provider(Box::new(provider));
    let first = scheduler.resolve(
        roots.clone(),
        vec![probe(
            "rust:core",
            ProjectLanguage::Rust,
            "core/src/lib.rs",
            QueuePriority::CurrentQuery,
        )],
    );
    assert_eq!(
        first.root_state("rust:core"),
        Some(&ProviderRootState::Ready)
    );

    let second = scheduler.resolve(
        roots,
        vec![probe(
            "rust:core",
            ProjectLanguage::Rust,
            "core/src/main.rs",
            QueuePriority::CurrentQuery,
        )],
    );

    let report = second.root_reports.get("rust:core").unwrap();
    assert_eq!(report.provider_id.as_deref(), Some("shared-id"));
    assert_eq!(
        report.state_history,
        vec![
            ProviderRootState::Ready,
            ProviderRootState::Resolving,
            ProviderRootState::Ready,
        ]
    );
    assert_eq!(
        provider_state.lock().unwrap().started_roots,
        vec!["rust:core"]
    );
}

#[test]
fn idle_shutdown_uses_language_and_provider_id_for_exact_provider() {
    let clock = ManualClock::new(100);
    let budget = ProviderBudget {
        max_concurrent_roots: 1,
        max_concurrent_resolves_per_root: 8,
        single_symbol_timeout_ms: 25,
        idle_shutdown_ms: 1_000,
        max_memory_mb: 512,
    };
    let go_provider = FakeProvider::new("shared-id", ProjectLanguage::Go, FakeBehavior::Ok)
        .with_budget(budget.clone());
    let go_state = go_provider.state();
    let rust_provider =
        FakeProvider::new("shared-id", ProjectLanguage::Rust, FakeBehavior::Ok).with_budget(budget);
    let rust_state = rust_provider.state();

    let mut scheduler = SemanticScheduler::with_clock(Box::new(clock.clone()));
    scheduler.register_provider(Box::new(go_provider));
    scheduler.register_provider(Box::new(rust_provider));
    let report = scheduler.resolve(
        vec![
            root("go:api", "api", ProjectLanguage::Go),
            root("rust:core", "core", ProjectLanguage::Rust),
        ],
        vec![probe(
            "rust:core",
            ProjectLanguage::Rust,
            "core/src/lib.rs",
            QueuePriority::CurrentQuery,
        )],
    );
    assert_eq!(
        report.root_state("rust:core"),
        Some(&ProviderRootState::Ready)
    );

    clock.set(1_101);
    assert_eq!(scheduler.shutdown_idle(), 1);
    assert!(go_state.lock().unwrap().shutdown_roots.is_empty());
    assert_eq!(rust_state.lock().unwrap().shutdown_roots, vec!["rust:core"]);
}
