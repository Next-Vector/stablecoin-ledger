# Stablecoin Ledger — Task List

MVP work broken into 7 milestones and 38 tasks (the blockchain milestones M6 and M7 moved to another repository), following the stages in [specification.md](specification.md) §11. Each task is one commit with tests that prove it; each milestone closes with a PR into `dev`. Each task is a GitHub issue on the [project board](https://github.com/orgs/Next-Vector/projects/16); task numbers are issue numbers, and tasks added during the work are listed with their milestone.

| Milestone | Tasks |
|---|---|
| [M0 · Scaffold](#m0--scaffold) | 5 |
| [M1 · ledger-core](#m1--ledger-core) | 8 |
| [M2 · ledger-store](#m2--ledger-store) | 5 |
| [M3 · ledger-api + DSL](#m3--ledger-api--dsl) | 9 |
| [M4 · Webhooks](#m4--webhooks) | 5 |
| [M5 · chain-watcher](#m5--chain-watcher) | 3 |
| [M8 · Polish](#m8--polish) | 3 |

## M0 · Scaffold

Workspace, CI, pre-commit hook, Docker Compose, README. Spec: docs/specification.md §11 stage 0.

### 1. Cargo workspace with crate skeletons

- [ ] Done
- **Requirements:** NFR-11, §7
- **Scope:** Create the Cargo workspace (Rust stable, edition 2024) with empty crates: ledger-core, ledger-script, ledger-store, ledger-api, chain-watcher, withdrawer, reconciler, signer-proto, signer-local (chain-watcher, withdrawer and reconciler were later removed, #64). `#![forbid(unsafe_code)]` in every crate.
- **Done when:** `cargo build --workspace` and `cargo test --workspace` pass on a clean checkout.

### 2. CI pipeline: fmt, clippy, test, audit, deny

- [ ] Done
- **Requirements:** NFR-11, NFR-14
- **Scope:** GitHub Actions on `runs-on: [self-hosted, light]`, one ordered fail-fast job: `cargo fmt --check` → `cargo clippy -D warnings` → `cargo test` → `cargo audit` + `cargo deny`. Runs on push and PR to `dev`/`main`.
- **Done when:** A PR with a clippy warning or failing test is red; a clean PR is green.

### 3. Pre-commit hook: fmt, clippy, unit tests

- [ ] Done
- **Requirements:** CLAUDE.md — pre-commit hook
- **Scope:** Committed hook that blocks a commit on `cargo fmt --check`, `cargo clippy -D warnings` or `cargo test` failure. Heavier suites (integration, e2e) stay in CI.
- **Done when:** A commit with a formatting error is rejected locally.

### 4. Docker Compose dev stack: PostgreSQL + anvil

- [ ] Done
- **Requirements:** §3, CLAUDE.md — container conventions
- **Scope:** `.docker/docker-compose-dev.yml` with PostgreSQL 16 and anvil, `run-dev` wrapper, `.env` + `.env.example` (+ git-ignored `.env.local`). Multi-stage Dockerfile with a `dev` target before the prod target. HTTP services go behind the shared Traefik proxy (`api.stablecoin-ledger.localhost`), no host port for the API.
- **Done when:** `./run-dev up -d` starts PostgreSQL and anvil; services are reachable.

### 5. README and LICENSE (BSL 1.1)

- [ ] Done
- **Requirements:** §1, §12
- **Scope:** Root README: what the project is, how to run locally, link to docs/. LICENSE with BSL 1.1 parameters (Change Date 4 years, Change License Apache-2.0, Additional Use Grant per §12 — pending legal review).
- **Done when:** README renders on GitHub; LICENSE present.

## M1 · ledger-core

Types, validation, in-memory journal, holds, property tests. No I/O. Spec §4, §5.1, stage 1.

### 6. Account address and asset types with validation

- [ ] Done
- **Requirements:** §4.1, §4.2
- **Scope:** `AccountAddress` (`[a-z0-9_]+` segments, `:`, ≤ 256 chars), reserved namespaces, `AssetId` (`SYMBOL@network/decimals`). Amounts are `u128` base units; no floats anywhere.
- **Done when:** Unit tests cover valid/invalid addresses and assets.

### 7. Posting and Transaction types with checked arithmetic

- [ ] Done
- **Requirements:** FR-01, NFR-03, §4.3
- **Scope:** `Posting`, `Transaction` (1..=256 postings), rules 4.3.1–4.3.3. All amount arithmetic via `checked_add`/`checked_sub`; overflow is an error, never a panic.
- **Done when:** Unit tests for every rule and for overflow.

### 8. In-memory journal: atomic apply, insufficient funds, frozen accounts

- [ ] Done
- **Requirements:** FR-01, FR-02, FR-08
- **Scope:** Apply a transaction atomically; reject with `INSUFFICIENT_FUNDS` if an account without `allow_negative` goes negative; frozen accounts cannot be a source but can receive.
- **Done when:** Unit tests: partial failure leaves no trace; frozen source rejected; frozen destination credited.

### 9. Reversals, point-in-time and aggregated balances

- [ ] Done
- **Requirements:** FR-03, FR-04, FR-05
- **Scope:** Reverse a transaction once (mirrored postings + reference). Balance per asset now and as of a transaction id. Aggregated balances by pattern (`users:*:wallet`).
- **Done when:** Unit tests incl. double-reversal rejection.

### 10. Holds: hold, capture, void, expiry

- [ ] Done
- **Requirements:** FR-07, §4.4
- **Scope:** Hold moves funds to `holds:{hold_id}`; partial/full capture returns the remainder; void returns all; expired holds void automatically. Every action is an ordinary transaction.
- **Done when:** Unit tests for each transition and expiry.

### 11. Property tests: zero-sum invariant and no negative balances

- [ ] Done
- **Requirements:** §4.3.4, §10
- **Scope:** `proptest` over random sequences of transfers, reversals and hold operations.
- **Done when:** 10,000 random operations find no invariant violation.

### 48. Git hooks: commit-msg, pre-push, auto-enable

- [ ] Done
- **Requirements:** CLAUDE.md commit and branching rules
- **Scope:** commit-msg enforces `type(scope): summary (#N)`, one body paragraph at most and no `Co-Authored-By`; pre-push refuses pushes to `main` and runs `cargo audit` and `cargo deny`; the first local build sets `core.hooksPath=.githooks` (skipped in CI).
- **Done when:** `scripts/test-hooks.sh` proves each hook rejects a bad case and accepts a good one; it runs in CI.

### 49. Dependabot for Cargo, GitHub Actions and Docker images

- [ ] Done
- **Requirements:** CLAUDE.md supply-chain rules
- **Scope:** weekly updates for `cargo`, `github-actions` and `docker-compose`, PRs into `dev`, minor and patch bumps grouped per ecosystem.
- **Done when:** `.github/dependabot.yml` validates against the official schema in CI.

## M2 · ledger-store

PostgreSQL persistence, migrations, atomicity, concurrency, rebuild. Spec §9, stage 2.

### 12. PostgreSQL schema and migrations

- [ ] Done
- **Requirements:** §9
- **Scope:** sqlx migrations for transactions, postings, balances, accounts, holds (+ indexes for history queries). Trigger forbidding UPDATE/DELETE on transactions and postings. Amounts `NUMERIC(39,0)`.
- **Done when:** Integration test (testcontainers) applies migrations and proves UPDATE/DELETE are rejected.

### 13. Storage trait: atomic apply with materialized balances

- [ ] Done
- **Requirements:** NFR-01, NFR-02, NFR-06
- **Scope:** Implement the ledger-core storage trait: postings and balance updates in one DB transaction, accounts locked in deterministic (address-sorted) order, O(1) current balance reads.
- **Done when:** Integration tests: atomicity on failure, no deadlocks under concurrent cross transfers.

### 14. Concurrency test: 1,000 parallel debits

- [ ] Done
- **Requirements:** NFR-01, §10
- **Scope:** 1,000 parallel debits of 1 USDC from an account holding 500 USDC.
- **Done when:** Exactly 500 successes and 500 `INSUFFICIENT_FUNDS`; balance is 0.

### 15. `ledger rebuild-balances` command

- [ ] Done
- **Requirements:** NFR-08
- **Scope:** Recompute `balances` entirely from the posting journal.
- **Done when:** Integration test: corrupt balances, rebuild, values match the journal.

### 16. Transaction history with filters and cursor pagination

- [ ] Done
- **Requirements:** FR-06
- **Scope:** Filter by account, asset, period, metadata; stable cursor pagination with a max page size.
- **Done when:** Integration tests for each filter and for pagination stability.

## M3 · ledger-api + DSL

REST API, idempotency, auth, OpenAPI, ledger-script DSL with dry-run. Spec §5.2, §5.7, §8, stage 3.

### 17. API skeleton: config validation, error envelope, health, metrics, OpenAPI

- [ ] Done
- **Requirements:** §8, NFR-12, NFR-13
- **Scope:** axum + tower; config validated at boot (fail fast); single error format `{"error":{code,message,details}}`; `/health`, `/health/ready`, `/metrics`; OpenAPI 3.1 via utoipa; structured JSON logs; graceful shutdown.
- **Done when:** Integration tests: bad config fails at boot; unknown route returns the error envelope; OpenAPI served.

### 18. API key authentication with roles

- [ ] Done
- **Requirements:** NFR-09
- **Scope:** Keys with `read`/`write`/`admin` roles, stored hashed; deny by default.
- **Done when:** Tests: missing/invalid key → 401, insufficient role → 403.

### 19. Endpoints: transactions, accounts, balances, holds, freeze

- [ ] Done
- **Requirements:** FR-01–FR-08, §8
- **Scope:** Routes from the §8 table for the journal core. Amounts as strings.
- **Done when:** e2e tests over HTTP for each route, incl. `INSUFFICIENT_FUNDS`.

### 20. Idempotency-Key on all mutating requests

- [ ] Done
- **Requirements:** FR-10, FR-11
- **Scope:** Same key + same body → first result; same key + different body → `409 IDEMPOTENCY_CONFLICT`.
- **Done when:** Tests: retry creates no second transaction; conflicting body → 409.

### 21. ledger-script: parser and AST

- [ ] Done
- **Requirements:** FR-60, FR-61, FR-64
- **Scope:** Parser (chumsky or hand-written) with line/column/expected-token errors; typed variables; no loops, recursion, network or time; script ≤ 64 KB.
- **Done when:** Golden tests for every construct; error-message tests.

### 22. ledger-script: compiler to postings

- [ ] Done
- **Requirements:** FR-62, FR-63
- **Scope:** Ordered sources with `max` caps and explicit overdraft; destinations by percentage/fraction + `remaining`; deterministic rounding; ≤ 256 postings.
- **Done when:** Golden tests + property test: shares always sum to the input.

### 23. Script endpoints: dry-run and stored scripts

- [ ] Done
- **Requirements:** FR-65, FR-66
- **Scope:** `POST /v1/scripts/dry-run`; `POST·GET /v1/scripts`, `GET /v1/scripts/{name}/versions`; `POST /v1/transactions` with `script`/`script_name` + `vars`; script text and vars saved in metadata.
- **Done when:** e2e tests: dry-run writes nothing; stored script runs by name.

### 24. DSL fuzzing

- [ ] Done
- **Requirements:** §10
- **Scope:** cargo-fuzz target for the parser.
- **Done when:** 1 hour of fuzzing without panics.

### 56. Docs: sync the task list and schema with the implementation

- [ ] Done
- **Requirements:** —
- **Scope:** add the tasks created during the work to this list; align the §9 `holds` table in the specification with the migration.
- **Done when:** this list matches the board and the specification matches the migrations.

## M4 · Webhooks

Outbox, delivery worker, retries, HMAC signatures, delivery log. Spec §5.6, stage 4.

### 25. Webhook subscriptions API and secret rotation

- [ ] Done
- **Requirements:** FR-50, FR-54
- **Scope:** CRUD for endpoints (URL, event types, secret, enabled); rotate-secret keeps the old secret valid for 24 h.
- **Done when:** e2e tests for CRUD and rotation window.

### 26. Transactional outbox for webhook events

- [ ] Done
- **Requirements:** FR-51, FR-52, FR-55
- **Scope:** Write events to `webhook_events` in the same DB transaction as the change; unique id and per-resource `sequence`.
- **Done when:** Test: killing the process right after commit still delivers the event after restart.

### 27. webhook-dispatcher: delivery, retries, HMAC signature

- [ ] Done
- **Requirements:** FR-53, FR-54
- **Scope:** `SELECT … FOR UPDATE SKIP LOCKED`; POST JSON, 10 s timeout, 2xx success; exponential backoff 30 s → 24 h, ≤ 15 attempts; `X-Ledger-Signature` HMAC-SHA256, `X-Ledger-Event-Id`.
- **Done when:** wiremock receiver verifies every signature; on 500 delivery is retried on schedule; two dispatchers never double-deliver.

### 28. Delivery log, manual redelivery, SSRF protection

- [ ] Done
- **Requirements:** FR-56, FR-57
- **Scope:** Deliveries endpoint; `POST /v1/webhook-events/{id}/redeliver`; reject private/loopback/link-local URLs in production config.
- **Done when:** Tests for log content, redelivery and SSRF rejection.

### 57. Expire overdue holds automatically in PostgreSQL

- [ ] Done
- **Requirements:** FR-07
- **Scope:** a periodic job voids holds past `expires_at` as `expired`, each in its own DB transaction with `FOR UPDATE SKIP LOCKED` so replicas can share the work, and emits `hold.expired`.
- **Done when:** integration tests: a due hold is voided and its funds return; a hold not yet due is untouched; concurrent sweepers void each hold once.

## M5 · chain-watcher

The ledger side of deposits: the registries the chain module relies on. Deposit indexing, withdrawals and reconciliation moved to the private `stablecoin-chain-watcher` repository (see below). Spec §5.3, stage 5.

### 29. Asset registry and deposit address registration API

- [ ] Done
- **Requirements:** FR-20, §4.2
- **Scope:** `assets` registry (asset ids only; chain details belong to the chain module). `POST /v1/deposit-addresses` registers (network, address) → wallet account, the address as an opaque string; an address belongs to one account, an account has one address per network. The ledger never generates or watches addresses.
- **Done when:** e2e tests: register, duplicate address rejected.

### 62. Serve Swagger UI at /api

- [ ] Done
- **Requirements:** NFR-11
- **Scope:** Swagger UI at the API root `/api`, the raw spec at `/api-json`, both public.
- **Done when:** `/api` serves Swagger UI loading `/api-json`; README updated.

### 64. Remove chain-watcher, withdrawer and reconciler from the ledger

- [ ] Done
- **Requirements:** §3, §7
- **Scope:** drop the empty `chain-watcher`, `withdrawer` and `reconciler` crates; describe them in the docs as the external chain module.
- **Done when:** the workspace builds and passes without them; the docs no longer present them as ledger components.

## Moved to stablecoin-chain-watcher

The ledger only keeps accounts and stays independent of any chain, so the blockchain milestones moved to the private `stablecoin-chain-watcher` repository, which uses this ledger through its API. GitHub redirects the old issue links.

| Milestone | Former issues | Now in stablecoin-chain-watcher |
|---|---|---|
| M5 · chain-watcher | #30–#34 | #1–#5 |
| M6 · withdrawer | #35–#39 | #6–#10 |
| M7 · reconciler | #40–#41 | #11–#12 |

## M8 · Polish

Benchmarks, demo, diagrams, write-up. Spec stage 8.

### 42. Benchmarks for throughput and latency

- [ ] Done
- **Requirements:** NFR-04, NFR-05
- **Scope:** criterion + k6/oha: ≥ 2,000 transfers/s on one PostgreSQL (8 vCPU); p99 `POST /transactions` < 50 ms at 50% of peak.
- **Done when:** Results published in the README.

### 43. One-command demo with docker compose

- [ ] Done
- **Requirements:** §10
- **Scope:** `docker compose up` brings up the whole system and runs a demo script (deposit, transfer, withdrawal, reconciliation).
- **Done when:** Demo runs end to end on a clean machine.

### 44. Architecture diagrams and write-up article

- [ ] Done
- **Requirements:** §11 stage 8
- **Scope:** Final diagrams in docs/ and an article explaining the design decisions.
- **Done when:** Published in docs/.
