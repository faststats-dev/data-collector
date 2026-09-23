# Replay pipeline architecture TODO

This is an ordered delivery plan for the replay system in this repository and `../monorepo`. Checked boxes record implemented and locally verified work; unchecked deployment acceptance remains required. Findings come from source inspection, not production benchmarks. The target is a fast, horizontally scalable system with bounded resource use, explicit failure recovery, and maintainable code; completion requires measured evidence, not an architectural claim.

## Delivery contract

Implement **step 1, ship it, then step 2, ship it**, and continue in numerical order. Each numbered step is a complete production release: its schema, infrastructure, producers, consumers, APIs, UI changes where applicable, tests, runbooks, and cutover belong to that step. Dependencies may point only to earlier steps. A step can remain deployed indefinitely without a later step to make it correct. Steps are release units, not promises of one day's work; split an oversized step only into equally complete vertical releases.

Existing behavior may remain outside a step's scope. Do not introduce disposable queues, fake successful results, direct-write shortcuts, silent truncation, unbounded retries, or parallel implementations that a later step must repair. Compatibility is not a target-design constraint, but deployment must preserve accepted data and product behavior: use tested migrations and explicit maintenance windows where needed. Finish and remove a step's replaced paths in that release after its rollback window; do not defer cleanup to a final rewrite.

Every step must:

- [ ] Record prerequisites, owner, deployment order, measurable acceptance thresholds, and a rollback or roll-forward procedure before implementation.
- [ ] Refactor touched code into clear domain, storage, orchestration, and provider boundaries; keep transactions and resource ownership explicit.
- [ ] Use typed/versioned contracts, structured errors, bounded configuration, and shared Rust/TypeScript golden fixtures for identities, filters, detectors, and timestamps. Do not duplicate business rules across services.
- [ ] Pass formatting, lint/type checks, focused behavioral tests, migration checks, and failure tests appropriate to the changed boundary in both affected repositories. No unrelated rewrites, suppressed checks, or implementation-mirroring tests.
- [ ] Demonstrate the release's useful production behavior with later steps absent. Include its support procedure and capacity limits.
- [ ] Remove superseded code/configuration and document the resulting ownership. Temporary rollout flags must have an owner and removal condition within the release.
- [ ] Maintain a removal checklist naming every replaced code path, schema/index, worker, topic/consumer group, object location, environment variable, secret/permission and deployment resource. Each item must be removed or justified as a tested permanent part of the target architecture before the owning release closes.
- [ ] Close implementation decisions with code and evidence in their owning step. An unimplemented dependency, unresolved correctness defect, manual recurring repair or “clean up later” task blocks completion. Historical applied migrations remain immutable records; obsolete live schema and runtime behavior do not.

## Non-negotiable target architecture

| Responsibility | Target owner | Contract |
| --- | --- | --- |
| Compressed replay payloads, immutable manifests, render/evidence artifacts | Object storage | Verified immutable identity, one central replay bucket, reconciliation, retention |
| Recording lifecycle, deletion/generation fences, settings, mutable product state | PostgreSQL | Short transactions; authoritative state; shardable operational data |
| Durable analysis orchestration | Temporal Cloud with the native Rust SDK | One workflow owner per execution; independently scaled Activity workers; PostgreSQL remains publication authority |
| Summary text, findings, manual assignments, artifact/attempt references | PostgreSQL | Atomic publication and transactional outbox |
| Replay analytics, detailed event facts, filters, heatmap aggregates | ClickHouse | Rebuildable Kafka-fed projections; explicit deduplication, revision, deletion, and query semantics |
| Ingestion and all external ClickHouse data delivery | Kafka | Durable acknowledgment, partition-safe offsets, bounded backpressure, quarantine and replay |
| Similarity retrieval | pgvector | Explicit versioned representatives; indexed search only after recall/latency evaluation |
| Shared replay storage and workload isolation | One central replay bucket and tenant-aware admission | No per-project bucket provisioning; preserve project authorization and resource fairness |

**Kafka-only ClickHouse ingestion:** every external replay row, state update, tombstone, repair, and historical backfill must first be published to a versioned Kafka topic. Use Kafka-engine tables and materialized views, matching the existing monorepo ingestion pattern. Applications, workers, CLI scripts, and data migrations never insert replay data directly into ClickHouse, including during outages. Internal materialized-view writes are the sink, not a bypass. DDL and managed deletion/TTL operations are administration, not alternative ingestion paths. Give application/query credentials no INSERT privileges; reserve administrative capabilities for managed migrations/deletion operations. Queue durably if Kafka or the sink is unavailable.

Retain Rust processing, Kafka, compressed object storage, generation fences, execution tokens where required for publication, child-process supervision, bounded downloads, model-output validation, and incremental detector checkpoints. Temporal must not replace Kafka streaming or make ingestion wait for analysis.

**Confirmed storage direction:** “remove the multi tenant replay model” means **remove per-project buckets and provisioning**. Use exactly one central replay bucket for raw payloads, manifests and derived artifacts, with project/generation/type prefixes and explicit object references. No bucket routing by region, project or storage policy is part of this plan. Keep project authorization, retention and deletion isolation. Step 2 includes every reader/writer, lifecycle worker and provisioning cleanup needed to ship this change; it does not require dedicated tenant cells or a database per project.

## Infrastructure gaps and recommendations

“Missing” below means absent from the inspected replay code, manifests, or local deployment configuration; production infrastructure outside these repositories was not audited. Existing local services are PostgreSQL/pgvector, Kafka, ClickHouse, Redis, and an S3-compatible store. Existing ClickHouse SQL already uses Kafka tables and materialized views. These services do not need wholesale replacement.

| Gap or unverified capability | Recommendation | Delivered by |
| --- | --- | --- |
| Enforced renderer sandbox/egress/resource boundary | Isolated container/job workers with OS sandboxing, deny-by-default egress, no provider/database credentials, and hard resource limits | 1 |
| Immutable storage, inventory reconciliation, consistent retention/deletion | Production-supported S3-compatible storage, checksum verification, inventory/reconciliation workers, one central replay bucket; validate the actual provider's conditional-write behavior | 2 |
| Durable workflow service and independently scaled stage workers | Use Temporal Cloud with native Rust workers; keep compute in our infrastructure and avoid operating an additional orchestration cluster | 5 |
| Reliable PostgreSQL-to-Kafka delivery | Transactional outbox and durable dispatcher with at-least-once delivery and idempotent consumers. Use this one permanent delivery mechanism for authoritative PostgreSQL changes | 5, reused by 6–8 |
| Replay facts/state/deletion topics, quarantine, schema compatibility checks | Versioned topic contracts in source plus CI compatibility validation; explicit partitions/retention/ACLs and DLQ tooling. Enforce contract compatibility in CI using checked-in versioned schemas | 7–8 |
| Replay ClickHouse sink and duplicate-safe aggregates | Extend the existing Kafka-engine/MV pattern with replay-specific migrations and read models; prove offset/retry and aggregate correctness under redelivery | 8 |
| Production HA, backup/restore and disaster recovery evidence | Audit actual Kafka replication/ISR, PostgreSQL failover/PITR, ClickHouse replication/Keeper where self-hosted, object durability and Temporal recovery; provision missing HA before the relevant release receives production traffic | Each owning step; full recovery exercise in 14 |

The local single-broker Kafka configuration (replication factor 1), single ClickHouse/PostgreSQL instances, and alpha-tagged local object-store image are development fixtures, not proof of production durability. Size Kafka retention for the maximum supported outage plus catch-up time; object manifests and authoritative state must support rebuilding beyond that horizon. Temporal owns durable analysis execution; pgvector owns similarity retrieval.

## Ordered releases

## 1. Give rendering a stronger isolation boundary

**Prerequisites:** None.

**Shippable outcome:** Deploy isolated rendering for the current pipeline, with all credentials, network and process-tree tests passing.

**Deployment and recovery:** Canary in the real runtime; on failure stop admission and roll forward the isolation configuration, never fall back to an insecure renderer.

