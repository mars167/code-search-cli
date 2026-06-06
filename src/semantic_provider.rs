//! Internal semantic provider contract and scheduler skeleton.
//!
//! LCI language adapters must obey this contract:
//! - Providers are rooted by the LCI-01 `ProjectRoot::id` and must report their
//!   language, provider version, capabilities, budget, and supported partial
//!   reasons before a session is used.
//! - A provider session owns root discovery/import state for exactly one project
//!   root. Providers return normalized semantic facts only; public JSON
//!   rendering, fallback behavior, freshness gates, and batch query UX remain in
//!   CodeTrail core or later LCI tasks.
//! - `SemanticScheduler` owns queue priority, request de-duplication,
//!   per-provider/per-root budgets, single-symbol timeout reporting, and idle
//!   shutdown. Providers may also report partial work, but elapsed-time timeout
//!   attribution is measured by the scheduler clock and surfaced with
//!   `started_at_ms`/`elapsed_ms` on `SemanticPartial`.
//! - Provider failures are root-scoped. A missing, failed, timed-out, or partial
//!   provider marks only the affected root as partial/missing and does not fail
//!   the whole workspace semantic index.
//!
//! Root state machine:
//! - `missing -> starting -> importing -> ready`
//! - `ready -> resolving -> ready`
//! - `ready -> stale -> resolving`
//! - `starting | importing | resolving -> partial`
//! - `partial -> retrying -> ready | partial`

