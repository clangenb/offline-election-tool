# Offline Election Tool

Rust CLI/server for simulating Substrate-based blockchain (Polkadot/Kusama) NPoS elections offline. Predicts validator sets by running election algorithms (seq-phragmen, phragmms) against on-chain snapshot data fetched via RPC.

## Build & Test

```bash
cargo build              # debug build
cargo build --release    # release build
cargo test --all-features # run all tests (CI command)
```

Compilation is slow due to heavy polkadot-sdk dependencies. Use `cargo check` for quick validation. The `[patch.crates-io]` section in Cargo.toml points all substrate crates to `clangenb/polkadot-sdk` branch `cl/patch-npos`.

## Architecture

```
src/
├── main.rs                      # CLI (clap): simulate, snapshot, process-results, server
├── models.rs                    # Core types: Chain, Algorithm, Validator, Snapshot, SimulationResult
├── primitives.rs                # Type aliases: AccountId, Balance, Storage, Hash
├── simulate.rs                  # Election simulation pipeline + Override struct
├── snapshot.rs                  # Snapshot building from chain state
├── miner_config.rs              # Runtime election config, algorithm dispatch (DynamicSolver)
├── subxt_client.rs              # Subxt RPC client with reconnection
├── raw_state_client.rs          # Low-level jsonrpsee RPC queries (storage, validators, bags)
├── multi_block_state_client.rs  # Multi-block election state abstraction (phases, pages)
└── api/
    ├── routes/root.rs           # Axum router: POST /simulate, GET /snapshot
    ├── handler/simulate.rs      # Simulate endpoint handler
    ├── handler/snapshot.rs      # Snapshot endpoint handler
    └── utils.rs                 # Block hash parsing
```

## Key Data Flow

RPC connection → fetch block details & election phase → build snapshot (on-chain or derived from staking storage) → apply overrides (add/remove candidates/voters) → run election algorithm → check feasibility → extract validator details → format output as JSON.

## Chain Support

- **Polkadot**: SS58 prefix 0, 10 decimals (DOT), NposSolution16
- **Kusama**: SS58 prefix 2, 12 decimals (KSM), NposSolution24
- **Substrate**: SS58 prefix 42, NposSolution16

Chain is auto-detected from runtime version spec_name.

## Configuration

- `override.json` — manual election overrides (candidates, voters, vote removal); tracked in git
- `configs/` — chain-specific override and post-process configs; gitignored
- `results/` — simulation output; gitignored

Override schema (all fields optional; omitted fields default to empty):
```json
{
  "candidates": ["addr"],
  "candidates_remove": ["addr"],
  "voters": [["addr", stake_u64, ["target_addr"]]],
  "voters_remove": ["addr"],
  "voters_remove_vote": [["voter_addr", ["target_addr"]]],
  "self_bond": [["validator_addr", amount_planck_u64]]
}
```

`self_bond` simulates a validator having added self-bond: each entry re-adds the
validator as a candidate (surviving the `min_validator_bond` filter, which is
applied before overrides using on-chain `ledger.active`) and injects a self-vote
of `amount`. Used to model the upcoming 10k DOT minimum self-bond.

## Testing

Tests use `mockall` for trait mocking (`MockRpcClient`, `MockChainClientTrait`, `MockMultiBlockClientTrait`). Test modules are co-located in each source file under `#[cfg(test)]`. API tests use `axum-test`.

## CI/CD

- **rust_main_ci.yml**: `cargo test --all-features` on push/PR to main (src/ or Cargo.* changes)
- **push_docker.yml**: multi-arch Docker build (amd64, arm64) pushed to `bilinearlabs/offline-election-tool:<short-hash>` on main push

## Docker

```bash
docker build -t offline-election-tool .          # local build
docker run -p 3000:3000 offline-election-tool \
  --rpc-endpoint wss://... server --address 0.0.0.0:3000
```

Multi-stage Dockerfile: cargo-chef for dependency caching, stripped release binary, debian-slim runtime, non-root user.

## Conventions

- Async Rust with tokio multi-thread runtime
- Trait-based service abstraction (`SimulateService`, `SnapshotService`, `ChainClientTrait`, etc.)
- Task-local storage for per-request election config (server mode); global mutex fallback (CLI mode)
- Stakes formatted as human-readable strings in output types (`ValidatorOutput`, `SimulationResultOutput`)
- Validator slots are 1-indexed in output
- All chain constants fetched at startup, not hardcoded