**Priority: high; can proceed independently.**

### Original problem (implementation below removes this path)

Chromium runs with `--no-sandbox`. The renderer child inherits the worker environment, and Chromium inherits that environment again, exposing credentials to more processes than necessary.

Fresh browser contexts and network URL blocking are useful existing controls, but they do not substitute for an enforced isolation boundary around untrusted recorded content.

### Tasks

- [x] Define the renderer's required inputs and remove unrelated database/provider credentials from its environment.
- [x] Pass local staged inputs or narrowly scoped object access instead of broad persistent credentials where practical.
- [x] Keep model-provider credentials in the existing supervisor/inference code and out of the browser process; this isolation must work before the Temporal stage split.
- [x] Enable the Chromium sandbox where the execution environment supports it; otherwise use an explicitly designed stronger outer isolation boundary.
- [x] Enforce network restrictions outside browser-level request blocking.
- [x] Apply CPU, memory, process, temporary-disk, and execution-time limits to the renderer and descendants.
- [x] Retain fresh contexts, temporary-directory cleanup, and process-group termination on cancellation.
- [x] Implement private authenticated renderer service, protocol/input/output bounds, one-job admission and busy-job deferral without consuming failed-attempt retries; keep object downloads, inference and database access in the supervisor.
- [x] Move rendering into `apps/replay-renderer` and remove `crates/rrweb2video`; keep only wire types/limits in `crates/replay-render-protocol`. Move the CLI, player assets and tests with the renderer; remove browser/encoding dependencies from the summarizer and database/storage dependencies from the renderer.
- [x] Wire separate Docker images into one DigitalOcean app through `../infra`, with generated shared authentication, private DNS/port only, independent instance counts and CPU/memory sizes, and infrastructure regression tests.
- [x] Implement shutdown/disconnect/deadline cancellation, whole-process-group termination, adopted-descendant reaping and restart scratch cleanup; no local-render fallback remains.
- [ ] Complete the deployment acceptance checks below, including actual App Platform OOM/restart behavior.

### Acceptance criteria

- Chromium cannot inherit database or provider secrets from the supervisor.
- Renderer network access and resource limits are enforced independently of replay JavaScript/browser configuration.
- Failed or cancelled jobs leave no running descendants or unbounded temporary artifacts.
- [ ] Verify isolation in the actual App Platform deployment: startup kernel preflight, private-only routing, a representative replay, replica routing, enforced CPU/memory/disk/process limits, timeout/disconnect/shutdown and OOM restart cleanup. These require a deployment; local Docker and mocked Terraform checks alone do not satisfy this gate.

### Implementation and local verification

The boundary is permanent: the current PostgreSQL worker calls the private render API;
Temporal Activities will call the same API in step 5. Rendering releases its slot
before inference. There is no Temporal dependency in this release. The renderer
uses inherited Linux seccomp to deny network sockets outside `AF_UNIX`, pipe-based
CDP, clean environments and fixed resource/deadline limits. The API service has
no storage/database/provider credentials. See `apps/replay-summarizer/README.md`
for the supervisor and `apps/replay-renderer/README.md` for rendering behavior and isolation. Deployment and scaling details live in `../infra/README.md`.

- [x] Rust unit/integration tests and strict Clippy for the affected packages pass; optional paid-provider and additional standalone browser fixtures remain explicitly ignored.
- [x] Disposable PostgreSQL/pgvector integration verifies busy-renderer deferral preserves retry budget and execution fencing, alongside existing publication tests.
- [x] Both separate production Docker images build successfully.
- [x] Infrastructure formatting, validation and all 12 mocked OpenTofu tests pass.
- [x] Linux container tests verify a real Chromium/FFmpeg render, authentication, invalid input, admission, child credential isolation, disconnect cancellation and shutdown.
- [x] Stalled-job timeout and abrupt-child-death container regressions pass, including orphan-process reaping.
- [x] Run renderer and disposable PostgreSQL checks through Rust tests under `cargo test`; remove the Python test wrappers.

### References

- `apps/replay-renderer/Dockerfile`
- `apps/replay-renderer/chromium.sh`
- `apps/replay-summarizer/Dockerfile`
- `apps/replay-summarizer/src/renderer.rs`: private API client
- `apps/replay-summarizer/src/replay_loader.rs`: credentialed downloads and evidence preparation
- `crates/replay-render-protocol/src/lib.rs`: shared wire types and limits
- `apps/replay-renderer/src/main.rs`: admission and job supervision
- `apps/replay-renderer/src/sandbox.rs`
- `apps/replay-renderer/tests/container.rs`
- `../infra/modules/workload/app.tf` and `../infra/environments/production/apps/services.tf`
- `apps/replay-renderer/src/browser.rs`

## 2. Strengthen object identity, reconciliation, and storage layout

**Prerequisites:** Step 1.

**Shippable outcome:** All ingest, playback, summarizer, expiry and project-delete paths use verified immutable object references in the single central replay bucket.

**Deployment and recovery:** Deploy readers for explicit storage locations before writers; migrate retained objects with checksum audits and retire bucket provisioner/lifecycle assumptions in this release. Roll back routing only to verified retained objects; never resume mutable overwrites.

**Priority: high correctness and operational requirement.**

### Current problem

Object keys derive from batch identity and timestamp rather than payload content. Uploads are unconditional and precede the transactional duplicate check. Conflicting concurrent submissions with the same object identity can overwrite bytes before database metadata determines the winner.

Database failures intentionally leave uploaded objects behind. Those objects require reconciliation. One object per client chunk can also create high request and metadata overhead. Bucket-per-project makes provisioning and policy management grow with tenant count.

### Tasks

- [ ] Define a canonical payload checksum and immutable object identity.
- [ ] Treat an existing batch identity with different content as a conflict, not an ordinary retry.
- [ ] Use content-addressed keys or verified conditional creation supported by the selected object store.
- [ ] Record checksums in authoritative chunk metadata and verify loaded objects against those checksums.
- [ ] Reconcile object inventories against authoritative chunk/artifact references with a safe grace period and live upload claims for in-flight writes.
- [ ] Cover upload-success/database-failure, uncertain commit outcomes, duplicate uploads, generation reset, and deletion races.
- [ ] Replace bucket-per-project provisioning with exactly one central replay bucket and project/generation/type prefixes; preserve authorization and project-specific retention through explicit metadata and deletion workers. Bucket prefixes alone are not an authorization boundary.
- [ ] Specify retention for raw data, manifests, derived artifacts, and orphaned uploads together.

### Acceptance criteria

- Conflicting retries cannot mutate bytes referenced by an accepted chunk.
- Identical retries remain idempotent.
- Abandoned objects are eventually reclaimed without deleting in-flight or referenced data.
- All replay services use the same central bucket; project-scoped reads, expiry, reset and deletion cannot affect another project.

### References

- `apps/replay-consumer/src/storage.rs`: upload path, `replay_object_key`, persistence outcomes
- `apps/replay-consumer/src/object_store.rs`
- `apps/replay-summarizer/src/object_store.rs`

### Cross-repository storage and deletion cutover

Use current authoritative chunk metadata plus the checksums/location references added here; this release must not wait for step 4 manifests. Later manifest creation consumes these same verified references.

