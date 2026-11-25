# Designing a stablecoin ledger that cannot lose money

A custodial service that holds stablecoins for its users is, underneath, a bookkeeping problem with a blockchain attached. Users pay each other thousands of times a day; the chain is touched only when money comes in or goes out. This article walks through the decisions behind `stablecoin-ledger` — what each one protects against and what it costs — in the order money flows through the system. The normative requirements are in the [specification](specification.md); the diagrams are in [architecture.md](architecture.md).

## 1. Double entry, and nothing else

Every movement of money is a **posting**: an amount of one asset from a source account to a destination account. A **transaction** is an atomic set of 1–256 postings. Money is never created or destroyed inside the ledger, only moved, which gives one invariant that is cheap to check and catches almost every bug:

> For every asset, the sum of all balances — the `world` account included — is zero.

Money entering the system comes *from* `world` (which goes negative by the amount that came in), and money leaving goes back *to* it. Reconciliation then becomes arithmetic: `−balance(world)` is what the platform owes, and it must match what is on chain.

Accounts are hierarchical names (`users:42:wallet`, `holds:17`, `platform:fees`) rather than rows that must exist first. An account exists from its first posting; a row in `accounts` appears only when something is set on it (frozen, may go negative). Patterns such as `users:*:wallet` aggregate across users without a schema change.

**Cost:** clients have to think in postings. The DSL (section 6) exists so that they rarely write them by hand.

## 2. Integers in base units

Amounts are `u128` base units — `1500000` is 1.5 USDC — with checked arithmetic everywhere; floats are banned from the codebase. `u128` holds any ERC-20 supply (18 decimals included). The API sends amounts as strings because JSON numbers cannot carry 128 bits. An asset is `SYMBOL@network/decimals` (`USDC@ethereum/6`), so the same coin on two networks is two assets and a posting can never mix them.

## 3. An append-only journal with materialized balances

`transactions` and `postings` are the source of truth, and PostgreSQL triggers reject `UPDATE`, `DELETE` and `TRUNCATE` on them. A mistake is corrected by a **reversal**: a new transaction with the postings mirrored, allowed once per transaction. Nothing is ever rewritten, so the history is an audit log by construction.

Reading balances by summing the journal would get slower forever, so `balances` is a materialized table updated in the same DB transaction as the postings. Because it is derived, it can be recomputed at any time (`ledger-api rebuild-balances`), and the integration tests check that the rebuild finds nothing to fix.

**Cost:** two writes per posting instead of one, and corrections show up as extra transactions rather than disappearing.

## 4. The rules in one pure function

All the rules that decide whether a transaction may apply — a frozen account cannot send, arithmetic must not overflow, no account may end negative unless it is `world` or flagged `allow_negative` — live in one function, `ledger_core::plan`. It takes the transaction, the current balances and the account flags, and returns the new balances or a rejection. It does no I/O.

Both stores call it: the in-memory journal used by the property tests, and PostgreSQL. A rule exists exactly once, and the property tests (random transactions, the zero-sum invariant checked after each) cover the database path as well. Negativity is checked on the **net** result of the transaction, so "credit then debit" inside one transaction works as people expect.

`allow_negative` exists for system accounts that represent expenses: gas is paid by the platform, so `platform:gas:ethereum → world` drives that account negative on purpose. It is an operator setting (`ledger-api allow-negative <account>`), never something an API client can set.

## 5. Concurrency without overdrafts or deadlocks

A transfer locks the balance rows of every `(account, asset)` it touches, **in sorted order**, then plans and writes. Sorting means two transfers between the same accounts queue instead of deadlocking; locking means the balance a rule reads is the balance it writes against. The flags of the sources (`frozen`, `allow_negative`) are read under a share lock, so an account cannot be frozen between the check and the commit.

Background work that several replicas share — webhook delivery, hold expiry — claims rows with `FOR UPDATE SKIP LOCKED`, so replicas divide the work instead of waiting for each other, and each hold expires exactly once (a test runs four sweepers on twenty holds to prove it).

**Cost:** a very hot account (one merchant receiving everything) serializes its writers. The specification lists sharding such an account across sub-accounts as the mitigation if that ever matters.

## 6. A small language for money flows

Splitting a payment between a seller, a platform fee and a referral, or drawing from several accounts in order, is error-prone as hand-built postings. The ledger compiles a small DSL, inspired by Numscript, into postings and applies them atomically:

```text
send [USDC@ethereum/6 100000000] (
  source = @users:42:wallet
  destination = {
    90% to @merchants:7:wallet
    remaining to @platform:fees
  }
)
```

Rounding is deterministic (each share gets its floor; `remaining` takes the rest, or without it the leftover base units go one each to the shares in order, so the parts always sum to the input), scripts are versioned and stored, and `POST /v1/scripts/dry-run` shows the postings and resulting balances without writing anything. The parser and compiler are fuzzed with `cargo-fuzz`.

## 7. Idempotency in the same transaction as the write

