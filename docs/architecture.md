# Stablecoin Ledger — Architecture

Every movement of money is a posting in a double-entry journal, and only `ledger-core` can write one: no service touches the journal tables directly. The ledger only keeps accounts and is independent of any chain; deposit scanning, withdrawals and reconciliation belong to the chain module (private repository `stablecoin-chain-watcher`), which uses the ledger through its REST API. This document explains how the parts fit together; the normative requirements (FR/NFR/SIG ids) live in [specification.md](specification.md).

## Components

```mermaid
flowchart TB
    client[Client service]
    net["EVM network<br/>RPC source #1 + RPC source #2"]
    signer["signer (private repo)<br/>keys, address models, sweep"]
    chain["chain module (private repo stablecoin-chain-watcher)<br/>deposits, withdrawals, reconciliation"]

    subgraph ledger [stablecoin-ledger]
        api["ledger-api<br/>REST /v1, auth, idempotency<br/>+ webhook-dispatcher"]
        core["ledger-core + ledger-script<br/>invariants, holds, DSL (no I/O)"]
        store["ledger-store<br/>atomic DB transactions"]
    end

    pg[(PostgreSQL 18)]

    client -- "REST /v1" --> api
    api -- "signed webhooks" --> client
    chain -- "REST /v1" --> api
    net <-- "events, balances, signed txs" --> chain
    chain -- "gRPC + mTLS" --> signer

    api --> core
    core --> store --> pg
```

| Crate | Responsibility |
|---|---|
| `ledger-core` | Accounts, assets, postings, holds, invariants. No I/O, so it is fully testable without a database |
| `ledger-script` | Transaction DSL: parser and compiler to postings. Depends only on `ledger-core` |
| `ledger-store` | PostgreSQL implementation of the storage trait: journal, materialized balances, migrations |
| `ledger-api` | REST `/v1` (axum), API-key auth with roles, idempotency, OpenAPI 3.1; hosts the `webhook-dispatcher` worker (separate command) |
| `signer-proto` | gRPC contract and the `Signer` trait |
| `signer-local` | Local signer for anvil and Sepolia; refuses to start with a mainnet network |

The chain module is a separate service in its own repository: it never reads or writes the ledger database, only the API, so the two scale, deploy and fail independently.

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

## Deposit flow (chain module)