- [ ] Update `../monorepo/apps/backend/src/services/session-replays/object-store.ts`, `bucket-provisioner.ts`, `replay-storage.ts`, `replay-expiry-worker.ts`, playback downloads, collector and summarizer together. Persist an explicit bucket/key/layout reference instead of reconstructing locations from a project ID.
- [ ] Keep project-specific expiry in authoritative metadata and scheduled deletion; a shared-bucket lifecycle rule must never erase another project's data or fail to honor a shorter retention period.
- [ ] Fence new reads/writes immediately on deletion or generation reset, revoke outstanding access within a defined bound, and reconcile raw objects, manifests and artifacts. Inventory cleanup must respect live upload claims and references, not just object age.
- [ ] Provision the central bucket once through checked-in infrastructure configuration. Define `REPLAY_S3_BUCKET` as its exact name in collector, consumer, summarizer, backend and deployment examples. Remove `REPLAY_S3_BUCKET_PREFIX`, project-derived bucket names, alias/fallback interpretation and unused prefix normalization.
- [ ] Migrate all retained objects and references, verify checksums and playback, then remove old project buckets after the explicit rollback window. Remove `ReplayBucketProvisionerLive`, its startup wiring, `REPLAY_RUN_MIGRATION_CLEANUP`, `deleteLegacyBucket`, `deleteBucketsExcept`, project lifecycle synchronization, and obsolete provisioning-only states/callers. No startup migration may delete the central bucket.
- [ ] Replace project reset/delete operations with generation-fenced deletion of owned prefixes/references. Runtime credentials must not permit bucket creation/deletion or account-wide bucket enumeration; reserve bucket administration for infrastructure deployment.
- [ ] Define one key layout, for example `projects/{project_id}/generations/{generation}/{raw|manifests|artifacts}/...`; forbid cross-project deduplication. Restrict authorized object access by project and generation, including signed URL creation.
- [ ] Implement bounded, checkpointed expiry/orphan deletion with durable retry records now, using existing PostgreSQL maintenance facilities; do not depend on step 5. Apply project retention to referenced objects and artifacts, and reclaim multipart uploads/noncurrent versions if enabled. Bucket-wide lifecycle rules must be safe for every project.
- [ ] Verify central-bucket durability/recovery and documented throughput limits against the target workload. An object-store outage may delay processing; it must not trigger a second-bucket fallback or acknowledgment of unpersisted data.

## 3. Separate recording completeness from inactivity

**Prerequisites:** Steps 1–2.

**Shippable outcome:** Finalization and UI distinguish complete from timed-out-incomplete recordings using real sequence coverage.

**Deployment and recovery:** Migrate range state and update readers before enabling new finalization; retain accepted chunk facts so state can be recomputed without claiming gaps are complete.

**Priority: high correctness requirement.**

### Current problem

Controls retain the maximum received sequence and terminal sequence, but do not prove that intervening sequences arrived. Chunks 1 and 3 plus terminal 3 can satisfy the terminal condition despite missing chunk 2.

An inactivity deadline is useful for progress, but it is not evidence of completeness. Late data reopens recordings and invalidates analysis.

### Tasks

- [ ] Define sequence semantics, including initial sequence, empty terminal markers, retries, and any coalesced ranges.
- [ ] Track received ranges or a contiguous watermark against an explicit terminal boundary.
- [ ] Represent open, complete, and timed-out-incomplete states separately.
- [ ] Persist completeness and missing-range information in recording metadata and analysis provenance; step 4 includes this existing state in immutable manifests.
- [ ] Define whether and when incomplete recordings may be summarized, and expose the evidence limitation.
- [ ] Define late-arrival behavior using the existing accepted-chunk revision/execution fences: invalidate stale results, cancel stale work and reschedule. Step 4 strengthens content identity; gap detection and stale-work rejection must already work in this release.
- [ ] Prevent repeated equivalent terminal markers from extending deadlines.
- [ ] Test out-of-order delivery, missing middle chunks, duplicate ranges, and a terminal marker arriving before data.

### Acceptance criteria

- A terminal maximum alone cannot mark a recording gap-free.
- Timeouts allow progress without falsely asserting completeness.
- Late arrivals produce explicit revision transitions and cannot overwrite newer analysis.

### References

- `apps/replay-consumer/src/controls.rs`: `record_chunk`
- `apps/replay-consumer/src/finalizer.rs`

## 4. Use immutable recording revisions and analysis specifications

**Prerequisites:** Steps 1–3.

**Shippable outcome:** Existing execution and publication use immutable manifest/spec identities, including manual reprocessing.

**Deployment and recovery:** Populate identities from verified chunks; update all uniqueness constraints and API lookups in this release. Stop affected jobs during cutover if needed; retain old references for the rollback window.

**Priority: high; foundation for caching and retries.**

### Current problem

`chunk_count` doubles as a recording revision. That relies on accepted chunks remaining append-only and unchanged. It does not identify content independently of representation changes, corrections, or compaction.

Job and summary uniqueness identify a recording revision without a complete analysis specification. Recording model/prompt/render metadata after execution is not the same as versioning the requested analysis identity.

### Tasks

- [ ] Introduce an immutable manifest revision or content hash covering ordered chunk identities, hashes, sequence ranges, and completeness state.
- [ ] Separate logical recording content identity from physical object layout. Benchmark chunk sizes and GET/PUT volume in this release; if compaction is needed to meet the agreed capacity budget, implement it here with atomic reference replacement and safe garbage collection. Otherwise close the decision with evidence that the uncompacted layout meets the target; do not leave an unevaluated compaction task.
- [ ] Define an analysis specification containing model selection, prompt hash/version, output schema, renderer version, preprocessing version, and render/evidence settings.
- [ ] Use recording revision plus analysis-spec hash as the execution and cache identity.
- [ ] Preserve distinct analysis runs for different specifications and define which result is currently selected for display.
- [ ] Fence publication against deletion, storage generation, and the intended revision.
- [ ] Make reprocessing and model/prompt comparisons explicit operations rather than mutation of an old job's meaning.

### Acceptance criteria

- Two different payload sets cannot share an analysis identity merely because they have equal chunk counts.
- A prompt/model/render change creates a distinct analysis identity.
- Cached outputs cannot be reused across incompatible analysis specifications.
- Physical compaction has documented effects on logical identity and existing summaries.

### References

- `apps/replay-summarizer/src/jobs.rs`: `Claim`, `prepare`, `finish`
- `apps/replay-summarizer/src/summarize.rs`: version constants and metadata
- `../monorepo/packages/database/schemas/replays.ts`: revision uniqueness constraints

## 5. Adopt Temporal Cloud and decouple summary publication from grouping

**Prerequisites:** Steps 1–4.

**Shippable outcome:** Deploy a complete durable analysis workflow with checkpointed outputs, a proven workflow-start outbox, short summary publication transactions and independently retryable grouping.

**Deployment and recovery:** Deploy pending grouping states in backend/UI and the workflow workers together. Exercise the Temporal implementation in production canaries, drain old jobs, transfer each execution once, then remove the old scheduler. Roll back only after fencing/draining the new owner while preserving pending findings and manual assignments; never run two schedulers for one execution.

**Priority: highest.**

### Current problem

The retry unit spans download, rendering, video analysis, embedding, and publication. Successful model output is not durably checkpointed before embedding/publication. A downstream failure can repeat rendering and a paid model request.

Step 1 releases the private renderer slot before remote inference. The PostgreSQL supervisor still carries each job through download, rendering, inference and publication in one execution attempt; these stages still need independent durable retries and admission.

### Tasks

- [ ] Define explicit stages: prepare manifest, render or extract evidence, analyze, validate, publish summary, embed, and group.
- [ ] Store durable outputs and provenance for successful stages before advancing.
- [ ] Key outputs by immutable recording revision and versioned analysis specification; coordinate with step 4.
- [ ] Use Temporal Activity timeouts/heartbeats, retry budgets, error classifications, and attempt records; retain PostgreSQL publication fences rather than a competing stage scheduler.
- [ ] Reuse successful render and inference outputs after embedding or publication failures.
- [ ] Separate renderer concurrency from inference, embedding, and grouping concurrency. Use separately deployable worker pools with independent scheduling and bounded resources.
- [ ] Define artifact retention and cleanup for successful, failed, abandoned, and superseded attempts.
- [ ] Define crash recovery at every boundary, including a provider response received just before persistence fails.
- [ ] Use provider idempotency or request lookup where available; otherwise document the remaining ambiguous-response duplicate-cost window.

### Acceptance criteria

