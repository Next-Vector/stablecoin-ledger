# Stablecoin Ledger — Architecture

Every movement of money is a posting in a double-entry journal, and only `ledger-core` can write one: no service touches the journal tables directly. The ledger only keeps accounts and is independent of any chain; everything on-chain lives in private repositories with their own databases — the indexers, the transaction registry (`stablecoin-tx-registry`), the withdrawer (`stablecoin-withdrawer-evm`) and the signer — and reaches the ledger only through its REST API. `stablecoin-dev` runs the whole system locally. This document explains how the parts fit together; the normative requirements (FR/NFR/SIG ids) live in [specification.md](specification.md).

## Components

```mermaid
flowchart TB
    client[Client service]
    net["EVM network<br/>several RPC providers / nodes"]
    idx["indexers × N (private repos)<br/>stablecoin-watcher-evm, …"]
    reg["stablecoin-tx-registry (private)<br/>transactions, statuses, quorum, reconciliation"]
    wd["stablecoin-withdrawer-evm (private)<br/>nonce, broadcast"]
    signer["signer (private repo)<br/>keys, address generation, signing"]

    subgraph ledger [stablecoin-ledger]
        api["ledger-api<br/>REST /v1, auth, idempotency<br/>+ webhook-dispatcher"]
        core["ledger-core + ledger-script<br/>invariants, holds, DSL (no I/O)"]
        store["ledger-store<br/>atomic DB transactions"]
    end

    pg[(PostgreSQL 18)]

    client -- "REST /v1" --> api
    api -- "signed webhooks" --> client
    net -- "events, balances" --> idx
    idx -- "reports" --> reg
    reg -- "REST /v1 postings" --> api
    reg -- "withdrawal orders" --> wd
    wd -- "gRPC + mTLS" --> signer
    wd -- "signed txs" --> net

    api --> core
    core --> store --> pg
```

| Crate | Responsibility |
|---|---|
| `ledger-core` | Accounts, assets, postings, holds, invariants. No I/O, so it is fully testable without a database |
| `ledger-script` | Transaction DSL: parser and compiler to postings. Depends only on `ledger-core` |
| `ledger-store` | PostgreSQL implementation of the storage trait: journal, materialized balances, migrations |
| `ledger-api` | REST `/v1` (axum), API-key auth with roles, idempotency, OpenAPI 3.1; hosts the `webhook-dispatcher` worker (separate command) |

Each chain-side service has its own repository and database. None of them reads or writes the ledger database; the transaction registry is the only one that writes to the ledger, through the API. Every network is watched by several independent indexers, and the registry acts on an event only when the quorum agrees (FR-27).

## The money model

- A **posting** moves an amount of one asset from a source account to a destination account; a **transaction** is an atomic set of 1–256 postings.
- **Invariant:** for every asset the sum of all balances, `world` included, is zero. Only `world` and system accounts may go negative.
- Amounts are `u128` base units with checked arithmetic; floats are forbidden.
- The journal (`transactions`, `postings`) is append-only, enforced by a DB trigger. Corrections are reversals. `balances` is a materialized view updated in the same DB transaction and can be rebuilt from the journal at any time.
- Accounts in a transaction are locked in address order, so concurrent debits never deadlock and never overdraw.

Account layout used by the flows below:

| Account | Holds |
|---|---|
| `world` | The outside world: source of deposits, destination of withdrawals |
| `users:{id}:pending` | Deposits seen on-chain, not yet final |
| `users:{id}:wallet` | Spendable user balance |
| `holds:{id}` | Funds reserved for a withdrawal or other hold |
| `chain:{net}:in_flight` | Withdrawals broadcast but not yet final |
| `platform:fees` · `platform:gas:{net}` | Platform fees · gas spent |

## Writing a transaction

Every write — a transfer, a hold, a script run, a reversal — follows the same path, and everything in the box commits or rolls back together:

```mermaid
sequenceDiagram
    autonumber
    participant C as Client
    participant A as ledger-api
    participant DB as PostgreSQL

    C->>A: POST /v1/transactions + Idempotency-Key
    A->>DB: key seen before? (same request → replay the first answer, other request → 409)
    rect rgb(235, 245, 255)
        Note over A,DB: one DB transaction
        A->>DB: lock balance rows of every touched (account, asset), sorted
        A->>DB: read flags of the sources (frozen, allow_negative), share-locked
        Note over A: plan(): pure rules from ledger-core —<br/>frozen sources, overflow, no overdraft on the net result
        A->>DB: insert transaction + postings (append-only)
        A->>DB: update materialized balances
        A->>DB: insert webhook event + deliveries (outbox)
        A->>DB: record the Idempotency-Key with the transaction id
    end
    A-->>C: 201 + the transaction
```

- The rules live once, in `ledger_core::plan`, and the in-memory journal and PostgreSQL both call it, so the property tests of the core cover the database path too.
- Rows are locked in `(account, asset)` order, so two transfers touching the same accounts queue instead of deadlocking, and a balance is never read stale.
- A request that races another with the same key fails on the key's unique index, rolls back, and is answered from the winner's record: one write, two identical answers.

## Hold lifecycle

```mermaid
stateDiagram-v2
    [*] --> active: POST /v1/holds · wallet → holds:{id}
    active --> captured: capture · holds:{id} → destination(s), rest → owner
    active --> voided: void · holds:{id} → owner
    active --> expired: expires_at passed · sweeper, holds:{id} → owner, hold.expired
    captured --> [*]
    voided --> [*]
    expired --> [*]
```

Each transition is one ledger transaction, so the funds of a hold are always in exactly one place: the owner's wallet, `holds:{id}`, or the destinations. A capture may split between several destinations (a withdrawal's amount and fee). Past its deadline a hold can no longer be captured, only voided, until the sweeper (`FOR UPDATE SKIP LOCKED`, safe on any number of replicas) expires it.

## Deposit flow

