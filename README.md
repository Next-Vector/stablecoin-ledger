# Stablecoin Ledger

A double-entry ledger engine in Rust for custodial services that hold stablecoins (USDC, USDT, DAI) on behalf of their users. Internal transfers settle instantly in an off-chain journal; the blockchain is touched only for deposits and withdrawals.

> **Status:** early development — milestone M0 (scaffold). Nothing here is production-ready yet. See the [task list](docs/tasks.md) and the [project board](https://github.com/orgs/Next-Vector/projects/16).

## Documentation

| Document | Contents |
|---|---|
| [docs/specification.md](docs/specification.md) | MVP specification: requirements, API, database schema, acceptance criteria |
| [docs/architecture.md](docs/architecture.md) | How the services and flows fit together, with diagrams |
| [docs/tasks.md](docs/tasks.md) | Milestones and tasks |

## Prerequisites

- [rustup](https://rustup.rs) — the toolchain version is pinned in `rust-toolchain.toml` and installed automatically
- Docker with Compose
- The shared Next-Vector Traefik proxy and its external `proxy` network (`docker network create proxy`), so dev services are reachable at `*.stablecoin-ledger.localhost`

## Getting started

```sh
git clone git@github.com:Next-Vector/stablecoin-ledger.git
cd stablecoin-ledger

# Build and test (the first build also enables the git hooks)
cargo build --workspace
cargo test --workspace
```

### Git hooks

The first local build sets `core.hooksPath=.githooks` for the clone (skipped in CI and when a hooks path is already set). To enable them by hand: `git config core.hooksPath .githooks`.

| Hook | What it enforces |
|---|---|
| `pre-commit` | `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test` |
| `commit-msg` | Subject `type(scope): summary (#N)` ≤ 100 chars, at most one body paragraph, no `Co-Authored-By` trailer |
| `pre-push` | No pushes to `main`; `cargo audit` and `cargo deny` (install with `cargo install --locked cargo-audit cargo-deny`) |

`./scripts/test-hooks.sh` tests all hooks.

## Dev stack

```sh
./run-dev up -d                # PostgreSQL + ledger-api (first start compiles, ~2 min)
./scripts/check-dev-stack.sh   # smoke check
./run-dev down                 # stop
```

| Service | Address |
|---|---|
| ledger-api | `http://api.stablecoin-ledger.localhost` — `/health`, `/health/ready`, `/metrics`, Swagger UI at `/api`, spec at `/api-json` |
| PostgreSQL 18 | `localhost:5432` (see `DATABASE_URL` in `.env`) |

Defaults live in `.env`; put host-specific overrides in `.env.local` (git-ignored). `.env.example` documents every key.

## API keys

Every `/v1` route needs `Authorization: Bearer <key>` (routes, roles and error codes: [spec §8](docs/specification.md#8-rest-api); try them in Swagger UI via **Authorize**); `/health*`, `/metrics`, `/api` and `/api-json` are public. Roles are hierarchical: `admin` ⊇ `write` ⊇ `read`. Keys are issued and revoked by an operator with database access:

```sh
ledger-api create-api-key --role write --name merchant-backend   # prints the key once
ledger-api revoke-api-key <key_id>
```

In the dev stack run them inside the container: `./run-dev exec ledger-api cargo run -q -p ledger-api -- create-api-key --role admin --name dev`.

## Checks

The same gates run in the pre-commit hook and in CI:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo audit                               # CI only
cargo deny check licenses bans sources    # CI only
```

## Fuzzing

The ledger-script DSL has two cargo-fuzz targets (nightly only; `rustup toolchain install nightly --profile minimal && cargo install --locked cargo-fuzz`):

```sh
./scripts/fuzz.sh parse 3600     # parser: never panics, errors point inside the text
./scripts/fuzz.sh compile 3600   # compiler: deterministic, moves only the accounts it reports to be locked
```

The `Fuzz` workflow runs both for 10 minutes every week and on demand.

## License

[Business Source License 1.1](LICENSE). Production use is allowed, except offering the ledger to third parties as a hosted or managed service and connecting a third-party signing service for mainnet. Each version converts to Apache-2.0 four years after its first public release.

The Additional Use Grant wording is preliminary and pending legal review.
