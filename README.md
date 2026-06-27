# ShieldFund Contracts

Soroban smart contracts for the ShieldFund ZK treasury platform, deployed on the Stellar blockchain. Three contracts form the on-chain layer: a treasury vault for USDC custody, a real-time payment streaming engine, and a ZK proof registry.

---

## Contracts

### `treasury_vault`

Multi-sig treasury that holds USDC and gates disbursements behind ZK proof verification.

| Function | Auth | Description |
|----------|------|-------------|
| `initialize(admin, usdc_token)` | — | One-time setup. Panics if called again. |
| `deposit(depositor, amount)` | `depositor` | Transfer USDC into the vault. Amount is in stroops (1 USDC = 10,000,000). |
| `disburse(recipient, amount, proof_hash)` | `admin` | Pay USDC to a recipient. `proof_hash` anchors the ZK justification on-chain. |
| `get_balance()` | — | Live USDC balance held by the contract. |
| `get_stats()` | — | Returns `{ vault_balance, total_raised, total_disbursed }`. |
| `get_admin()` | — | Current admin address. |
| `get_token()` | — | USDC token SAC address. |
| `transfer_admin(new_admin)` | `admin` | Hand off admin rights. |

---

### `streaming`

Real-time payment streams. Recipients accumulate USDC per second without any cron job — the live balance is computed on-chain from `accumulated + elapsed * flow_rate_per_second`.

| Function | Auth | Description |
|----------|------|-------------|
| `initialize(admin, usdc_token)` | — | One-time setup. |
| `create_stream(recipient, flow_rate_per_second, end_time)` | `admin` | Create a stream. Fund the contract first — it validates you hold enough USDC. Returns `stream_id`. |
| `toggle_stream(stream_id)` | `admin` | Pause ↔ Active. Snapshots `accumulated` on pause so no time is lost. |
| `get_accumulated(stream_id)` | — | Live claimable balance (simulation call — no fee, no state change). |
| `withdraw(stream_id)` | `recipient` | Recipient claims their accumulated USDC. Marks stream Completed if past `end_time`. |
| `get_stream(stream_id)` | — | Returns full `Stream` record. |
| `get_all_streams()` | — | Returns all streams. |
| `get_admin()` | — | Current admin address. |

**Flow rate conversion:**
```
flow_rate_per_second = ceil(monthly_usdc_stroops / 2_592_000)
# Example: 5,000 USDC/month
flow_rate_per_second = ceil(50_000_000_000 / 2_592_000) = 19_291
```

---

### `proof_registry`

On-chain ZK proof store. Every registered proof gets a sequential ID, a hash index for O(1) lookups, and a permanent ledger timestamp.

| Function | Auth | Description |
|----------|------|-------------|
| `initialize(admin)` | — | One-time setup. |
| `register_proof(submitter, proof_hash, public_inputs_hash, proof_type)` | `submitter` | Anchor a Noir proof on-chain. `proof_type` is one of `payroll`, `operational`, `relief`. Panics on duplicate hash. Returns `proof_id`. |
| `get_proof(id)` | — | Returns `ProofEntry` by sequential ID. |
| `get_all_proofs()` | — | Returns all registered proofs. |
| `verify_proof_exists(proof_hash)` | — | Returns `bool` — cheap existence check by hash. |
| `get_id_by_hash(proof_hash)` | — | Returns the proof ID for a given hash. |
| `get_admin()` | — | Current admin. |
| `transfer_admin(new_admin)` | `admin` | Transfer admin rights. |

---

## Prerequisites

