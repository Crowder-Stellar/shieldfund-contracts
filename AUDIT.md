# External review scope

This is the brief for the external security review of the ShieldFund contracts. **The review must be complete,
with all findings fixed or explicitly accepted, before any mainnet deployment.**

## In scope

| Contract | File | Purpose |
|---|---|---|
| `treasury_vault` | `treasury_vault/src/lib.rs` | Custodies USDC; pays out only against a registered, unspent proof bound to the exact recipient and amount |
| `streaming` | `streaming/src/lib.rs` | Per-second payment streams funded by direct token transfer |
| `proof_registry` | `proof_registry/src/lib.rs` | Admin-anchored store of ZK proof hashes and their public-inputs hashes |

The toolchain is `soroban-sdk` 22, built with `stellar contract build` (`wasm32v1-none`). Tests run with
`cargo test --workspace`.

Also worth reviewing, because the contracts depend on it: the public-inputs hashing that `treasury_vault`
recomputes must match `shieldfund-proof-server/src/hash.js` and the `payroll_compliance` circuit's
public-input order. Test vectors are in `treasury_vault` → `hashing_matches_proof_server`.

## Trust model

- **Each contract has a single admin key**, set in `__constructor` at deploy and moved only via
  `propose_admin` → `accept_admin`.
- **`proof_registry` does not verify proofs.** It trusts its admin, which is the backend's submitter key, to
  register only proofs that `bb verify` accepted off-chain. Anyone holding that key can authorise any payment
  whose public inputs they can compute.
- **`treasury_vault::disburse`** requires admin auth and a registered, unspent proof whose
  `public_inputs_hash` equals `keccak256(merkle_root ‖ budget_commitment ‖ recipient_field ‖ amount ‖ proof_type_id)`.
  It does not check `merkle_root` or `budget_commitment` against any on-chain allowlist or budget; the admin
  supplies them.
- **`streaming`** trusts its admin to fund streams and relies on the token SAC for transfers.

## Invariants we expect to hold

1. A proof hash pays out at most once (`SpentProof`), and a failed `disburse` never marks it spent.
2. `disburse` pays exactly the `amount` and `recipient` the proof's public inputs commit to.
3. `total_raised` and `total_disbursed` never overflow silently (checked; `Overflow` error).
4. A stream never accrues past `end_time` or for time spent paused, regardless of pause/resume order.
5. `withdraw` pays the recipient at most `flow_rate × (end_time − start_time)` over the stream's life.
6. Only the current admin can change the admin, and only the nominee can complete the change.
7. Every state change emits an event (see README → Events).
8. `get_committed()` never exceeds the token balance, so every open stream can be paid in full.

## Known issues and open questions

These are deliberately not fixed in this PR. They're listed so the reviewer can weigh in.

1. ~~**Streams can over-commit funds.**~~ Fixed: `create_stream` reserves each stream's total against
   `balance − committed`; payouts and completion release the reservation.
2. **No reclaim of unreserved funds.** Reservations for paused time are released when a stream completes,
   so they can fund new streams, but there is still no admin function to withdraw uncommitted tokens from
   `streaming`. Closing a stream that is paused through its end needs the recipient's `withdraw` (zero
   payout after `end_time`).
3. **No storage TTL management.** Persistent entries (proofs, streams, spent-proof markers, totals) are
   never `extend_ttl`'d. Archived entries need restoring before they can be read. An archived `SpentProof`
   marker is restored, not lost, so it can't be bypassed, but this should be confirmed.
4. **Single-key admin, no pause or upgrade path.** See tasks 13 (multisig) and 21 (pause).
5. **Off-chain trust in `proof_registry`.** See tasks 9 (on-chain verifier spike) and 10 (m-of-n attestation).
6. **`get_all_proofs` / `get_all_streams` are unbounded.** They're kept only for current clients; use the
   paginated reads instead.

## Deployment status

The contracts currently on testnet (IDs in `README.md`) **predate** the admin-only `register_proof` check,
typed errors, two-step admin transfer, and pagination. A new testnet deploy is blocked on the circuit fixes
(Austin #17, #19, #20). After it, the contract IDs need updating in the backend, the frontend, and the
proof-server README.