- Injecting an embedding or publication failure does not cause a successfully checkpointed summary call to run again.
- Worker restarts resume from the latest valid durable stage output.
- Stale recordings cannot publish cached outputs belonging to an older revision.
- Rendering can continue while unrelated jobs wait on model APIs.

### References

- `apps/replay-summarizer/src/main.rs`: `run_job`, `prepare_insights_with_lease`
- `apps/replay-summarizer/src/renderer.rs`: private render client
- `apps/replay-renderer/src/main.rs`: isolated render service
- `apps/replay-summarizer/src/jobs.rs`

### Durable execution decision — use Temporal Cloud with Rust

**Decision made on 2026-09-23:** adopt Temporal Cloud and native Rust workflows/Activities. Keep our Rust processing code and isolated renderer. Do not build a general durable execution engine or introduce a TypeScript/Node adapter. This is a completed architecture decision; the tasks below implement and validate it.

The current `jobs.rs`/`main.rs` already implement claiming, execution tokens, lease renewal, timeouts and retry recovery, but the retry boundary is still the whole analysis job. Extending this to durable stage outputs, independently scheduled pools, cancellation, parallel segments, workflow upgrades and recovery would create a substantial orchestration subsystem. Those requirements are central to this backlog, so a maintained workflow engine is justified. This conclusion comes from code and documentation review; it does not claim an unperformed performance benchmark.