| Tool | Install |
|------|---------|
| Rust + Cargo | `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \| sh` |
| wasm32 target | `rustup target add wasm32-unknown-unknown` |
| Stellar CLI | `cargo install stellar-cli --features opt` |
| Funded testnet account | `stellar keys generate --global mykey --network testnet` then fund via [Friendbot](https://friendbot.stellar.org) |

---

## Build

```bash
# From the repo root
stellar contract build

# WASM files are written to:
# target/wasm32-unknown-unknown/release/treasury_vault.wasm
# target/wasm32-unknown-unknown/release/streaming.wasm
# target/wasm32-unknown-unknown/release/proof_registry.wasm
```

---

## Test

```bash
cargo test
```

Tests run inside the Soroban test environment (no live network needed). The `streaming` and `proof_registry` contracts have embedded `#[cfg(test)]` suites covering:
- Accumulated balance accrual math
- Paused stream snapshots
- Duplicate proof registration guards
- Proof count increments

---

## Deploy to Testnet

```bash
cd contracts/

# Set your admin address
export ADMIN_ACCOUNT=G...YOUR_STELLAR_ADDRESS...

# Make the script executable and run it
chmod +x scripts/deploy-testnet.sh
./scripts/deploy-testnet.sh
```

The script will:
1. Build all three WASM contracts
2. Deploy each to Stellar testnet via the Soroban RPC
3. Call `initialize` on each contract
4. Print all three contract IDs

**Copy the printed IDs into `src/lib/contracts.ts` in [shieldfund-frontend](https://github.com/Crowder-Stellar/shieldfund-frontend).**

---

## Interact via Stellar CLI

After deploying, you can invoke contract functions directly from the terminal:

```bash
export NETWORK=testnet
export VAULT_ID=C...       # from deploy output
export STREAMING_ID=C...
export REGISTRY_ID=C...
export ADMIN=G...          # your key alias registered with stellar-cli

# Check vault stats
stellar contract invoke --id $VAULT_ID --network $NETWORK -- get_stats

# Deposit 10 USDC (= 100_000_000 stroops)
stellar contract invoke \
  --id $VAULT_ID \
  --source $ADMIN \
  --network $NETWORK \
  -- deposit \
  --depositor $ADMIN \
  --amount 100000000

# Disburse 5 USDC with a proof hash anchor
stellar contract invoke \
  --id $VAULT_ID \
  --source $ADMIN \
  --network $NETWORK \
  -- disburse \
  --recipient G...RECIPIENT... \
  --amount 50000000 \
  --proof_hash 0000000000000000000000000000000000000000000000000000000000000000

# Create a stream: 1929 stroops/sec ≈ 5000 USDC/month, ending in 30 days
stellar contract invoke \
  --id $STREAMING_ID \
  --source $ADMIN \
  --network $NETWORK \
  -- create_stream \
  --recipient G...RECIPIENT... \
  --flow_rate_per_second 1929 \
  --end_time $(($(date +%s) + 2592000))

# Check claimable balance for stream 0
stellar contract invoke \
  --id $STREAMING_ID \
  --network $NETWORK \
  -- get_accumulated \
  --stream_id 0

# Register a ZK proof
stellar contract invoke \
  --id $REGISTRY_ID \
  --source $ADMIN \
  --network $NETWORK \
  -- register_proof \
  --submitter $ADMIN \
  --proof_hash abcdef0000000000000000000000000000000000000000000000000000000000 \
  --public_inputs_hash 1234560000000000000000000000000000000000000000000000000000000000 \
  --proof_type payroll

# Verify a proof exists by hash
stellar contract invoke \
  --id $REGISTRY_ID \
  --network $NETWORK \
  -- verify_proof_exists \
  --proof_hash abcdef0000000000000000000000000000000000000000000000000000000000
```

---

## Project Structure

```
shieldfund-contracts/
├── Cargo.toml                          # Workspace manifest — all members share soroban-sdk dep
├── Cargo.lock                          # Pinned dependency tree
├── .gitignore                          # Excludes target/ and .stellar/
│
├── treasury_vault/
│   ├── Cargo.toml
│   └── src/lib.rs                      # deposit, disburse, get_stats, transfer_admin
│
├── streaming/
│   ├── Cargo.toml
│   └── src/lib.rs                      # create_stream, toggle_stream, withdraw, get_accumulated
│
├── proof_registry/
│   ├── Cargo.toml
│   └── src/lib.rs                      # register_proof, verify_proof_exists, get_all_proofs
│
└── scripts/
    └── deploy-testnet.sh               # One-shot build + deploy + initialize for all 3 contracts
```

---

## Related Repos

- [shieldfund-frontend](https://github.com/Crowder-Stellar/shieldfund-frontend) — React dashboard (paste contract IDs here after deploy)
- [shieldfund-backend](https://github.com/Crowder-Stellar/shieldfund-backend) — Express API for off-chain indexing
