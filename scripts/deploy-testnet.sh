#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# ShieldFund — Soroban testnet deploy script
#
# Prerequisites:
#   • Stellar CLI installed:  cargo install stellar-cli --features opt
#   • Rust + wasm32 target:   rustup target add wasm32-unknown-unknown
#   • A funded testnet account in the Stellar CLI keystore
#
# Usage:
#   cd contracts/
#   chmod +x scripts/deploy-testnet.sh
#   ADMIN_ACCOUNT=<your-stellar-address> ./scripts/deploy-testnet.sh
#
# After successful deploy this script prints the contract IDs.
# Copy them into ../src/lib/contracts.ts → CONTRACT_IDS.TESTNET.
# ─────────────────────────────────────────────────────────────────────────────

set -euo pipefail

NETWORK="testnet"
RPC_URL="https://soroban-testnet.stellar.org"
NETWORK_PASSPHRASE="Test SDF Network ; September 2015"

# USDC on Stellar testnet (Circle's deployed SAC)
USDC_TESTNET="CBIELTK6YBZJU5UP2WWQEQZMYJMZROFZKYPVCCNWY5TU4BOQ3EOWXPD"

ADMIN="${ADMIN_ACCOUNT:?Set ADMIN_ACCOUNT env var to your Stellar testnet address}"

echo ""
echo "══════════════════════════════════════════════"
echo "  ShieldFund — Soroban testnet deployment"
echo "══════════════════════════════════════════════"
echo "  Admin :  $ADMIN"
echo "  Network: $NETWORK"
echo ""

# ── 1. Build all contracts ─────────────────────────────────────────────────

echo "▸ Building contracts (release WASM)…"
stellar contract build

WASM_DIR="target/wasm32-unknown-unknown/release"

# ── 2. Deploy Treasury Vault ───────────────────────────────────────────────

echo ""
echo "▸ Deploying treasury_vault…"
VAULT_ID=$(stellar contract deploy \
  --wasm "$WASM_DIR/treasury_vault.wasm" \
  --source "$ADMIN" \
  --network "$NETWORK" \
  --rpc-url "$RPC_URL" \
  --network-passphrase "$NETWORK_PASSPHRASE")

echo "  treasury_vault : $VAULT_ID"

echo "▸ Initializing treasury_vault…"
stellar contract invoke \
  --id "$VAULT_ID" \
  --source "$ADMIN" \
  --network "$NETWORK" \
  -- initialize \
  --admin "$ADMIN" \
  --usdc_token "$USDC_TESTNET"

# ── 3. Deploy Streaming ────────────────────────────────────────────────────

echo ""
echo "▸ Deploying streaming…"
STREAMING_ID=$(stellar contract deploy \
  --wasm "$WASM_DIR/streaming.wasm" \
  --source "$ADMIN" \
  --network "$NETWORK" \
  --rpc-url "$RPC_URL" \
  --network-passphrase "$NETWORK_PASSPHRASE")

echo "  streaming : $STREAMING_ID"

echo "▸ Initializing streaming…"
stellar contract invoke \
  --id "$STREAMING_ID" \
  --source "$ADMIN" \
  --network "$NETWORK" \
  -- initialize \
  --admin "$ADMIN" \
  --usdc_token "$USDC_TESTNET"

# ── 4. Deploy Proof Registry ───────────────────────────────────────────────

echo ""
echo "▸ Deploying proof_registry…"
REGISTRY_ID=$(stellar contract deploy \
  --wasm "$WASM_DIR/proof_registry.wasm" \
  --source "$ADMIN" \
  --network "$NETWORK" \
  --rpc-url "$RPC_URL" \
  --network-passphrase "$NETWORK_PASSPHRASE")

echo "  proof_registry : $REGISTRY_ID"

echo "▸ Initializing proof_registry…"
stellar contract invoke \
  --id "$REGISTRY_ID" \
  --source "$ADMIN" \
  --network "$NETWORK" \
  -- initialize \
  --admin "$ADMIN"

# ── 5. Print summary ───────────────────────────────────────────────────────

echo ""
echo "══════════════════════════════════════════════"
echo "  Deployment complete — copy into contracts.ts"
echo "══════════════════════════════════════════════"
echo ""
echo "  TREASURY_VAULT : $VAULT_ID"
echo "  STREAMING      : $STREAMING_ID"
echo "  PROOF_REGISTRY : $REGISTRY_ID"
echo "  USDC_SAC       : $USDC_TESTNET"
echo ""
echo "  → paste these into src/lib/contracts.ts → CONTRACT_IDS.TESTNET"
echo ""