use std::{
    collections::{BTreeMap, BTreeSet},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::project_graph::{ProjectLanguage, ProjectRoot};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticProviderVersion {
    pub name: String,
    pub version: String,
    pub protocol_version: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCapabilities {
    pub language: ProjectLanguage,
    pub provider_version: SemanticProviderVersion,
    pub supports_batch_resolve: bool,
    pub supports_import_graph: bool,
    pub supports_workspace_symbols: bool,
    pub max_batch_size: usize,
    pub partial_reasons: Vec<PartialReason>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderBudget {
    pub max_concurrent_roots: usize,
    pub max_concurrent_resolves_per_root: usize,
    pub single_symbol_timeout_ms: u64,
    pub idle_shutdown_ms: u64,
    pub max_memory_mb: usize,
}

impl Default for ProviderBudget {
    fn default() -> Self {
        Self {
            max_concurrent_roots: 1,
            max_concurrent_resolves_per_root: 16,
            single_symbol_timeout_ms: 250,
            idle_shutdown_ms: 30_000,
            max_memory_mb: 1024,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PartialReason {
    ProviderMissing,
    StartupFailed,
    Timeout,
    ResourceLimited,
    ProviderPartial,
    UnsupportedCapability,
    ResolveFailed,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderFailureReason {
    ProviderMissing,
    StartupFailed,
    Timeout,
    ResourceLimited,
    UnsupportedCapability,
    ResolveFailed,
}

impl ProviderFailureReason {
    fn partial_reason(&self) -> PartialReason {
        match self {
            ProviderFailureReason::ProviderMissing => PartialReason::ProviderMissing,
            ProviderFailureReason::StartupFailed => PartialReason::StartupFailed,
            ProviderFailureReason::Timeout => PartialReason::Timeout,
            ProviderFailureReason::ResourceLimited => PartialReason::ResourceLimited,
            ProviderFailureReason::UnsupportedCapability => PartialReason::UnsupportedCapability,
            ProviderFailureReason::ResolveFailed => PartialReason::ResolveFailed,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderFailure {
    pub root_id: String,
    pub provider_id: String,
    pub reason: ProviderFailureReason,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderRootState {
    Missing,
    Starting,
    Importing,
    Ready,
    Resolving,
    Stale,
    Partial,
    Retrying,
}

impl ProviderRootState {
    pub fn can_transition_to(&self, next: &ProviderRootState) -> bool {
        matches!(
            (self, next),
            (ProviderRootState::Missing, ProviderRootState::Starting)
                | (ProviderRootState::Starting, ProviderRootState::Importing)
                | (ProviderRootState::Importing, ProviderRootState::Ready)
                | (ProviderRootState::Ready, ProviderRootState::Resolving)
                | (ProviderRootState::Resolving, ProviderRootState::Ready)
                | (ProviderRootState::Ready, ProviderRootState::Stale)
                | (ProviderRootState::Stale, ProviderRootState::Resolving)
                | (ProviderRootState::Starting, ProviderRootState::Partial)
                | (ProviderRootState::Importing, ProviderRootState::Partial)
                | (ProviderRootState::Resolving, ProviderRootState::Partial)
                | (ProviderRootState::Partial, ProviderRootState::Retrying)
                | (ProviderRootState::Retrying, ProviderRootState::Ready)
                | (ProviderRootState::Retrying, ProviderRootState::Partial)
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueuePriority {
    CurrentQuery,
    DirtyRoot,
    StagedRoot,
    ConfigAffectedRoot,
    RecentSymbol,
    BackgroundRefresh,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticRange {
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticProbeKind {
    Definition,
    Reference,
    Call,
    Type,
    Import,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticProbe {
    pub root_id: String,
    pub language: ProjectLanguage,
    pub file: String,
    pub range: SemanticRange,
    pub kind: SemanticProbeKind,
    pub symbol: String,
    pub priority: QueuePriority,
    pub preferred_provider_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSessionInput {
    pub root: ProjectRoot,
    pub provider_id: String,
    pub language: ProjectLanguage,
    pub budget: ProviderBudget,
    pub source_files: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSession {
    pub root_id: String,
    pub provider_id: String,
    pub language: ProjectLanguage,
    pub state: ProviderRootState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NormalizedSemanticFact {
    pub root_id: String,
    pub language: ProjectLanguage,
    pub file: String,
    pub range: SemanticRange,
    pub symbol: String,
    pub kind: SemanticProbeKind,
    pub provider_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticPartial {
    pub root_id: String,
    pub provider_id: String,
    pub reason: PartialReason,
    pub message: String,
    pub started_at_ms: Option<u64>,
    pub elapsed_ms: Option<u64>,
    pub probe: Option<SemanticProbe>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticBatchResult {
    pub root_id: String,
    pub provider_id: String,
    pub facts: Vec<NormalizedSemanticFact>,
    pub partial: Vec<SemanticPartial>,
}

impl SemanticBatchResult {
    pub fn partial(
        session: &ProviderSession,
        probes: &[SemanticProbe],
        reason: PartialReason,
        message: impl Into<String>,
    ) -> Self {
        let message = message.into();
        Self {
            root_id: session.root_id.clone(),
            provider_id: session.provider_id.clone(),
            facts: Vec::new(),
            partial: probes
                .iter()
                .cloned()
                .map(|probe| SemanticPartial {
                    root_id: session.root_id.clone(),
                    provider_id: session.provider_id.clone(),
                    reason: reason.clone(),
                    message: message.clone(),
                    started_at_ms: None,
                    elapsed_ms: None,
                    probe: Some(probe),
                })
                .collect(),
        }
    }
}

pub trait SchedulerClock {
    fn now_ms(&self) -> u64;
}

#[derive(Clone, Debug, Default)]
struct SystemSchedulerClock;

impl SchedulerClock for SystemSchedulerClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or_default()
    }
}

pub trait SemanticProvider {
    fn id(&self) -> &str;
    fn language(&self) -> ProjectLanguage;
    fn capabilities(&self) -> ProviderCapabilities;
    fn budget(&self) -> ProviderBudget;
    fn start_session(
        &mut self,
        input: ProviderSessionInput,
    ) -> Result<ProviderSession, ProviderFailure>;
    fn resolve_batch(
        &mut self,
        session: &ProviderSession,
        probes: &[SemanticProbe],
    ) -> SemanticBatchResult;
    fn shutdown_idle(&mut self, root_id: &str);
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRootReport {
    pub root_id: String,
    pub provider_id: Option<String>,
    pub state: ProviderRootState,
    pub state_history: Vec<ProviderRootState>,
    pub partial: Vec<SemanticPartial>,
    pub failures: Vec<ProviderFailure>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeferredReason {
    MaxConcurrentRoots,
    MaxConcurrentResolvesPerRoot,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeferredSemanticWork {
    pub root_id: String,
    pub provider_id: String,
    pub reason: DeferredReason,
    pub probe_count: usize,
}

impl ProviderRootReport {
    fn new(root_id: &str) -> Self {
        Self {
            root_id: root_id.to_string(),
            provider_id: None,
            state: ProviderRootState::Missing,
            state_history: Vec::new(),
            partial: Vec::new(),
            failures: Vec::new(),
        }
    }

    fn transition(&mut self, state: ProviderRootState) {
        self.state = state.clone();
        if self.state_history.last() != Some(&state) {
            self.state_history.push(state);
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticSchedulerReport {
    pub facts: Vec<NormalizedSemanticFact>,
    pub partial: Vec<SemanticPartial>,
    pub deferred: Vec<DeferredSemanticWork>,
    pub root_reports: BTreeMap<String, ProviderRootReport>,
    pub resolved_probe_count: usize,
    pub deferred_probe_count: usize,
}

impl SemanticSchedulerReport {
    pub fn root_state(&self, root_id: &str) -> Option<&ProviderRootState> {
        self.root_reports.get(root_id).map(|report| &report.state)
    }

    pub fn partial_reasons(&self, root_id: &str) -> Vec<PartialReason> {
        self.root_reports
            .get(root_id)
            .map(|report| {
                report
                    .partial
                    .iter()
                    .map(|partial| partial.reason.clone())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect()
            })
            .unwrap_or_default()
    }

    fn root_report(&mut self, root_id: &str) -> &mut ProviderRootReport {
        self.root_reports
            .entry(root_id.to_string())
            .or_insert_with(|| ProviderRootReport::new(root_id))
    }

    fn transition(&mut self, root_id: &str, state: ProviderRootState) {
        self.root_report(root_id).transition(state);
    }

    fn add_partial(&mut self, partial: SemanticPartial) {
        self.root_report(&partial.root_id)
            .partial
            .push(partial.clone());
        self.partial.push(partial);
    }

    fn add_failure(&mut self, failure: ProviderFailure) {
        let partial = SemanticPartial {
            root_id: failure.root_id.clone(),
            provider_id: failure.provider_id.clone(),
            reason: failure.reason.partial_reason(),
            message: failure.message.clone(),
            started_at_ms: None,
            elapsed_ms: None,
            probe: None,
        };
        self.root_report(&failure.root_id)
            .failures
            .push(failure.clone());
        self.add_partial(partial);
    }

    fn defer_probes(
        &mut self,
        root_id: String,
        provider_id: String,
        reason: DeferredReason,
        probe_count: usize,
    ) {
        if probe_count == 0 {
            return;
        }
        self.deferred_probe_count += probe_count;
        self.deferred.push(DeferredSemanticWork {
            root_id,
            provider_id,
            reason,
            probe_count,
        });
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ProviderKey {
    language: ProjectLanguage,
    id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ProbeKey {
    root_id: String,
    language: ProjectLanguage,
    provider_id: String,
    file: String,
    range: SemanticRange,
    kind: SemanticProbeKind,
}

#[derive(Clone, Debug)]
struct QueuedProbe {
    provider_id: String,
    probe: SemanticProbe,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct SessionKey {
    provider_id: String,
    root_id: String,
}

#[derive(Clone, Debug)]
struct ProviderSessionRecord {
    session: ProviderSession,
    budget: ProviderBudget,
    last_used_ms: u64,
}

#[derive(Clone, Debug)]
struct RootProbeGroup {
    provider_id: String,
    root_id: String,
    probes: Vec<SemanticProbe>,
}

pub struct SemanticScheduler {
    providers: BTreeMap<ProviderKey, Box<dyn SemanticProvider>>,
    sessions: BTreeMap<SessionKey, ProviderSessionRecord>,
    clock: Box<dyn SchedulerClock>,
}

impl Default for SemanticScheduler {
    fn default() -> Self {
        Self {
            providers: BTreeMap::new(),
            sessions: BTreeMap::new(),
            clock: Box::<SystemSchedulerClock>::default(),
        }
    }
}

impl SemanticScheduler {
    pub fn with_clock(clock: Box<dyn SchedulerClock>) -> Self {
        Self {
            providers: BTreeMap::new(),
            sessions: BTreeMap::new(),
            clock,
        }
    }

    pub fn register_provider(&mut self, provider: Box<dyn SemanticProvider>) {
        let key = ProviderKey {
            language: provider.language(),
            id: provider.id().to_string(),
        };
        self.providers.insert(key, provider);
    }

    pub fn resolve(
        &mut self,
        roots: Vec<ProjectRoot>,
        probes: Vec<SemanticProbe>,
    ) -> SemanticSchedulerReport {
        let roots_by_id = roots
            .into_iter()
            .map(|root| (root.id.clone(), root))
            .collect::<BTreeMap<_, _>>();
        let mut report = SemanticSchedulerReport::default();
        let queued = self.deduplicate_probes(probes, &mut report);
        let groups = group_queued_probes(queued);

        let mut provider_root_counts = BTreeMap::<ProviderKey, usize>::new();

        for group in groups {
            let provider_id = group.provider_id;
            let root_id = group.root_id;
            let probes = group.probes;
            let Some(root) = roots_by_id.get(&root_id) else {
                report.transition(&root_id, ProviderRootState::Missing);
                report.add_partial(SemanticPartial {
                    root_id,
                    provider_id,
                    reason: PartialReason::ProviderMissing,
                    message: "probe root is absent from project graph".to_string(),
                    started_at_ms: None,
                    elapsed_ms: None,
                    probe: None,
                });
                continue;
            };
            let provider_key = ProviderKey {
                language: root.language.clone(),
                id: provider_id.clone(),
            };
            let Some(budget) = self
                .providers
                .get(&provider_key)
                .map(|provider| provider.budget())
            else {
                report.transition(&root_id, ProviderRootState::Missing);
                report.add_partial(SemanticPartial {
                    root_id,
                    provider_id,
                    reason: PartialReason::ProviderMissing,
                    message: "semantic provider is not registered".to_string(),
                    started_at_ms: None,
                    elapsed_ms: None,
                    probe: None,
                });
                continue;
            };
            let root_limit = budget.max_concurrent_roots.max(1);
            let active_roots = provider_root_counts
                .entry(provider_key.clone())
                .or_default();
            if *active_roots >= root_limit {
                report.defer_probes(
                    root_id,
                    provider_id,
                    DeferredReason::MaxConcurrentRoots,
                    probes.len(),
                );
                continue;
            }
            *active_roots += 1;

            let resolve_limit = budget.max_concurrent_resolves_per_root.max(1);
            let active_count = probes.len().min(resolve_limit);
            let (active, deferred) = probes.split_at(active_count);
            report.defer_probes(
                root_id.clone(),
                provider_id.clone(),
                DeferredReason::MaxConcurrentResolvesPerRoot,
                deferred.len(),
            );
            if active.is_empty() {
                continue;
            }

            let Some(session) = self.ensure_session(
                root,
                &provider_key,
                &provider_id,
                &budget,
                active,
                &mut report,
            ) else {
                continue;
            };

            report.transition(&root_id, ProviderRootState::Resolving);
            let started_at_ms = self.clock.now_ms();
            let batch = self
                .providers
                .get_mut(&provider_key)
                .expect("provider existed before session start")
                .resolve_batch(&session, active);
            let finished_at_ms = self.clock.now_ms();
            let elapsed_ms = finished_at_ms.saturating_sub(started_at_ms);
            report.resolved_probe_count += active.len();
            if elapsed_ms > budget.single_symbol_timeout_ms {
                for probe in active.iter().cloned() {
                    report.add_partial(SemanticPartial {
                        root_id: root_id.clone(),
                        provider_id: provider_id.clone(),
                        reason: PartialReason::Timeout,
                        message: format!(
                            "semantic resolve exceeded single-symbol timeout budget ({}ms > {}ms)",
                            elapsed_ms, budget.single_symbol_timeout_ms
                        ),
                        started_at_ms: Some(started_at_ms),
                        elapsed_ms: Some(elapsed_ms),
                        probe: Some(probe),
                    });
                }
            } else {
                report.facts.extend(batch.facts);
                for mut partial in batch.partial {
                    partial.started_at_ms.get_or_insert(started_at_ms);
                    partial.elapsed_ms.get_or_insert(elapsed_ms);
                    report.add_partial(partial);
                }
            }
            if let Some(record) = self.sessions.get_mut(&SessionKey {
                provider_id: provider_id.clone(),
                root_id: root_id.clone(),
            }) {
                record.last_used_ms = finished_at_ms;
            }
            if report
                .root_reports
                .get(&root_id)
                .map(|root_report| root_report.partial.is_empty())
                .unwrap_or(true)
            {
                report.transition(&root_id, ProviderRootState::Ready);
            } else {
                report.transition(&root_id, ProviderRootState::Partial);
            }
        }

        report
    }

    pub fn mark_stale(&mut self, root_id: &str) -> ProviderRootReport {
        let mut report = ProviderRootReport::new(root_id);
        report.transition(ProviderRootState::Ready);
        report.transition(ProviderRootState::Stale);
        report
    }

    pub fn retry_partial(&mut self, root_id: &str) -> ProviderRootReport {
        let mut report = ProviderRootReport::new(root_id);
        report.transition(ProviderRootState::Partial);
        report.transition(ProviderRootState::Retrying);
        report
    }

    pub fn shutdown_idle_roots<'a>(&mut self, root_ids: impl IntoIterator<Item = &'a str>) {
        for root_id in root_ids {
            let session_keys = self
                .sessions
                .keys()
                .filter(|session_key| session_key.root_id == root_id)
                .cloned()
                .collect::<Vec<_>>();
            for session_key in session_keys {
                for provider in self.providers.values_mut() {
                    if provider.id() == session_key.provider_id {
                        provider.shutdown_idle(root_id);
                    }
                }
                self.sessions.remove(&session_key);
            }
        }
    }

    pub fn shutdown_idle(&mut self) -> usize {
        let now_ms = self.clock.now_ms();
        let session_keys = self
            .sessions
            .iter()
            .filter_map(|(session_key, record)| {
                let idle_ms = now_ms.saturating_sub(record.last_used_ms);
                (idle_ms >= record.budget.idle_shutdown_ms).then(|| session_key.clone())
            })
            .collect::<Vec<_>>();
        let shutdown_count = session_keys.len();
        for session_key in session_keys {
            for provider in self.providers.values_mut() {
                if provider.id() == session_key.provider_id {
                    provider.shutdown_idle(&session_key.root_id);
                }
            }
            self.sessions.remove(&session_key);
        }
        shutdown_count
    }

    fn ensure_session(
        &mut self,
        root: &ProjectRoot,
        provider_key: &ProviderKey,
        provider_id: &str,
        budget: &ProviderBudget,
        active: &[SemanticProbe],
        report: &mut SemanticSchedulerReport,
    ) -> Option<ProviderSession> {
        let session_key = SessionKey {
            provider_id: provider_id.to_string(),
            root_id: root.id.clone(),
        };
        if let Some(record) = self.sessions.get_mut(&session_key) {
            record.last_used_ms = self.clock.now_ms();
            return Some(record.session.clone());
        }

        report.root_report(&root.id).provider_id = Some(provider_id.to_string());
        report.transition(&root.id, ProviderRootState::Starting);
        let source_files = active
            .iter()
            .map(|probe| probe.file.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let input = ProviderSessionInput {
            root: root.clone(),
            provider_id: provider_id.to_string(),
            language: root.language.clone(),
            budget: budget.clone(),
            source_files,
        };
        let start_result = self
            .providers
            .get_mut(provider_key)
            .expect("provider existed before session start")
            .start_session(input);
        match start_result {
            Ok(mut session) => {
                report.transition(&root.id, ProviderRootState::Importing);
                session.state = ProviderRootState::Ready;
                report.transition(&root.id, ProviderRootState::Ready);
                self.sessions.insert(
                    session_key,
                    ProviderSessionRecord {
                        session: session.clone(),
                        budget: budget.clone(),
                        last_used_ms: self.clock.now_ms(),
                    },
                );
                Some(session)
            }
            Err(failure) => {
                report.add_failure(failure);
                report.transition(&root.id, ProviderRootState::Partial);
                None
            }
        }
    }

    fn deduplicate_probes(
        &self,
        probes: Vec<SemanticProbe>,
        report: &mut SemanticSchedulerReport,
    ) -> Vec<QueuedProbe> {
        let mut unique = BTreeMap::<ProbeKey, QueuedProbe>::new();
        for probe in probes {
            let Some(provider_id) =
                self.provider_id_for(&probe.language, probe.preferred_provider_id.as_deref())
            else {
                report.transition(&probe.root_id, ProviderRootState::Missing);
                report.add_partial(SemanticPartial {
                    root_id: probe.root_id,
                    provider_id: probe
                        .preferred_provider_id
                        .unwrap_or_else(|| "unregistered".to_string()),
                    reason: PartialReason::ProviderMissing,
                    message: "semantic provider is not registered".to_string(),
                    started_at_ms: None,
                    elapsed_ms: None,
                    probe: None,
                });
                continue;
            };
            let key = ProbeKey {
                root_id: probe.root_id.clone(),
                language: probe.language.clone(),
                provider_id: provider_id.clone(),
                file: probe.file.clone(),
                range: probe.range.clone(),
                kind: probe.kind.clone(),
            };
            match unique.get_mut(&key) {
                Some(existing) if probe.priority < existing.probe.priority => {
                    existing.probe = probe;
                }
                Some(_) => {}
                None => {
                    unique.insert(key, QueuedProbe { provider_id, probe });
                }
            }
        }
        let mut queued = unique.into_values().collect::<Vec<_>>();
        queued.sort_by(|left, right| {
            left.probe
                .priority
                .cmp(&right.probe.priority)
                .then_with(|| left.probe.root_id.cmp(&right.probe.root_id))
                .then_with(|| left.probe.file.cmp(&right.probe.file))
        });
        queued
    }

    fn provider_id_for(
        &self,
        language: &ProjectLanguage,
        preferred: Option<&str>,
    ) -> Option<String> {
        if let Some(preferred) = preferred {
            let key = ProviderKey {
                language: language.clone(),
                id: preferred.to_string(),
            };
            return self
                .providers
                .contains_key(&key)
                .then(|| preferred.to_string());
        }
        self.providers
            .keys()
            .find(|key| &key.language == language)
            .map(|key| key.id.clone())
    }
}

fn group_queued_probes(queued: Vec<QueuedProbe>) -> Vec<RootProbeGroup> {
    let mut group_indexes = BTreeMap::<(String, String), usize>::new();
    let mut groups = Vec::<RootProbeGroup>::new();
    for queued_probe in queued {
        let key = (
            queued_probe.provider_id.clone(),
            queued_probe.probe.root_id.clone(),
        );
        if let Some(index) = group_indexes.get(&key) {
            groups[*index].probes.push(queued_probe.probe);
        } else {
            group_indexes.insert(key, groups.len());
            groups.push(RootProbeGroup {
                provider_id: queued_probe.provider_id,
                root_id: queued_probe.probe.root_id.clone(),
                probes: vec![queued_probe.probe],
            });
        }
    }
    groups
}