Networks fail after the server commits and before the client hears back, so every mutating request accepts an `Idempotency-Key`. The key is recorded **in the same DB transaction as the write**, with a hash of the request. A retry with the same key and body returns the original result with `Idempotent-Replayed: true`; the same key with a different body is a `409`; a rejected request records nothing, so retrying it is evaluated again. If two copies race, the loser fails on the key's unique index, rolls back and answers from the winner's record — one write, two identical answers.

The same mechanism makes chain events safe: the transaction registry posts a deposit with the key `{chain_id}:{tx_hash}:{log_index}`, so a restart can never credit twice. A transfer reversed by a reorg and then included in another block is credited under `…:{log_index}:2`, because the original key would only replay the reversed transaction.

## 8. Webhooks through a transactional outbox

Clients learn about changes from webhooks. The event row is written in the **same DB transaction** as the change, together with one delivery per subscription; an event therefore exists if and only if its change committed — no lost events, no phantom ones. A dispatcher inside `ledger-api` sends deliveries (HMAC-SHA256 signature over timestamp and body, 10 s timeout, redirects not followed), retries failures from 30 s doubling up to 24 h for 15 attempts, and logs every attempt. A claimed delivery is leased for 60 s, so one lost with a crashed process is retried and a late outcome is ignored.

Delivery is at-least-once: receivers deduplicate by event id and order within a resource by `sequence`. Signing secrets are encrypted at rest (AES-256-GCM) because HMAC needs the secret itself; rotation keeps the old secret valid for 24 hours. In production a receiver URL must resolve only to public addresses — checked when it is saved and again at every attempt through a resolver that refuses internal addresses, so DNS rebinding cannot reach the internal network.

## 9. The ledger knows nothing about blockchains

The first design had the chain indexer, the withdrawal sender and reconciliation inside this repository. They are gone. The ledger keeps accounts; everything on chain is a separate service with its own database that talks to the ledger only through its API:

- **Indexers** — one repository per kind (`stablecoin-watcher-evm` is the first, for EVM), several independent instances per network on different RPC providers — read the chain and report events.
- **The transaction registry** (`stablecoin-tx-registry`) stores every on-chain transaction and its status, acts only when a quorum of indexers agrees on an event and its block hash, posts the result to the ledger, and reconciles daily.
- **The withdrawer** (`stablecoin-withdrawer-evm`) manages nonces and broadcasts; it is the only service that talks to the signer.
- **The signer** generates deposit addresses and signs, in its own private repository.

Why split it: a ledger that cannot reach a chain cannot be tricked by one. A single compromised or buggy indexer can never move money, because the registry needs agreement from independent sources. Withdrawal confirmations come from the same indexers, matched by `(hot wallet, nonce)` because a re-sent transaction has a new hash, and an outgoing transfer that matches no withdrawal raises an alert. The ledger is also usable on its own — internal transfers, holds and webhooks — for systems that have no blockchain at all.

What the ledger keeps for the chain side is small: a registry of the assets it accounts for and of deposit addresses (an address is an opaque string, so EVM, Tron and Solana formats all fit), and two operations withdrawals need — a capture that splits between several destinations (amount in flight, fee to the platform) and system accounts that may go negative (gas).

**Cost:** more repositories and a network hop for every deposit. `stablecoin-dev` runs them all locally with one command to keep development bearable.

## 10. Holds that cannot be forgotten

A hold moves funds to `holds:{id}`, where nothing else can spend them, and ends in exactly one way: captured (to one or more destinations, the rest back to the owner), voided, or expired. Each ending is a single ledger transaction, so the funds are always in one place. A hold may have a deadline; past it a capture is refused at once, and a sweeper returns the funds and emits `hold.expired`.

## 11. Proving it

Every task in this repository was built test-first. The levels:

- **Unit and property tests** in `ledger-core` and `ledger-script`, including random transaction sequences checked against the zero-sum invariant.
- **Integration tests** against a real PostgreSQL (a throwaway container), including concurrency: parallel transfers never overdraw, parallel sweepers expire each hold once, racing idempotent requests write once.
- **HTTP tests** through the full router, authentication included.
- **Fuzzing** of the DSL.

A test that was only ever seen passing proves nothing, so most changes were also checked by **defect injection**: remove the guard (the row lock, the deadline filter, the checksum, the unique index), watch the test fail, put it back. Several tests were fixed that way — for example, the parallel hold-expiry test kept passing without the hold row lock until it was moved to a multi-threaded runtime, where it failed three times out of three.

## What is next

- **Benchmarks on dedicated hardware.** On a shared laptop the ledger reaches 1,971–2,303 transfers/s (NFR-04: ≥ 2,000) and a p99 of 32 ms at 1,000 requests/s (NFR-05: < 50 ms); see the README for the setup.
- **A one-command demo** of the whole system through `stablecoin-dev`.
- The chain side: the indexer, the transaction registry and the withdrawer, each in its own repository.