The ledger never generates deposit addresses: the address model depends on the key storage type and lives in the signer (or the client's own system). The ledger only registers the mapping.

```mermaid
sequenceDiagram
    autonumber
    participant C as Client
    participant S as signer
    participant A as ledger-api
    participant W as chain module
    participant R1 as RPC #1
    participant R2 as RPC #2
    participant L as ledger-core

    C->>S: GetAddress(account)
    S-->>C: deposit address
    C->>A: POST /v1/deposit-addresses
    W->>R1: Transfer logs from cursor
    W->>R2: Transfer logs from cursor
    W->>A: POST /v1/transactions world → users:{id}:pending (Idempotency-Key chain_id:tx_hash:log_index)
    A->>L: apply
    Note over W,L: webhook deposit.seen
    alt finality reached and both sources agree on the block hash
        W->>A: users:{id}:pending → users:{id}:wallet
        Note over W,L: webhook deposit.confirmed
    else block dropped by a reorg before finality
        W->>A: revert the pending posting
        Note over W,L: webhook deposit.orphaned
    else sources disagree or one is down
        Note over W: finalization halted on this network, alert raised
    end
```

The cursor (block number + hash) is persisted, and every event is keyed by `chain_id:tx_hash:log_index`, so a restart at any point never double-credits or skips a deposit.

**Sweep** is executed by the signer. The chain module recognizes a `Transfer` from a deposit address to a registered hot wallet as a sweep: user balances stay unchanged, and only the actual gas from the receipt is posted (`platform:gas:{net} → world`).

## Withdrawal flow (chain module)

The chain module drives withdrawals with the ledger's holds and transactions; each transition below is one ledger request.

```mermaid
stateDiagram-v2
    [*] --> requested: POST /v1/withdrawals · hold wallet → hold account
    requested --> broadcast: order and tx signed · capture → in_flight + fees
    requested --> failed: rejected before broadcast · void hold
    broadcast --> broadcast: stuck · re-send with same nonce, higher gas
    broadcast --> confirmed: finality · in_flight → world, gas posted
    broadcast --> failed: reverted · in_flight → wallet, fee reversed
    confirmed --> [*]
    failed --> [*]
```

- Every transition's posting is one atomic ledger transaction, guarded by an idempotency key.
- The nonce and the signed transaction are persisted **before** broadcast; after a crash the chain module only re-sends the same transaction, so a payout can never happen twice.
- The chain module signs a withdrawal order (ed25519) when the hold is created; the signer refuses any transaction that does not match a valid order.
- Without a connected signer the chain module does not process withdrawals and its endpoint returns `501 SIGNER_NOT_CONFIGURED`.

## Webhooks

```mermaid
flowchart LR
    change["state change<br/>(posting, deposit, withdrawal)"] -->|same DB transaction| outbox[(webhook_events)]
    outbox -->|FOR UPDATE SKIP LOCKED| d1[dispatcher #1]
    outbox -->|FOR UPDATE SKIP LOCKED| d2[dispatcher #2]
    d1 -->|"POST + X-Ledger-Signature (HMAC-SHA256)"| rcv[Receiver]
    d2 --> rcv
    rcv -. "non-2xx / timeout" .-> retry["retry with backoff<br/>30 s → 24 h, ≤ 15 attempts"]
    retry -.-> outbox
```

The transactional outbox guarantees an event exists if and only if its change was committed. Delivery is at-least-once; receivers deduplicate by `X-Ledger-Event-Id` and order within a resource by `sequence`.

## Reconciliation (chain module)

Once a day (default 00:00 UTC) the chain module checks, per asset, with balances read from the ledger API:

```text
Σ on-chain balance (finalized block) = −balance(world) − Σ balance(users:*:pending)
```

On-chain balances are read from both RPC sources. The result is stored; a non-zero difference raises an alert, emits `reconciliation.mismatch` and, if configured, halts withdrawals of that asset.

## Trust boundaries and open core

```mermaid
flowchart LR
    subgraph public [Public repo — stablecoin-ledger]
        ledger[ledger-api]
        local[signer-local<br/>testnets only]
    end
    chain[chain module<br/>private repo]
    subgraph private [Private repo — signer, paid]
        srv[signer-server<br/>policies, audit, tenants]
        keys["key backends<br/>keystore · AWS/GCP KMS · HSM · Ledger · HD seed"]
        addr[address models + sweep]
    end
    chain -- "REST /v1" --> ledger
    chain -- "Signer trait, gRPC + mTLS" --> srv
    chain -. dev / CI .-> local
    srv --> keys
    srv --> addr
```

- Private keys never enter this repository; the ledger does not talk to the signer at all.
- Without a signer subscription the chain module is read-only towards the chain: deposits and reconciliation work, withdrawals are disabled; the ledger itself is unaffected.
- `signer-local` implements the same contract, so the public repository builds and passes e2e tests on its own.

## Technology stack

| Layer | Choice |
|---|---|
| Language / runtime | Rust stable (edition 2024), `tokio` |
| HTTP | `axum`, `tower`, `utoipa` (OpenAPI 3.1) |
| Storage | PostgreSQL 18, `sqlx` with compile-time checked queries |
| Blockchain | none in the ledger; the chain module uses `alloy` (EVM), anvil locally and Ethereum Sepolia |
| Observability | `tracing` (JSON logs), OpenTelemetry, Prometheus metrics |
| Tests | `proptest`, `testcontainers`, `criterion`, anvil, `wiremock`, `cargo-fuzz` |
