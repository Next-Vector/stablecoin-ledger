# Stablecoin Ledger — Documentation

Stablecoin Ledger is a double-entry ledger engine in Rust for custodial stablecoin services: internal transfers settle off-chain, and the blockchain is touched only for deposits and withdrawals.

| Document | Contents |
|---|---|
| [specification.md](specification.md) | Full MVP specification: scope, data model, functional (FR) and non-functional (NFR) requirements, architecture, REST API, database schema, acceptance criteria, stages, risks and accepted decisions |

## Key decisions at a glance

- **Deposit addresses are not generated here.** The address model (CREATE2 forwarders, HD derivation, …) depends on the key storage type and lives in the private `signer` service; this repository only registers addresses (FR-20, FR-26, SIG-11).
- **Two independent RPC sources per network.** A deposit is finalized only when both agree on the block hash (FR-27).
- **Daily reconciliation** of the journal against on-chain balances (FR-40).
- **Private keys never enter this repository.** Signing goes through the `Signer` gRPC contract; `signer-local` covers development and CI on test networks only (section 7.1).
- **MVP network:** anvil for tests, Ethereum Sepolia with a mock USDC as the first real network.
