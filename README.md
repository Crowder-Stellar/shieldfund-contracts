# ShieldFund Contracts

![CI](https://github.com/Crowder-Stellar/shieldfund-contracts/actions/workflows/ci.yml/badge.svg)
![Stellar](https://img.shields.io/badge/Stellar-Testnet-blue?logo=stellar)
![Rust](https://img.shields.io/badge/Rust-1.96-orange?logo=rust)
![Soroban SDK](https://img.shields.io/badge/Soroban%20SDK-22-blueviolet)

Soroban smart contracts for the ShieldFund ZK treasury platform on Stellar. Three contracts — a treasury vault for token custody, a real-time payment streaming engine, and an on-chain ZK proof registry — form the complete on-chain layer.

---

## Live Testnet Deployment

All three contracts are deployed and initialized on **Stellar Testnet** (deployed 2026-06-28).

| Contract | ID | Explorer |
|----------|----|---------|
| `treasury_vault` | `CAUWJPC73YLQMSV6X4QPLUVS2UZFE2PMRIQSSCDN62DNN6J76Y5RETIG` | [View →](https://stellar.expert/explorer/testnet/contract/CAUWJPC73YLQMSV6X4QPLUVS2UZFE2PMRIQSSCDN62DNN6J76Y5RETIG) |
| `streaming` | `CDU7ZIVQ3UC4K3DHV3NMQGW5UMSYFCKCC6YJKHT4YLNEZJRWL6THE6WQ` | [View →](https://stellar.expert/explorer/testnet/contract/CDU7ZIVQ3UC4K3DHV3NMQGW5UMSYFCKCC6YJKHT4YLNEZJRWL6THE6WQ) |
| `proof_registry` | `CBDLHQQPKC5524CFWPD4HMPTZGWBYQNW3IKGAFH6IAYBU3F2F6AO2332` | [View →](https://stellar.expert/explorer/testnet/contract/CBDLHQQPKC5524CFWPD4HMPTZGWBYQNW3IKGAFH6IAYBU3F2F6AO2332) |
| XLM Token SAC | `CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC` | [View →](https://stellar.expert/explorer/testnet/contract/CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC) |
| Admin Account | `GBJ5FP5UB4YUE2EONTPPSAGKZZGDETFZLEJXJRCALSYTJZIDVWAN3C7P` | [View →](https://stellar.expert/explorer/testnet/account/GBJ5FP5UB4YUE2EONTPPSAGKZZGDETFZLEJXJRCALSYTJZIDVWAN3C7P) |

---

## Work Breakdown Structure

```
shieldfund-contracts (Cargo workspace)
│
├── treasury_vault          ← Core custody contract
│   ├── initialize()        one-time setup, sets admin + token SAC
│   ├── deposit()           user → vault token transfer (auth: depositor)
│   ├── disburse()          vault → recipient + ZK proof anchor (auth: admin)
│   ├── get_balance()       read-only: live vault token balance
│   ├── get_stats()         read-only: { vault_balance, total_raised, total_disbursed }
│   ├── get_admin()
│   ├── get_token()
│   └── transfer_admin()    (auth: current admin)
│
├── streaming               ← Real-time payment engine
│   ├── initialize()        sets admin + token SAC
│   ├── create_stream()     creates Stream record (auth: admin)
│   │                       validates contract holds enough tokens first
│   ├── toggle_stream()     Active ↔ Paused, snapshots accumulated (auth: admin)
│   ├── get_accumulated()   simulation call: accumulated + elapsed × rate
│   ├── withdraw()          recipient claims tokens (auth: recipient)
│   ├── get_stream()        read single Stream record
│   ├── get_all_streams()   read all Stream records
│   └── get_admin()
│
└── proof_registry          ← ZK proof anchor store
    ├── initialize()        sets admin
    ├── register_proof()    stores ProofEntry + hash index (auth: submitter)
    │                       panics on duplicate hash
    ├── get_proof()         read by sequential ID
    ├── get_all_proofs()    read all ProofEntry records
    ├── verify_proof_exists() bool check by hash (O(1))
    ├── get_id_by_hash()    reverse lookup: hash → id
    ├── get_admin()
    └── transfer_admin()    (auth: current admin)
```

### How the contracts interact

```
User (Freighter wallet)
    │
    ├─ deposit() ──────────────────► treasury_vault
    │                                     │ holds tokens
    └─ register_proof() ──────────► proof_registry
                                          │ proof_hash stored
                                          │
treasury_vault::disburse(proof_hash) ─────┘ (admin links payment to proof)
    │
    └─► transfers tokens to recipient

streaming contract
    │ admin funds it with a token transfer
    └─► create_stream() → per-second drip to recipient
        withdraw()      → recipient claims accumulated tokens
```

---

## Contracts Reference

### `treasury_vault`

| Function | Auth | Parameters | Returns |
|----------|------|-----------|---------|
| `initialize` | — | `admin: Address`, `usdc_token: Address` | — |
| `deposit` | `depositor` | `depositor: Address`, `amount: i128` | — |
| `disburse` | `admin` | `recipient: Address`, `amount: i128`, `proof_hash: BytesN<32>` | — |
| `get_balance` | — | — | `i128` |
| `get_stats` | — | — | `VaultStats` |
| `get_admin` | — | — | `Address` |
| `get_token` | — | — | `Address` |
| `transfer_admin` | `admin` | `new_admin: Address` | — |

`amount` is always in **stroops** (7 decimal places). 1 XLM = 10,000,000 stroops.

### `streaming`

| Function | Auth | Parameters | Returns |
|----------|------|-----------|---------|
| `initialize` | — | `admin: Address`, `usdc_token: Address` | — |
| `create_stream` | `admin` | `recipient: Address`, `flow_rate_per_second: i128`, `end_time: u64` | `u32` (stream ID) |
| `toggle_stream` | `admin` | `stream_id: u32` | `StreamStatus` |
| `get_accumulated` | — | `stream_id: u32` | `i128` |
| `withdraw` | `recipient` | `stream_id: u32` | `i128` (amount paid) |
| `get_stream` | — | `stream_id: u32` | `Stream` |
| `get_all_streams` | — | — | `Vec<Stream>` |
| `get_admin` | — | — | `Address` |

**Flow rate formula:** `flow_rate_per_second = ceil(monthly_amount_stroops / 2_592_000)`

Example — 5,000 XLM per month:
```
flow_rate = ceil(50_000_000_000 / 2_592_000) = 19_291 stroops/sec
```

### `proof_registry`

| Function | Auth | Parameters | Returns |
|----------|------|-----------|---------|
| `initialize` | — | `admin: Address` | — |
| `register_proof` | `submitter` | `submitter: Address`, `proof_hash: BytesN<32>`, `public_inputs_hash: BytesN<32>`, `proof_type: Symbol` | `u32` (proof ID) |
| `get_proof` | — | `id: u32` | `ProofEntry` |
| `get_all_proofs` | — | — | `Vec<ProofEntry>` |
| `verify_proof_exists` | — | `proof_hash: BytesN<32>` | `bool` |
| `get_id_by_hash` | — | `proof_hash: BytesN<32>` | `u32` |
| `get_admin` | — | — | `Address` |
| `transfer_admin` | `admin` | `new_admin: Address` | — |

`proof_type` is a Soroban `Symbol` (max 9 chars): `"payroll"`, `"operational"`, `"relief"`.

---

## Prerequisites

| Tool | Install |
|------|---------|
| Rust (stable) | `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \| sh` |
| wasm32v1-none target | `rustup target add wasm32v1-none` |
| Stellar CLI | Download binary from [releases](https://github.com/stellar/stellar-cli/releases/latest) or `cargo install stellar-cli` |

---

## Build

```bash
# From repo root
stellar contract build

# WASM output:
# target/wasm32v1-none/release/treasury_vault.wasm
# target/wasm32v1-none/release/streaming.wasm
# target/wasm32v1-none/release/proof_registry.wasm
```

---

## Test

```bash
cargo test --workspace
```

Tests run in the Soroban in-process environment — no live network, no XLM needed. Covers:

- `streaming`: accumulated balance accrual math, paused stream snapshot, flow-rate ceiling division
- `proof_registry`: register & verify, duplicate hash panic, proof count increment

---

## Deploy Your Own Testnet Instance

```bash
# 1. Generate a new key
stellar keys generate my-admin --network testnet

# 2. Get the address
stellar keys address my-admin

# 3. Fund via Friendbot
curl "https://friendbot.stellar.org?addr=$(stellar keys address my-admin)"

# 4. Register your token as a CLI alias (use native XLM SAC or your own token)
stellar contract id asset --asset native --network testnet
stellar contract alias add xlm_sac --id <XLM_SAC_ID> --network testnet

# 5. Deploy & initialize all 3 contracts
export ADMIN_ACCOUNT=$(stellar keys address my-admin)
chmod +x scripts/deploy-testnet.sh
./scripts/deploy-testnet.sh

# 6. Copy the printed IDs into shieldfund-frontend/src/lib/contracts.ts
```

---

## Interact via Stellar CLI

Using the live testnet contracts:

```bash
VAULT=CAUWJPC73YLQMSV6X4QPLUVS2UZFE2PMRIQSSCDN62DNN6J76Y5RETIG
STREAM=CDU7ZIVQ3UC4K3DHV3NMQGW5UMSYFCKCC6YJKHT4YLNEZJRWL6THE6WQ
REGISTRY=CBDLHQQPKC5524CFWPD4HMPTZGWBYQNW3IKGAFH6IAYBU3F2F6AO2332

# Read vault stats (free simulation — no fee, no signing)
stellar contract invoke --id $VAULT --source my-admin --network testnet \
  -- get_stats

# Deposit 10 XLM (100,000,000 stroops)
stellar contract invoke --id $VAULT --source my-admin --network testnet \
  -- deposit \
  --depositor $(stellar keys address my-admin) \
  --amount 100000000

# Create a stream: ~1 XLM/hour = 2778 stroops/sec, runs for 7 days
stellar contract invoke --id $STREAM --source my-admin --network testnet \
  -- create_stream \
  --recipient G...RECIPIENT_ADDRESS... \
  --flow_rate_per_second 2778 \
  --end_time $(($(date +%s) + 604800))

# Check live claimable balance for stream 0
stellar contract invoke --id $STREAM --source my-admin --network testnet \
  -- get_accumulated --stream_id 0

# Register a ZK proof
stellar contract invoke --id $REGISTRY --source my-admin --network testnet \
  -- register_proof \
  --submitter $(stellar keys address my-admin) \
  --proof_hash abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890 \
  --public_inputs_hash 1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef \
  --proof_type payroll

# Check if a proof hash exists
stellar contract invoke --id $REGISTRY --source my-admin --network testnet \
  -- verify_proof_exists \
  --proof_hash abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890
```

---

## Project Structure

```
shieldfund-contracts/
├── Cargo.toml                     # Workspace — all members share soroban-sdk = 22
├── Cargo.lock
├── .gitignore                     # Excludes target/, .stellar/, *.wasm
│
├── treasury_vault/
│   ├── Cargo.toml
│   └── src/lib.rs                 # deposit, disburse, get_stats, transfer_admin
│
├── streaming/
│   ├── Cargo.toml
│   └── src/lib.rs                 # create_stream, toggle_stream, withdraw, get_accumulated
│                                  # includes #[cfg(test)] suite
│
├── proof_registry/
│   ├── Cargo.toml
│   └── src/lib.rs                 # register_proof, verify_proof_exists, get_all_proofs
│                                  # includes #[cfg(test)] suite
│
└── scripts/
    └── deploy-testnet.sh          # Build + deploy + initialize all 3 contracts in one shot
```

---

## CI

GitHub Actions on every push and PR:
- **`cargo test --workspace`** — runs the in-process test suites
- **`stellar contract build`** — compiles all 3 WASMs (push to main only)
- WASM artifacts uploaded for 7 days after each successful build

---

## Related Repos

- [shieldfund-frontend](https://github.com/Crowder-Stellar/shieldfund-frontend) — React dashboard (paste contract IDs here)
- [shieldfund-backend](https://github.com/Crowder-Stellar/shieldfund-backend) — Express API for off-chain indexing