The ledger never generates deposit addresses: the address model depends on the key storage type and lives in the signer (or the client's own system). The ledger only registers the mapping, and the transaction registry reads it.

```mermaid
sequenceDiagram
    autonumber
    participant C as Client
    participant S as signer
    participant A as ledger-api
    participant I1 as indexer #1
    participant I2 as indexer #2
    participant R as tx-registry

    C->>S: GetAddress(account)
    S-->>C: deposit address
    C->>A: POST /v1/deposit-addresses
    I1->>R: report Transfer (tx_hash, log_index, block hash)
    I2->>R: report Transfer (same event)
    R->>A: GET /v1/deposit-addresses?network=…&address=… → account
    R->>A: POST /v1/transactions world → users:{id}:pending (Idempotency-Key chain_id:tx_hash:log_index)
    Note over R: webhook deposit.seen
    alt finality reached and the indexers agree on the block hash
        R->>A: users:{id}:pending → users:{id}:wallet
        Note over R: webhook deposit.confirmed
    else block dropped by a reorg before finality
        R->>A: revert the pending posting
        Note over R: webhook deposit.orphaned
    else indexers disagree or one is silent
        Note over R: finalization halted on this network, alert raised
    end
```

Each indexer persists its cursor (block number + hash), the registry stores every report, and every posting is keyed by `chain_id:tx_hash:log_index` (with `:{attempt}` after a reorg re-inclusion), so a restart at any point never double-credits or skips a deposit.

**Sweep** is executed by the signer. The indexers report a `Transfer` from a deposit address to a registered hot wallet as a sweep: user balances stay unchanged, and the registry posts only the actual gas from the receipt (`platform:gas:{net} → world`).

## Withdrawal flow

The transaction registry keeps the withdrawal state machine and writes each transition as one ledger request; the withdrawer signs (through the signer) and broadcasts; the indexers report the on-chain result, which the registry matches by `(hot wallet, nonce)`, since a re-sent transaction has a new hash.

```mermaid
stateDiagram-v2
    [*] --> requested: withdrawal request to the registry · hold wallet → hold account
    requested --> broadcast: order and tx signed · capture → in_flight + fees
    requested --> failed: rejected before broadcast · void hold
    broadcast --> broadcast: stuck · re-send with same nonce, higher gas
    broadcast --> confirmed: finality · in_flight → world, gas posted
    broadcast --> failed: reverted · in_flight → wallet, fee reversed
    confirmed --> [*]
    failed --> [*]
```

- Every transition's posting is one atomic ledger transaction, guarded by an idempotency key.
- The nonce and the signed transaction are persisted **before** broadcast; after a crash the withdrawer only re-sends the same transaction, so a payout can never happen twice.
- The registry signs a withdrawal order (ed25519) when the hold is created; the signer refuses any transaction that does not match a valid order.
- Without a connected signer withdrawals are not processed and the registry's endpoint returns `501 SIGNER_NOT_CONFIGURED`.
- An outgoing hot-wallet transfer that matches no withdrawal raises an alert.

## Webhooks

```mermaid
flowchart LR
    change["state change<br/>(posting, hold, account)"] -->|same DB transaction| outbox[(webhook_events)]
    outbox -->|FOR UPDATE SKIP LOCKED| d1[dispatcher #1]
    outbox -->|FOR UPDATE SKIP LOCKED| d2[dispatcher #2]
    d1 -->|"POST + X-Ledger-Signature (HMAC-SHA256)"| rcv[Receiver]
    d2 --> rcv
    rcv -. "non-2xx / timeout" .-> retry["retry with backoff<br/>30 s → 24 h, ≤ 15 attempts"]
    retry -.-> outbox
```

The transactional outbox guarantees an event exists if and only if its change was committed. Delivery is at-least-once; receivers deduplicate by `X-Ledger-Event-Id` and order within a resource by `sequence`.

## Reconciliation

Once a day (default 00:00 UTC) the transaction registry checks, per asset, with balances read from the ledger API:

```text
Σ on-chain balance (finalized block) = −balance(world) − Σ balance(users:*:pending)
```

On-chain balances come from the indexers' quorum. The result is stored; a non-zero difference raises an alert, emits the registry's `reconciliation.mismatch` webhook and, if configured, halts withdrawals of that asset.

## Trust boundaries and open core

```mermaid
flowchart LR
    subgraph public [Public repo — stablecoin-ledger]
        ledger[ledger-api]
    end
    subgraph chainside [Private repos — indexers, stablecoin-tx-registry, stablecoin-withdrawer-evm]
        reg[tx-registry]
        wd[withdrawer]
        local[signer-local<br/>testnets only]
    end
    subgraph private [Private repo — signer, paid]
        srv[signer-server<br/>policies, audit, tenants]
        keys["key backends<br/>keystore · AWS/GCP KMS · HSM · Ledger · HD seed"]
        addr[address generation + sweep]
    end
    reg -- "REST /v1" --> ledger
    wd -- "Signer trait, gRPC + mTLS" --> srv
    wd -. dev / CI .-> local
    srv --> keys
    srv --> addr
```

- Private keys never enter this repository; the ledger does not talk to the signer at all.
- Without a signer subscription the chain side is read-only towards the chain: deposits and reconciliation work, withdrawals are disabled; the ledger itself is unaffected.
- `signer-local` implements the same contract, so the withdrawer builds and passes e2e tests without the `signer` repository.

## Technology stack

| Layer | Choice |
|---|---|
| Language / runtime | Rust stable (edition 2024), `tokio` |
| HTTP | `axum`, `tower`, `utoipa` (OpenAPI 3.1) |
| Storage | PostgreSQL 18, `sqlx` with compile-time checked queries |
| Blockchain | none in the ledger; the indexers and the withdrawer use `alloy` (EVM), anvil locally and Ethereum Sepolia |
| Observability | `tracing` (JSON logs), OpenTelemetry, Prometheus metrics |
| Tests | `proptest`, `testcontainers`, `criterion`, anvil, `wiremock`, `cargo-fuzz` |
