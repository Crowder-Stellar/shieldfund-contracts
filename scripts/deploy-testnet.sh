#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# ShieldFund — Soroban deploy script
#
# Prerequisites:
#   • Stellar CLI installed:  cargo install stellar-cli --features opt
#   • Rust + wasm32v1-none target:  rustup target add wasm32v1-none
#   • A funded account in the Stellar CLI keystore (e.g. `stellar keys generate admin --fund`)
#
# Usage:
#   ADMIN_ACCOUNT=<key name or G... address> ./scripts/deploy-testnet.sh
#
# Optional:
#   NETWORK=local USDC_TOKEN=<C... token id> ./scripts/deploy-testnet.sh
#   (NETWORK defaults to testnet; USDC_TOKEN defaults to Circle's testnet USDC SAC.)
#
# Each contract's admin is set by its constructor in the same transaction that
# deploys it, so there is never an uninitialised contract anyone could claim.
# The vault is then pointed at the registry so disburse() works immediately.
# ─────────────────────────────────────────────────────────────────────────────

set -euo pipefail

NETWORK="${NETWORK:-testnet}"

# USDC on Stellar testnet (Circle's deployed SAC)
USDC_TESTNET="CBIELTK6YBZJU5UP2WWQEQZMYJMZROFZKYPVCCNWY5TU4BOQ3EOWXPD"
USDC_TOKEN="${USDC_TOKEN:-$USDC_TESTNET}"

ADMIN="${ADMIN_ACCOUNT:?Set ADMIN_ACCOUNT to a Stellar CLI key name or G... address}"
ADMIN_ADDRESS=$(stellar keys address "$ADMIN" 2>/dev/null || echo "$ADMIN")

echo ""
echo "══════════════════════════════════════════════"
echo "  ShieldFund — Soroban deployment"
echo "══════════════════════════════════════════════"
echo "  Admin  : $ADMIN_ADDRESS"
echo "  Network: $NETWORK"
echo "  Token  : $USDC_TOKEN"
echo ""

# ── 1. Build all contracts ─────────────────────────────────────────────────

echo "▸ Building contracts (release WASM)…"
stellar contract build

WASM_DIR="target/wasm32v1-none/release"

deploy() {
  local wasm="$1"; shift
  stellar contract deploy \
    --wasm "$WASM_DIR/$wasm" \
    --source "$ADMIN" \
    --network "$NETWORK" \
    -- "$@"
}

# ── 2. Deploy (constructor sets admin atomically) ──────────────────────────

echo ""
echo "▸ Deploying proof_registry…"
REGISTRY_ID=$(deploy proof_registry.wasm --admin "$ADMIN_ADDRESS")
echo "  proof_registry : $REGISTRY_ID"

echo "▸ Deploying treasury_vault…"
VAULT_ID=$(deploy treasury_vault.wasm --admin "$ADMIN_ADDRESS" --usdc_token "$USDC_TOKEN")
echo "  treasury_vault : $VAULT_ID"

echo "▸ Deploying streaming…"
STREAMING_ID=$(deploy streaming.wasm --admin "$ADMIN_ADDRESS" --usdc_token "$USDC_TOKEN")
echo "  streaming      : $STREAMING_ID"

# ── 3. Wire the vault to the registry ──────────────────────────────────────

echo ""
echo "▸ Pointing treasury_vault at proof_registry…"
stellar contract invoke \
  --id "$VAULT_ID" \
  --source "$ADMIN" \
  --network "$NETWORK" \
  -- set_proof_registry \
  --registry "$REGISTRY_ID"

# ── 4. Sanity checks ───────────────────────────────────────────────────────

for id in "$REGISTRY_ID" "$VAULT_ID" "$STREAMING_ID"; do
  got=$(stellar contract invoke --id "$id" --source "$ADMIN" --network "$NETWORK" -- get_admin | tr -d '"')
  if [ "$got" != "$ADMIN_ADDRESS" ]; then
    echo "✗ $id reports admin $got, expected $ADMIN_ADDRESS" >&2
    exit 1
  fi
done
got=$(stellar contract invoke --id "$VAULT_ID" --source "$ADMIN" --network "$NETWORK" -- get_proof_registry | tr -d '"')
if [ "$got" != "$REGISTRY_ID" ]; then
  echo "✗ treasury_vault is wired to $got, expected $REGISTRY_ID" >&2
  exit 1
fi
echo "✓ admin set on all three contracts, vault wired to registry"

# ── 5. Print summary ───────────────────────────────────────────────────────

echo ""
echo "══════════════════════════════════════════════"
echo "  Deployment complete"
echo "══════════════════════════════════════════════"
echo ""
echo "  TREASURY_VAULT : $VAULT_ID"
echo "  STREAMING      : $STREAMING_ID"
echo "  PROOF_REGISTRY : $REGISTRY_ID"
echo "  USDC_SAC       : $USDC_TOKEN"
echo ""
echo "  → paste these into shieldfund-frontend src/lib/contracts.ts and the backend .env"
echo ""