Temporal Cloud is the sole selected execution service. We deploy native Rust workers and retain domain fencing, publication transactions and delivery outboxes in PostgreSQL. Sources: [Temporal Rust SDK 1.0.0 GA release](https://github.com/temporalio/sdk-rust/releases/tag/v1.0.0) and [Rust SDK guide](https://docs.temporal.io/develop/rust).

**Integration boundary:** Kafka ingestion and ClickHouse streaming remain independent of Temporal. Start one analysis workflow per selected immutable recording revision and analysis specification, with separate durable grouping work after summary publication. Use Rust Activity workers for preparation, rendering, inference, embedding and publication, with separate task queues and resource limits. Rendering retains the private service boundary from step 1; its service supervises the isolated child process tree. Activity cancellation/heartbeats must cancel the render request, and the service must terminate its process tree on disconnect. Keep replay bodies, videos and model output in object storage and pass small verified references through workflow history. PostgreSQL retains product state, authorization, deletion/generation fences and publication authority.

**What we still implement:** idempotent object output keys, provider idempotency/request lookup where available, transactional outbox delivery, generation fencing, retention/deletion, and resource admission. Temporal does not make arbitrary external calls exactly-once or eliminate the crash window between a provider response and durable persistence. These are application correctness boundaries, not reasons to build another scheduler. See [Activity idempotency](https://docs.temporal.io/activity-definition).

**Cost and operational tradeoff:** Cloud introduces a paid external orchestration dependency; we continue paying for our workers and storage. Published action pricing starts at $50 per million actions, with history storage and support charged separately. Budget from analyzed recordings × actual actions per workflow, including retries and grouping, rather than raw rrweb events. Keep histories small and retention explicit. We do not have workload volumes sufficient for a credible monthly total, and there is no measured claim that Temporal lowers total cost. [Temporal pricing](https://temporal.io/pricing).

### Temporal implementation

- [ ] Provision Temporal Cloud namespaces/access controls, history retention and capacity for the replay workers; use a local Temporal development server in the integration environment.
- [ ] Add the stable native Rust SDK, starting from the GA 1.0 release line, pin compatible SDK crates and the supported Rust toolchain, and add Rust workflow/Activity worker entrypoints. Reuse processing libraries; keep blocking/render work outside deterministic workflow code.
- [ ] Replace database job claiming, lease-based workflow scheduling and retry ownership with Temporal. Preserve PostgreSQL execution/publication fences as domain guards, and persist product-facing status without making it a second scheduler.
- [ ] Migrate every job producer and status reader in this release: consumer finalization, backend manual requests/reprocessing, cancellation/deletion, settings changes and UI progress. Replace direct `replaySummaryJobs` queue writes and `leaseUntil`-based liveness checks with the execution-request outbox and an authoritative product-facing execution projection.
- [ ] Drain/fence the old queue, then remove obsolete claim/renew/retry loops, queue indexes/columns, environment settings and deployment entrypoints. Keep only fields required for run identity, publication fencing and product history. Retire old worker builds after their workflows finish or migrate safely; no permanently running compatibility workers.
- [ ] Provision the workflow-request Kafka topic, ACLs, schema, consumer group and dispatcher in this release. Define bounded dispatch retries, durable failed-delivery handling and operator replay here; do not depend on the later ingestion-consumer overhaul.
- [ ] Set retention and cleanup for delivered outbox rows, execution projections, expired artifacts and completed workflow histories. Preserve request identity/generation fences long enough to reject stale Kafka replay even after Temporal history expires; deduplication cannot rely only on retained workflow history.
- [ ] Validate the concrete implementation with worker-kill recovery, cancellation, history replay, deployment compatibility and representative throughput/cost tests before cutover. These are release acceptance checks for the selected architecture, not an open-ended tool evaluation.
- [ ] Define deterministic workflow IDs from project, generation, manifest revision and analysis-spec hash. PostgreSQL transactionally records the request/outbox; an outbox dispatcher publishes a versioned workflow-request event to Kafka, and a consumer starts the workflow idempotently before acknowledging it. The dispatcher marks delivery only after Kafka acknowledgment. A crash between start and acknowledgment must not create a second execution. Define closed-workflow ID reuse and explicit rerun identities.
- [ ] Keep payloads/videos/model responses in object storage; workflow history contains bounded identifiers, checksums and references. Do not create one workflow per rrweb event or chunk. Bound fan-out, history, retry time and signal volume; use child workflows or Continue-As-New where appropriate.
- [ ] Use independent task queues/resource pools for rendering, inference and grouping. Activities perform I/O with deadlines, heartbeats and cancellation; workflow code stays deterministic. Avoid stacking unconstrained SDK/provider/application retries.
- [ ] Activities may execute again. Use durable output keys and fenced idempotent publication for side effects; Temporal does not make provider calls or cross-store writes exactly-once. Preserve the ambiguous provider-response accounting window.
- [ ] Ship workflow-history replay tests, safe worker/version upgrades, timeout/crash recovery, project deletion and stale-generation cancellation. No activity may resurrect deleted data; database fences are required even after cancellation.
- [ ] Ship the publication/grouping separation below in this same release. Retain the current candidate-retrieval algorithm outside transactions; representative indexing is a later optimization, not a prerequisite for correct grouping.

References: [Rust SDK guide](https://docs.temporal.io/develop/rust), [Activity idempotency](https://docs.temporal.io/activity-definition), [Continue-As-New](https://docs.temporal.io/workflow-execution/continue-as-new). These explain the primitives used by the selected architecture.

### Remove external model calls from database transactions

### Current problem

Summary publication takes a project-wide advisory lock, locks recording state, and calls the Jev decision API before committing. Publications for the same project serialize behind the provider call. Manual operations using the same lock also wait. More summarizer replicas cannot remove this per-project bottleneck.

The five-second decision budget bounds the provider portion, not the entire publication transaction. Queries, lock acquisition, and writes add time.

### Tasks

- [ ] Publish validated summary text and pain points in a short, fenced transaction without embedding or grouping network calls.
- [ ] Enqueue grouping work atomically with finding publication, using a transactional outbox dispatched to the durable workflow owner.
- [ ] Retrieve candidate representatives and their versions, release database transactions, then call the decision provider.
- [ ] Apply decisions in a short transaction that revalidates recording revision, storage generation, finding validity, and relevant group/membership versions.
- [ ] Retry decisions whose candidate state changed instead of holding locks across inference.
- [ ] Preserve manual merge, split, and assignment decisions when applying automated results.
- [ ] Document one lock acquisition order shared by automated publication and manual edits.
- [ ] Set explicit lock and transaction deadlines; distinguish these from provider timeouts.

### Acceptance criteria

- No external HTTP call executes inside a publication/grouping database transaction.
- A slow or unavailable decision provider does not delay summary publication or hold project locks.
- Concurrent grouping and manual edits cannot silently overwrite one another.
- Measure same-project publication throughput and lock-wait latency before and after the change.

### References

- `apps/replay-summarizer/src/jobs.rs`: `finish`
- `apps/replay-summarizer/src/insights.rs`: `save`, `decide`

### Grouping outage semantics (included in this release)

**Priority: high.**

### Current problem

When the decision provider fails or the shared five-second budget expires, remaining findings become separate groups. No automatic reconciliation pass subsequently revisits them. Provider latency and finding order can therefore shape the product's taxonomy.

Fragmentation increases the search population. Using the oldest single member as the sole representative can also make matching quality depend heavily on historical arrival order.

### Tasks

- [ ] Distinguish `pending`, `decision_unavailable`, `matched`, and `confirmed_new_group` outcomes.
- [ ] Give unavailable decisions independent retry scheduling and backoff.
- [ ] Add reconciliation for unresolved or provisionally separated findings.
- [ ] Preserve manual decisions during automated reconciliation.
- [ ] Store decision-model version, candidate representative versions, scores, thresholds, and decision reason.
- [ ] Evaluate representative selection using labeled examples; consider multiple representative observations when a single example is inadequate.
- [ ] Build grouping-quality evaluations for false merges, false splits, order sensitivity, and provider outages.
- [ ] Reevaluate thresholds when changing embedding or decision models; do not interpret scores as calibrated probabilities.

### Acceptance criteria

- A provider outage leaves recoverable pending work rather than permanently asserting that observations are unrelated.
- Reprocessing pending work does not duplicate memberships or undo manual assignments.
- Grouping quality is evaluated separately from successful API execution.

### References

- `apps/replay-summarizer/src/insights.rs`: deadline handling, fallback group creation, `best_match`
- `apps/replay-summarizer/README.md`

## 6. Reduce PostgreSQL work on every ingested chunk

**Prerequisites:** Steps 1–5.

**Shippable outcome:** Ingestion commits minimal durable facts; a complete asynchronous detector/rebuild worker maintains current product counts and routes.

**Deployment and recovery:** Deploy the outbox consumer and revision fences before switching ingestion; validate parity then remove inline rebuilds. Pause analytics on rollback while retaining work, never discard notifications.

**Priority: highest.**

### Current problem

Each chunk triggers generation and duplicate checks, object upload, transactional rechecks, snapshot insertion, session aggregation, control updates, click analysis, and a billing uniqueness insert. The session row is updated multiple times, and mutable counters share a row with route arrays and a JSON checkpoint.

This creates round trips, row churn, index maintenance, and dependencies between durable ingestion and derived analytics. Kafka acknowledgment currently waits for click analysis to finish.

### Tasks

- [ ] Measure SQL statements, transaction duration, WAL volume, row/index growth, and lock waits per chunk.
- [ ] Define the minimum durable state required before acknowledging a chunk: verified payload reference, accepted identity, generation, and required lifecycle bookkeeping.
- [ ] Move derived analytics outside the ingestion transaction; emit work atomically using a transactional outbox published to Kafka.
- [ ] Provision this release's accepted-chunk topic, schema and consumers before switching ingestion. Keep one versioned detector/extraction implementation: it maintains the existing product projection now and emits the step 8 analytical facts later. The ClickHouse migration must remove superseded PostgreSQL analytical projections without leaving a second detector or rebuild scheduler.
- [ ] Correct the existing `uncompressed_bytes` assignment using the actual decoded size and retain exact usage semantics; include a compressed-versus-decoded fixture. This is data correctness, not instrumentation.

- [ ] Batch metadata operations where ordering and idempotency permit.
- [ ] Consolidate repeated session updates and avoid rewriting large derived fields on every chunk.
- [ ] Separate hot lifecycle state from larger analytical fields when measurements justify it.
- [ ] Preserve exact deduplication and billing semantics while reducing repeated uniqueness work.
- [ ] Keep generation/deletion fencing correct across the object upload and database commit boundary.

### Acceptance criteria

- Ingestion acknowledgment does not depend on historical click analysis or insight computation.
- Duplicate delivery does not double-count usage or chunk totals.
- Database work per accepted chunk has a documented, measured bound for the normal path.
- Delayed analytics does not prevent durable recording ingestion.

### References

- `apps/replay-consumer/src/storage.rs`: `store_replay_chunk`
- `apps/replay-consumer/src/controls.rs`
- `apps/replay-consumer/src/clicks.rs`: `refresh`

### Historical analytics rebuilds (included in this release)

**Priority: high for late-arrival workloads.**

### Current problem

Click analysis has a useful bounded incremental checkpoint. Overlapping timestamps, late chunks, or incompatible checkpoints trigger a read and sort of historical signals while holding the ingestion transaction and stream lock. Repeated late arrivals can produce near-quadratic cumulative rebuilding work.

Entry/exit route updates are arrival-based: a late older chunk can incorrectly become the exit route.

### Tasks

- [ ] Preserve the normal incremental click path where it remains cheap and correct.
- [ ] Mark analytics dirty when historical recomputation is required and enqueue a revision-scoped rebuild.
- [ ] Perform rebuilds outside ingestion transactions and publish only if their source revision is still valid.
- [ ] Coalesce repeated dirty notifications so one recording does not schedule redundant rebuilds.
- [ ] Define whether stale counts remain visible with a status or are temporarily unavailable.
- [ ] Derive entry and exit routes using event-time bounds with deterministic tie handling.
- [ ] Version detector behavior and support intentional recomputation after detector changes.
- [ ] Keep player and backend detector semantics aligned through shared fixtures or an explicit common specification.

### Acceptance criteria

- Late chunks do not trigger historical scans inside the ingestion transaction.
- Rebuild output cannot overwrite newer analytical state.
- Route endpoints remain correct under reordered chunk arrival.
- Benchmarks include sustained late/overlapping arrivals, not just append-only recordings.

### References

- `apps/replay-consumer/src/clicks.rs`: `Checkpoint`, `refresh`
- `apps/replay-consumer/src/storage.rs`: session route updates

## 7. Isolate consumer failures and remove the whole-batch barrier

**Prerequisites:** Steps 1–6.

**Shippable outcome:** Consumers isolate partition failures with durable quarantine and repair tooling.

**Deployment and recovery:** Canary a consumer group through crashes/rebalances; retain committed offsets and quarantine records across rollback. Do not reset offsets to latest.

**Priority: high reliability requirement.**

### Current problem

Four recording keys execute concurrently, but offset storage waits for the whole batch, which may include unrelated partitions. A slow upload delays all of them. Persistence errors exit the process, allowing permanent failures to create restart loops.

Malformed records are instead dropped and acknowledged without durable quarantine. `auto.offset.reset=latest` may skip retained history when a valid committed offset is unavailable.

### Tasks

- [ ] Introduce bounded per-partition execution with ordering preserved for each recording.
- [ ] Track contiguous completed offsets independently for each partition; never acknowledge across an unfinished gap.
- [ ] Pause and resume partitions under backpressure while continuing to service Kafka ownership and polling requirements.
- [ ] Classify transient infrastructure errors, permanent malformed data, invalid generations, and conflicting identities.
- [ ] Add bounded retry/backoff and durable quarantine with topic, partition, offset, schema/version context, and a retained payload or repairable reference.
- [ ] Advance past permanent failures only after the quarantine policy has durably recorded the failure.
- [ ] Provide controlled replay from quarantine.
- [ ] Choose explicit missing-offset behavior, such as failing loudly or replaying from earliest, rather than silently assuming latest is acceptable.
- [ ] Preserve rebalance fencing and graceful shutdown behavior under in-flight work.

### Acceptance criteria

- A failing partition does not stall persistence and acknowledgment on unrelated healthy partitions.
- A poison record does not cause an unbounded process restart loop.
- Malformed records remain diagnosable and recoverable.
- Crash/rebalance tests prove that offsets never advance past unpersisted records.

### References

- `apps/replay-consumer/src/consumer.rs`: `run`, `store_processed`, `create_consumer`, `handle_message`
- `crates/replay-message/src/lib.rs`

## 8. Move broad replay analytics off the transactional database

**Prerequisites:** Steps 1–7.

**Shippable outcome:** Kafka-fed event/state projections power production replay filters and a usable heatmap slice, with deletion and backfill working.

**Deployment and recovery:** Provision topics, sink and readers before producers; backfill through Kafka, compare results and cut over API/UI together. Retain the old read route only for a measured rollback window, then remove broad PostgreSQL analytics; no direct-insert fallback.

**Priority: high for retained-data growth.**

### Current problem

Replay queries combine route-array searches, custom JSON predicates, summary existence, collection membership, viewed state, and different sort orders in PostgreSQL. Broad historical exploration competes with ingestion and execution state.

A small indexed operational listing can remain in PostgreSQL. The concern is arbitrary multidimensional filtering over large retained histories.

### Tasks

- [ ] Inventory product query shapes, latency requirements, tenant sizes, and retention windows.
- [ ] Design ClickHouse projections for recording facts, route visits, interactions, summary availability, and analytical aggregates.
- [ ] Specify stable event identities, version ordering, duplicate handling, and late-arrival semantics.
- [ ] Deliver authoritative PostgreSQL changes through a transactional outbox and Kafka; no direct ClickHouse inserts.
- [ ] Define how mutable PostgreSQL state such as manual collections and viewed flags participates in filtering and pagination.
- [ ] Avoid fetching huge candidate ID lists into application memory to join the two databases.
- [ ] Propagate deletion and storage-generation changes, with explicit visibility guarantees while projections catch up.
- [ ] Provide replay/backfill and reconciliation procedures for rebuilding projections.
- [ ] Choose partitioning, ordering keys, batching, and retention policies from measured query and ingestion patterns.

### Acceptance criteria

- High-volume analytics does not execute broad scans against the job/ingestion database.
- Duplicate or reordered projection events produce correct query results under the documented consistency model.
- Cross-store filtering has defined pagination and deletion behavior.
- Benchmarks include large tenants, broad filters, concurrent ingestion, and retained history.

### References

- `../monorepo/apps/backend/src/services/session-replays/query.ts`
- `../monorepo/packages/database/schemas/replays.ts`

### Detailed queryable event contract

The existing `ReplayChunk` envelope has project/session/window/view IDs, generation, sequence ranges, coarse browser/OS/country, URL and raw rrweb events. `clicks.rs` extracts pointer coordinates and boundaries but primarily persists counts/checkpoints. Preserve useful structured facts rather than only a session summary or an opaque JSON blob.

| Fact family | Typed fields to capture/project when available | Queries enabled |
| --- | --- | --- |
| Identity and provenance | schema version, stable event ID, project/session/window/view, generation, source chunk/sequence/event index, event time, receive time, manifest revision where relevant, detector version, SDK version | Dedupe, late arrival analysis, exact replay jumps, recomputation |
| Session/device context | pseudonymous visitor ID under capture policy, browser/OS versions, device class, viewport dimensions, pixel ratio, country, environment, release, duration, active duration, completeness | Device/release cohorts, incomplete sessions, duration and activity filters |
| Navigation | sanitized origin/path, normalized route, navigation/view ID, referrer class, source-time start/end, event-time entry/exit | Page sequences, landing/exit routes, funnels, time on route |
| Interaction | click/tap/scroll type, pointer type, source timestamp, rrweb node ID scoped to document/snapshot, approved stable element key, viewport coordinates, scroll offsets, document dimensions, element bounds where available | Element filters, rage/dead-click signals, click/tap/scroll heatmaps |
| Correlated signals | error occurrence/issue ID, handled flag, vital name/value/rating, sanitized request route/method/status/duration, correlation ID | Replays with slow requests, a specific error, poor vitals or release regressions |
| Product state | summary status/spec/version, finding category/confidence, collection membership version, viewer-scoped viewed state, deletion/generation tombstones | Combined analytical/product filters with documented freshness |
| Approved custom context | typed allowlisted event names/properties, bounded key/value count and size, capture/sampling version | Customer-defined segments without uncontrolled schema/cardinality growth |

- [ ] Inventory which fields are actually recorded versus derivable versus missing. Ship necessary SDK/capture, collector-envelope, domain-schema and query/UI changes in this release. If the SDK lives outside these repos, identify its owner/release prerequisite before starting the step. Missing values remain null/unknown; do not fabricate historical geometry or detector results.
- [ ] Define units, nullable behavior, coordinate spaces, source clock handling and deterministic event ordering. Base stable raw event IDs on recording identity plus accepted chunk/event identity, not delivery time or a new UUID per retry. Revision-scoped derived facts use a separate identity so rebuilding a recording cannot duplicate unchanged raw events.
- [ ] Keep raw DOM snapshots, text/input values, bodies, credentials and full URLs with sensitive parameters out of analytical facts. Apply capture masking, URL/property allowlists and access controls before Kafka publication. Preserve existing privacy semantics when enriching data.
- [ ] Separate event-grain facts, versioned recording state and route/interaction aggregates. Use typed columns for common filters and bounded custom properties. Specify event counts versus distinct sessions/visitors for every analytical result.
- [ ] Ship filter operators and combinations in `packages/domain`, backend query services and replay UI together, including indexed cursor pagination, unknown values, empty results and explicit projection freshness. Viewer flags must be scoped by viewer; do not multiply all events by every viewer.

### Heatmaps as a complete supported slice

- [ ] Deliver click/tap density and scroll reach queries plus a usable visualization for a selected project, normalized route, time range and device/viewport cohort. Publish coverage and sampled/eligible session counts alongside results.
- [ ] Preserve viewport and document coordinates separately: scroll offsets, responsive breakpoint, route/view, layout/release and document/snapshot context determine alignment. Scope rrweb node IDs to their recording context; they are not stable selectors across sessions.
- [ ] For element-based maps, require a sanitized stable element key and matching layout reference. For coordinate maps, use compatible layout/viewport cohorts and explicit transform rules. Do not merge unrelated responsive layouts into a misleading overlay.
- [ ] Define scroll denominator from eligible page views and observed document height; record sampling policy/weights and missing geometry. Handle resize, dynamic page height, SPA navigation and nested scroll/iframe limitations explicitly.
- [ ] Bound pointer-move volume with a documented sampling policy if movement maps are added. Start with click/tap and scroll data; pointer paths must not turn Kafka/ClickHouse into a copy of every DOM mutation.
- [ ] Verify known-coordinate fixtures, resize/scroll transforms, duplicate delivery, late navigation and deletion; match aggregate totals to deduplicated source facts.

### Kafka sink, consistency and recovery

- [ ] Provision versioned facts, state and tombstone topics (for example `replay-events-v1`, `replay-state-v1`, `replay-deletions-v1`), keyed by project/generation/recording for per-recording order. Define source versions across topics; do not assume cross-topic arrival order.
- [ ] Use the step 5 outbox for transactional state and accepted-chunk notifications; extraction workers read verified objects and publish facts to Kafka before acknowledging their input. Kafka transport is at-least-once: deterministic identities and version handling must make reprocessing safe.
- [ ] Add Kafka-engine/MV migrations to `../monorepo/packages/clickhouse`, with explicit consumer groups, batch sizes, partition/consumer capacity, malformed-data quarantine and offset recovery. No `INSERT`, `INSERT SELECT`, HTTP insert, or direct repair path from application/backfill code.
- [ ] Prove duplicate-safe query and aggregate semantics before serving reads. ReplacingMergeTree background merging alone does not prevent an incremental sum MV from counting duplicates. Choose deduplicated query state or versioned replacement aggregate snapshots; test corrections/deletions and measure query cost before selecting engines.
- [ ] Publish versioned deletion/generation tombstones through Kafka, enforce authoritative read authorization immediately, and remove physical facts/aggregates within a documented deletion SLA. Keep deletion fences beyond the maximum replay horizon so old topics/backfills cannot resurrect data; include caches and heatmaps.
- [ ] Define state-projection lag, read-after-write behavior and stable pagination watermarks. Combine projected collection/summary state with analytics without large application-side ID joins; use bounded authoritative checks for returned records and fail closed on deleted generations.
- [ ] Backfill from verified retained manifests and authoritative state through the same topics/contracts; rate-limit it separately from live ingestion, checkpoint progress, preserve stable IDs and reconcile counts/checksums. Kafka retention alone is not the long-term rebuild archive.
- [ ] Benchmark sort keys/partitions against retained-data scans, large projects, high-cardinality routes and concurrent ingestion. Avoid a ClickHouse partition/table per project; start with time partitions and project-leading ordering based on measured queries.

### Remove superseded replay analytics in this release

- [ ] Replace `getSessionErrors` in `../monorepo/apps/backend/src/services/session-replays/service.ts`, which currently calls Tinybird's `get_errors_for_session_v3`, with the ClickHouse error-query path. Preserve project/session/window scoping, ordering and error details, with parity fixtures and historical coverage through Kafka backfill.
- [ ] Remove replay-specific Tinybird imports, adapters, endpoints/materializations, configuration and deployed resources after verifying callers and retained-data migration. Shared resources used by other products remain owned by those products; no replay route may rely on the retired path.
- [ ] Remove replaced PostgreSQL filter tables, broad-query helpers, analytical columns/indexes, duplicate projections and stale API/domain/UI branches after cutover. Keep operational metadata and product state only where their authoritative role is explicit.
- [ ] Retire migration/backfill jobs and temporary read switches after reconciliation; revoke their privileges and remove expired topic consumer groups and unused sink objects. Keep a tested rebuild command using the permanent Kafka contract.

Additional references: `crates/replay-message/src/lib.rs`, `apps/replay-consumer/src/clicks.rs`, `../monorepo/packages/domain/src/session-replays.ts`, `../monorepo/packages/domain/src/replay-events.ts`, `../monorepo/packages/clickhouse/migrations/initial_schema.sql`, and the [ClickHouse Kafka engine documentation](https://clickhouse.com/docs/integrations/connectors/data-ingestion/kafka/kafka-table-engine).

## 9. Replace per-group reconstruction in similarity retrieval

**Prerequisites:** Steps 1–8.

**Shippable outcome:** Grouping searches maintained representatives; invalidation and the selected exact/indexed retrieval behavior are tested.

**Deployment and recovery:** Build and validate representative records before query cutover; exact search over these same records is the correctness baseline and rollback route.

**Priority: high.**

### Current problem

For every finding, the candidate query discovers each group's oldest current representative through a lateral join across memberships, embeddings, summaries, projects, and sessions, then performs exact vector comparisons. Returning four candidates does not bound the search work to four groups.

This work currently happens under the project publication lock and grows with the project's insight population.

### Tasks

- [ ] Maintain explicit representative records containing group ID, project ID, embedding model/preparation version, representative evidence ID, vector, and validity/version metadata.
- [ ] Update representatives when evidence becomes stale, is deleted, or changes membership.
- [ ] Search representative vectors directly rather than reconstructing them through relational history on every request.
- [ ] Retain exact search for small candidate populations and establish a measured crossover for approximate search.
- [ ] Benchmark pgvector HNSW using representative tenant sizes, filtering selectivity, latency and recall against exact search; ship the exact/indexed routing needed to meet the accepted target in this release and close the index decision.
- [ ] Ensure comparisons never cross incompatible embedding/preparation versions.
- [ ] Measure query plans and candidate-retrieval cost independently of provider latency.

### Acceptance criteria

- Retrieval does not perform a per-group multi-table representative reconstruction.
- Representative invalidation is correct after deletion, revision changes, and manual moves.
- Approximate search, if adopted, meets an explicit recall target under tenant filtering.
- Query cost and latency are measured as group counts grow.

### References

- `apps/replay-summarizer/src/insights.rs`: candidate query in `save`
- `../monorepo/packages/database/schemas/replays.ts`: insight embeddings and memberships

## 10. Remove fixed background-work ceilings

**Prerequisites:** Steps 1–9.

**Shippable outcome:** Finalization and cleanup drain backlogs within explicit budgets independently of consumer replicas.

**Deployment and recovery:** Canary maintenance concurrency with ownership fences; reducing concurrency is a safe rollback without losing due work.

**Priority: high for throughput growth.**

### Current problem

The finalizer selects at most 100 recordings every five seconds, imposing an upper bound of 20 recordings per second per instance before query overhead. Recording-control cleanup selects at most 1,000 rows per hour per instance.

Finalization also creates automatic jobs for recordings whose settings may cause a worker to claim and skip them later.

### Tasks

- [ ] Drain finalization in bounded batches until caught up, with cooperative yielding and resource limits.
- [ ] Apply the same backlog-driven approach to recording-control cleanup.
- [ ] Run lifecycle maintenance independently of ingestion worker count and Kafka partition ownership.
- [ ] Avoid automatic job creation when summarization is ineligible or off, while retaining manual-request support.
- [ ] Define settings-change behavior explicitly: whether newly eligible historical recordings should be scheduled and how.
- [ ] Verify indexes and query behavior for long-lived incomplete recordings, including those without a full snapshot.

### Acceptance criteria

- A backlog is processed at an explicit resource budget rather than one fixed batch per timer tick.
- Finalizer capacity can scale independently of Kafka consumers.
- Disabled automatic summarization does not generate routine claim-and-skip traffic.
- Manual requests remain available for otherwise eligible recordings.

### References

- `apps/replay-consumer/src/finalizer.rs`
- `apps/replay-summarizer/src/jobs.rs`: `prepare`, `matches_settings`

## 11. Enforce tenant fairness and shared resource budgets

**Prerequisites:** Steps 1–10.

**Shippable outcome:** Tenant fairness and global provider budgets hold under overload and worker scaling.

**Deployment and recovery:** Load-test admission and quota enforcement before enabling; rollback may lower admission but may not remove hard provider/resource limits.

### Tasks

- [ ] Add tenant-aware scheduling with explicit fairness and starvation behavior for automatic and manual work.
- [ ] Enforce per-provider concurrency/rate limits and per-project resource or spending budgets.
- [ ] Enforce provider concurrency, request/token rates and spend budgets across replicas, not per process; define ownership, expiry and fail-closed behavior for admission reservations.
- [ ] Use weighted/fair admission with bounded per-project outstanding work, separate manual/automatic quotas, aging and cancellation. Partition Kafka work by recording rather than solely by project so a hot project can use multiple partitions.
- [ ] Bound CPU, memory, disk, object-store requests and database connections at admission; propagate backpressure without losing accepted work.
- [ ] Verify scaling up and down within shared limits through load tests, including a noisy large project, provider throttling and starvation cases.

### Acceptance criteria

- Agreed small-project latency and maximum starvation bounds hold under a saturated large-project workload.
- Adding replicas cannot exceed provider or project budgets, including retried attempts.
- Limits can be reduced safely while work drains, without process restart loops or an unbounded in-memory queue.

## 12. Stop requiring whole-session video as the primary analysis representation

**Prerequisites:** Steps 1–11.

**Shippable outcome:** Timestamped evidence and representative visual windows replace whole-session video defaults after quality evaluation.

**Deployment and recovery:** Version the analysis specification and compare labeled outcomes; a prior validated evidence policy remains selectable as a supported analysis specification.

**Priority: high for cost and analysis quality.**

### Current problem

Each selected replay is loaded, sorted, rendered, encoded, and sent as video with idle time retained. At 3 FPS and 8× speed, source-time frame spacing is about 2.67 seconds, so brief states may be absent. Provider processing may sample further.

Structured interaction evidence contains only the first 500 matching events, biasing long recordings toward early activity.

### Tasks

- [ ] Build a structured, timestamped timeline before selecting visual evidence.
- [ ] Include navigation, interactions, visible-state changes, and correlated error/vital signals when available and appropriately scoped.
- [ ] Select visual windows around meaningful transitions, repeated actions, and potential failures.
- [ ] Preserve broader coverage or sampling so event selection does not systematically hide unexpected problems.
- [ ] Replace first-500 truncation with a documented coverage strategy, counts, and explicit omitted intervals.
- [ ] Preserve source-time mappings through idle compression, clip selection, and playback acceleration.
- [ ] Evaluate against short-lived failures, long idle periods, touch interactions, missing assets, and replay artifacts.
- [ ] Measure quality and cost against the current whole-video baseline before changing defaults.

### Acceptance criteria

- Findings have traceable evidence and valid original recording timestamps.
- Long recordings receive deliberate coverage beyond the first 500 interactions.
- Visual sampling settings have documented effects on observable detail.
- Cost reductions do not come from silently losing known classes of important evidence.

### References

- `apps/replay-summarizer/src/config.rs`: `render_speed`
- `apps/replay-summarizer/src/replay_loader.rs`: `interaction_evidence`
- `apps/replay-summarizer/src/renderer.rs`
- `apps/replay-summarizer/src/summarize.rs`

## 13. Replace hard size failures with segmentation and resource-aware admission

**Prerequisites:** Steps 1–12.

**Shippable outcome:** Supported large recordings run through reconstructible segments and budgeted resource classes.

**Deployment and recovery:** Enable resource classes after checkpoint and memory tests; unsupported workloads remain explicit failures, with durable progress retained on rollback.

**Priority: high for large recordings and predictable capacity.**

### Current problem

The default decoded replay budget is 32 MiB. Model video input is limited to 64 MiB and encoded into a base64 JSON request, adding roughly one-third to its byte representation before other allocations.

Decoded bytes do not bound browser DOM memory, frame buffers, or render duration. Viewport validation allows dimensions up to 16,384 on each axis. A recording can be expensive despite a relatively small serialized payload.

### Tasks

- [ ] Establish independent limits for decoded bytes, event count, DOM complexity where measurable, output pixels, frame count, duration, memory, and temporary disk usage.
- [ ] Estimate render cost from manifest/event metadata before admitting a job.
- [ ] Cap or scale output resolution while preserving coordinate and timestamp interpretation.
- [ ] Segment recordings at full snapshots or other reconstructible checkpoints; do not split incremental DOM events arbitrarily.
- [ ] Store segment outputs durably and combine analysis hierarchically into a session-level summary using original-time references.
- [ ] Use uploaded artifacts or provider file references when supported instead of repeatedly embedding base64 video in JSON; retain a bounded supported transport when the provider requires inline data.
- [ ] Fail unsupported workloads early with a specific reason; route supported large recordings to an appropriate resource class.
- [ ] Measure peak memory across Rust payloads, request serialization, Chromium, and FFmpeg rather than treating decoded bytes as total memory.

### Acceptance criteria

- Oversized but supported recordings can be segmented instead of failing the entire analysis.
- Known-impossible budgets are rejected before a full render.
- Per-job resource limits contain pathological viewports and recordings.
- Peak-memory and render-cost measurements inform scheduling limits.

### References

- `apps/replay-summarizer/src/config.rs`
- `apps/replay-summarizer/src/renderer.rs`: `load`, render options
- `apps/replay-summarizer/src/summarize.rs`: `encode_video`
- `apps/replay-renderer/src/replay.rs`: viewport and frame-plan validation

## 14. Prove end-to-end scale, recovery and quality

**Prerequisites:** Steps 1–13.

**Shippable outcome:** Publish capacity, recovery and quality evidence for the complete target system and resolve any failed gates.

**Deployment and recovery:** Run isolated drills before controlled production exercises; preserve recovery points and halt expansion when acceptance thresholds fail.

This release is a final system-level capacity certification, not a deferred testing or cleanup phase. Every earlier release must already satisfy its own correctness, performance and operational gates.

### Tasks and acceptance gates

- [ ] Run a reproducible workload matrix covering retained history, many small projects, skewed large projects, duplicate/late/out-of-order chunks, incomplete recordings, deletion and long/pathological sessions. Record hardware, versions, data volumes and target load.
- [ ] Establish benchmark acceptance thresholds before implementation and verify p95/p99 ingestion acknowledgment, playback first-byte, filter/heatmap query, finalization and summary queue-age budgets at projected peak plus a documented headroom factor. Report saturation points and cost per accepted/processed replay.
- [ ] Demonstrate horizontal throughput gains while database lock waits, Kafka lag, object request rates, ClickHouse parts/merge backlog, memory and temporary disk remain bounded. Explain any serial bottleneck and remove it before claiming the scale target is met.
- [ ] Exercise failure after upload, transaction commit, outbox publish, workflow start, provider response, artifact persistence, ClickHouse consumption and offset commit. Validate no lost acknowledged work, no duplicate billing and no stale/deleted publication.
- [ ] Restore PostgreSQL, object references and workflow service state; rebuild ClickHouse exclusively through Kafka. Test outages longer than normal topic retention, document RPO/RTO and prove tombstones survive reconstruction.
- [ ] Verify renderer isolation, per-project authorization, workload isolation, cancellation cleanup, provider budgets and small-project fairness in the actual deployment environment.
- [ ] Evaluate summary/grouping quality against labeled evidence, including false merges/splits, late data, long sessions and short-lived visual failures. Successful JSON validation alone does not demonstrate quality.
- [ ] Audit remaining direct ClickHouse data writers, obsolete replay paths, duplicate schedulers, obsolete bucket provisioners, rollout flags and unused schemas. Any bypass or unfinished replacement fails completion.
- [ ] Reconcile the removal checklists against source searches, database catalogs, bucket inventory, Kafka topics/groups, Temporal worker deployments and the actual deployed infrastructure. Require zero unexplained leftovers; repository inspection alone cannot prove deployed cleanup.
- [ ] Verify fresh installation and upgrade from retained production-like data converge to the same supported architecture: one central replay bucket, Temporal Cloud/Rust execution, one outbox delivery mechanism, Kafka-fed ClickHouse and pgvector representatives. Verify no legacy service is required to start, read old recordings or recover work.
- [ ] Confirm every required infrastructure resource is reproducible from checked-in configuration, with pinned supported versions, scoped credentials, secret rotation instructions, bounded retention and a tested recovery procedure. Remove unused resources and credentials instead of retaining dormant deployments.
- [ ] Close every in-scope defect/debt item discovered during implementation and audit before marking the plan complete. Record supported limits and external-provider ambiguity explicitly; neither is permission to hide a failing supported workload or a workaround. This backlog cannot certify unknown future defects, but no known in-scope code or infrastructure debt may remain open.
- [ ] Publish operating limits, scaling triggers, ownership, runbooks, measured results and explicitly excluded workload classes outside the agreed product requirements. Do not describe the system as extremely scalable until it passes these gates.

## Source and technology references

Reviewed sources include the replay consumer/summarizer, replay message schema, monorepo replay services and database schemas, ClickHouse migration baseline and local `docker-compose.yml`. Deployment recommendations require confirmation against production inventory.

- [Temporal](https://temporal.io/) and the step 5 documentation links: durable workflows/Activities; application-side idempotency and publication fences remain required.
- [ClickHouse Kafka table engine](https://clickhouse.com/docs/integrations/connectors/data-ingestion/kafka/kafka-table-engine): Kafka-fed materialized-view ingestion; duplicate handling must be designed explicitly.
- [PostgreSQL SELECT / SKIP LOCKED](https://www.postgresql.org/docs/current/sql-select.html): bounded concurrent claiming for outbox/maintenance work.
- [pgvector documentation](https://github.com/pgvector/pgvector/blob/master/README.md): exact versus approximate retrieval and filtered-search evaluation.
