#![no_std]
use soroban_sdk::{
    contract, contractimpl, contracttype, symbol_short, vec,
    token, Address, Bytes, BytesN, Env, IntoVal, Symbol, Val, U256,
};

// ── Storage keys ──────────────────────────────────────────────────────────────

#[contracttype]
enum DataKey {
    Admin,
    PendingAdmin,
    Token,
    TotalRaised,
    TotalDisbursed,
    ProofRegistry,
    // A proof_hash that has already paid out — each proof is single-use.
    SpentProof(BytesN<32>),
}

// ── Storage TTL ──────────────────────────────────────────────────────────────
//
// Soroban archives entries whose TTL runs out. Instance storage (admin, token,
// wiring) is bumped on every call; persistent entries are bumped whenever they
// are written or used, and anyone can bump them explicitly via `extend_ttl`.

const DAY_IN_LEDGERS: u32 = 17_280; // ~5s ledgers
const TTL_EXTEND_TO: u32 = 30 * DAY_IN_LEDGERS;
const TTL_THRESHOLD: u32 = TTL_EXTEND_TO - DAY_IN_LEDGERS;

fn bump_instance(env: &Env) {
    env.storage().instance().extend_ttl(TTL_THRESHOLD, TTL_EXTEND_TO);
}

fn bump<K: IntoVal<Env, Val>>(env: &Env, key: &K) {
    env.storage().persistent().extend_ttl(key, TTL_THRESHOLD, TTL_EXTEND_TO);
}

/// Mirror of proof_registry's `ProofEntry`, decoded from the cross-contract
/// call in `disburse`. Field names and types must match exactly.
#[contracttype]
#[derive(Clone, Debug)]
pub struct ProofEntry {
    pub id: u32,
    pub proof_hash: BytesN<32>,
    pub public_inputs_hash: BytesN<32>,
    pub proof_type: Symbol,
    pub timestamp: u64,
    pub submitter: Address,
}

/// BN254 scalar field modulus (big-endian). Every public input of the
/// payroll_compliance circuit is an element of this field.
const FIELD_MODULUS: [u8; 32] = [
    0x30, 0x64, 0x4e, 0x72, 0xe1, 0x31, 0xa0, 0x29, 0xb8, 0x50, 0x45, 0xb6, 0x81, 0x81, 0x58, 0x5d,
    0x28, 0x33, 0xe8, 0x48, 0x79, 0xb9, 0x70, 0x91, 0x43, 0xe1, 0xf5, 0x93, 0xf0, 0x00, 0x00, 0x01,
];

/// Maps a proof_registry `proof_type` Symbol to the circuit's
/// `proof_type_id` (0 = payroll, 1 = operational, 2 = relief).
fn proof_type_id(env: &Env, proof_type: &Symbol) -> u32 {
    if *proof_type == Symbol::new(env, "payroll") {
        0
    } else if *proof_type == Symbol::new(env, "operational") {
        1
    } else if *proof_type == Symbol::new(env, "relief") {
        2
    } else {
        panic!("unknown proof_type")
    }
}

/// The circuit's `recipient_id` for a Stellar address: keccak256 of the
/// strkey's UTF-8 bytes, reduced into the field. Matches
/// `addressToField()` in shieldfund-proof-server/src/hash.js.
fn recipient_field(env: &Env, recipient: &Address) -> BytesN<32> {
    let strkey = recipient.to_string();
    let mut buf = [0u8; 56];
    assert!(strkey.len() as usize == buf.len(), "unexpected address length");
    strkey.copy_into_slice(&mut buf);
    let digest: Bytes = env.crypto().keccak256(&Bytes::from_slice(env, &buf)).into();
    let modulus = U256::from_be_bytes(env, &Bytes::from_array(env, &FIELD_MODULUS));
    field_bytes(env, &U256::from_be_bytes(env, &digest).rem_euclid(&modulus))
}

fn field_bytes(env: &Env, value: &U256) -> BytesN<32> {
    let raw = value.to_be_bytes();
    let mut out = [0u8; 32];
    let offset = 32 - raw.len() as usize;
    for (i, b) in raw.iter().enumerate() {
        out[offset + i] = b;
    }
    BytesN::from_array(env, &out)
}

/// keccak256 over the circuit's public inputs in declaration order, each as
/// a 32-byte big-endian field element. Matches `hashPublicInputs()` in
/// shieldfund-proof-server/src/hash.js, so it equals the `public_inputs_hash`
/// the proof server returns for a proof of exactly this payment.
fn public_inputs_hash(
    env: &Env,
    merkle_root: &BytesN<32>,
    budget_commitment: &BytesN<32>,
    recipient: &Address,
    amount: i128,
    proof_type_id: u32,
) -> BytesN<32> {
    let mut data = Bytes::new(env);
    data.append(&merkle_root.clone().into());
    data.append(&budget_commitment.clone().into());
    data.append(&recipient_field(env, recipient).into());
    data.append(&field_bytes(env, &U256::from_u128(env, amount as u128)).into());
    data.append(&field_bytes(env, &U256::from_u32(env, proof_type_id)).into());
    env.crypto().keccak256(&data).into()
}

// ── Public types (returned to callers) ───────────────────────────────────────

#[contracttype]
#[derive(Clone, Debug)]
pub struct VaultStats {
    /// Live USDC balance held by this contract (query from token SAC).
    pub vault_balance: i128,
    /// Cumulative USDC deposited since deployment.
    pub total_raised: i128,
    /// Cumulative USDC disbursed since deployment.
    pub total_disbursed: i128,
}

// ── Contract ─────────────────────────────────────────────────────────────────

#[contract]
pub struct TreasuryVaultContract;

#[contractimpl]
impl TreasuryVaultContract {
    /// Runs once, atomically with deployment, so there is no window in which
    /// someone else could claim the admin role on an uninitialised contract.
    pub fn __constructor(env: Env, admin: Address, usdc_token: Address) {
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Token, &usdc_token);
        env.storage().persistent().set(&DataKey::TotalRaised, &0i128);
        env.storage().persistent().set(&DataKey::TotalDisbursed, &0i128);
        bump_instance(&env);
        bump(&env, &DataKey::TotalRaised);
        bump(&env, &DataKey::TotalDisbursed);
        env.events().publish((symbol_short!("init"), admin), usdc_token);
    }

    /// Deposit USDC into the vault.
    ///
    /// The depositor must authorise this invocation (Freighter will prompt the
    /// user). Soroban's auth model propagates that authorisation into the
    /// sub-call to `token.transfer`, so no separate `approve` is needed.
    ///
    /// `amount` is in token stroops (7 decimal places: 1 USDC = 10_000_000).
    pub fn deposit(env: Env, depositor: Address, amount: i128) {
        depositor.require_auth();
        bump_instance(&env);
        assert!(amount > 0, "amount must be positive");

        let token_addr: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        let token_client = token::Client::new(&env, &token_addr);

        // Transfer USDC from depositor → this contract.
        // Because `depositor.require_auth()` was called above, Soroban's auth
        // framework authorises the sub-invocation automatically when the
        // transaction's auth entries are built by `server.prepareTransaction`.
        token_client.transfer(&depositor, &env.current_contract_address(), &amount);

        let raised: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::TotalRaised)
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&DataKey::TotalRaised, &(raised + amount));
        bump(&env, &DataKey::TotalRaised);

        env.events()
            .publish((symbol_short!("deposit"), depositor), amount);
    }

    /// Admin-only: set (or update) the ProofRegistry contract this vault
    /// checks disbursements against. Must be called before `disburse` will
    /// accept anything, since a freshly-deployed/initialized vault has no
    /// registry configured.
    pub fn set_proof_registry(env: Env, registry: Address) {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
        bump_instance(&env);
        env.storage().instance().set(&DataKey::ProofRegistry, &registry);
        env.events().publish((symbol_short!("set_reg"), admin), registry);
    }

    pub fn get_proof_registry(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::ProofRegistry)
            .expect("proof registry not configured")
    }

    /// Admin-only: disburse USDC to a recipient.
    ///
    /// `proof_hash` must be registered in ProofRegistry, and the proof must be
    /// *for this exact payment*: its registered `public_inputs_hash` has to
    /// equal the hash recomputed here from `merkle_root`, `budget_commitment`,
    /// `recipient`, `amount` and the registered proof type. Each proof pays
    /// out at most once.
    ///
    /// `amount` is in token stroops and must be the same value the proof was
    /// generated for. `merkle_root` / `budget_commitment` are returned by the
    /// proof server alongside `proof_hash`.
    pub fn disburse(
        env: Env,
        recipient: Address,
        amount: i128,
        proof_hash: BytesN<32>,
        merkle_root: BytesN<32>,
        budget_commitment: BytesN<32>,
    ) {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
        bump_instance(&env);
        assert!(amount > 0, "amount must be positive");

        let registry: Address = env
            .storage()
            .instance()
            .get(&DataKey::ProofRegistry)
            .expect("proof registry not configured");
        let proof_exists: bool = env.invoke_contract(
            &registry,
            &Symbol::new(&env, "verify_proof_exists"),
            vec![&env, proof_hash.into_val(&env)],
        );
        assert!(proof_exists, "proof_hash not registered in proof_registry");

        let spent_key = DataKey::SpentProof(proof_hash.clone());
        assert!(
            !env.storage().persistent().has(&spent_key),
            "proof already used for a disbursement"
        );

        let entry: ProofEntry = env.invoke_contract(
            &registry,
            &Symbol::new(&env, "get_proof_by_hash"),
            vec![&env, proof_hash.into_val(&env)],
        );
        let expected = public_inputs_hash(
            &env,
            &merkle_root,
            &budget_commitment,
            &recipient,
            amount,
            proof_type_id(&env, &entry.proof_type),
        );
        assert!(
            entry.public_inputs_hash == expected,
            "proof does not match this recipient and amount"
        );
        env.storage().persistent().set(&spent_key, &true);
        bump(&env, &spent_key);

        let token_addr: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        let token_client = token::Client::new(&env, &token_addr);

        // Verify the vault can cover this disbursement.
        let balance = token_client.balance(&env.current_contract_address());
        assert!(balance >= amount, "insufficient vault balance");

        // The vault contract is the owner of its own USDC balance, so no
        // extra auth entry is required for this transfer.
        token_client.transfer(&env.current_contract_address(), &recipient, &amount);

        let disbursed: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::TotalDisbursed)
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&DataKey::TotalDisbursed, &(disbursed + amount));
        bump(&env, &DataKey::TotalDisbursed);

        env.events()
            .publish((symbol_short!("disburse"), recipient), (amount, proof_hash));
    }

    /// Returns the live USDC balance held by the vault (reads from token SAC).
    pub fn get_balance(env: Env) -> i128 {
        let token_addr: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        token::Client::new(&env, &token_addr).balance(&env.current_contract_address())
    }

    /// Returns aggregated vault statistics in a single read.
    pub fn get_stats(env: Env) -> VaultStats {
        let token_addr: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        VaultStats {
            vault_balance: token::Client::new(&env, &token_addr)
                .balance(&env.current_contract_address()),
            total_raised: env
                .storage()
                .persistent()
                .get(&DataKey::TotalRaised)
                .unwrap_or(0),
            total_disbursed: env
                .storage()
                .persistent()
                .get(&DataKey::TotalDisbursed)
                .unwrap_or(0),
        }
    }

    /// Keeps the vault instance and its totals alive. Anyone may call this; it
    /// only extends TTLs, it changes no data.
    pub fn extend_ttl(env: Env) {
        bump_instance(&env);
        bump(&env, &DataKey::TotalRaised);
        bump(&env, &DataKey::TotalDisbursed);
    }

    /// True if this proof has already paid out a disbursement.
    pub fn is_proof_spent(env: Env, proof_hash: BytesN<32>) -> bool {
        env.storage().persistent().has(&DataKey::SpentProof(proof_hash))
    }

    pub fn get_admin(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Admin).unwrap()
    }

    pub fn get_token(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Token).unwrap()
    }

    /// Step 1 of an admin handover: the current admin nominates a successor.
    /// Nothing changes until the successor accepts, so a typo can't hand the
    /// contract to an address nobody controls. A new proposal replaces any
    /// pending one.
    pub fn propose_admin(env: Env, new_admin: Address) {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
        bump_instance(&env);
        env.storage().instance().set(&DataKey::PendingAdmin, &new_admin);
        env.events().publish((symbol_short!("adm_prop"), admin), new_admin);
    }

    /// Step 2: the nominated address accepts and becomes admin.
    pub fn accept_admin(env: Env) {
        let pending: Address = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .expect("no pending admin");
        pending.require_auth();
        bump_instance(&env);
        let old: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        env.storage().instance().set(&DataKey::Admin, &pending);
        env.storage().instance().remove(&DataKey::PendingAdmin);
        env.events().publish((symbol_short!("adm_acc"), old), pending);
    }

    /// The current admin withdraws a pending proposal.
    pub fn cancel_admin_transfer(env: Env) {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
        bump_instance(&env);
        assert!(
            env.storage().instance().has(&DataKey::PendingAdmin),
            "no pending admin"
        );
        env.storage().instance().remove(&DataKey::PendingAdmin);
        env.events().publish((symbol_short!("adm_cncl"), admin), ());
    }

    pub fn get_pending_admin(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::PendingAdmin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proof_registry::ProofRegistryContract;
    use soroban_sdk::FromVal;
    use soroban_sdk::{
        testutils::{storage::{Instance as _, Persistent as _}, Address as _, Events as _, Ledger},
        token::{Client as TokenClient, StellarAssetClient},
        Symbol,
    };

    struct Setup<'a> {
        vault: TreasuryVaultContractClient<'a>,
        registry: proof_registry::ProofRegistryContractClient<'a>,
        token: TokenClient<'a>,
        token_admin: StellarAssetClient<'a>,
        admin: Address,
    }

    fn setup(env: &Env) -> Setup<'_> {
        let admin = Address::generate(env);

        let token_id = env.register_stellar_asset_contract_v2(admin.clone()).address();
        let token = TokenClient::new(env, &token_id);
        let token_admin = StellarAssetClient::new(env, &token_id);

        let vault_id = env.register(TreasuryVaultContract, (admin.clone(), token_id.clone()));
        let vault = TreasuryVaultContractClient::new(env, &vault_id);

        let registry_id = env.register(ProofRegistryContract, (admin.clone(),));
        let registry = proof_registry::ProofRegistryContractClient::new(env, &registry_id);

        Setup { vault, registry, token, token_admin, admin }
    }

    fn fund_vault(env: &Env, setup: &Setup, amount: i128) {
        let depositor = Address::generate(env);
        setup.token_admin.mint(&depositor, &amount);
        setup.vault.deposit(&depositor, &amount);
    }

    fn some_hash(env: &Env, byte: u8) -> BytesN<32> {
        BytesN::from_array(env, &[byte; 32])
    }

    fn hex32(env: &Env, hex: &str) -> BytesN<32> {
        let mut out = [0u8; 32];
        for i in 0..32 {
            out[i] = u8::from_str_radix(&hex[2 + 2 * i..4 + 2 * i], 16).unwrap();
        }
        BytesN::from_array(env, &out)
    }

    // Test vectors produced by shieldfund-proof-server/src/hash.js
    // (hashPublicInputs / addressToField) for merkle_root = 0x11..11 and
    // budget_commitment = 0x22..22.
    //
    // A G... account recipient, checked against the hashing helpers only.
    const G_RECIPIENT: &str = "GBJ5FP5UB4YUE2EONTPPSAGKZZGDETFZLEJXJRCALSYTJZIDVWAN3C7P";
    const G_RECIPIENT_FIELD: &str =
        "0x1f5e152d8c3ea3c53910a0328b35a2ea0408937977bb10bde7562f287a1e3ed9";
    const G_PIH_500000_PAYROLL: &str =
        "0x1cd7ebfee1f402d0f5be4b37d05c8f853433807e8e90cfba4fe01cdfff805209";
    const G_PIH_500000_OPERATIONAL: &str =
        "0x468a2b56bd5a4f70a80bf2869af69da57612ef06169f08ff3d78eeb98936bca9";
    const G_PIH_499999_PAYROLL: &str =
        "0x5eec4cb1810118d05243ebdb6280e6e13ef13a666307c2d8be2b68f67cdff343";

    // A C... contract recipient used for end-to-end disbursements: contract
    // addresses hold the test token without needing a trustline.
    const RECIPIENT: &str = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM";
    const RECIPIENT_FIELD: &str =
        "0x1e9e09bf34d76d67afec2e73d8081a51376776fa9e04442261295d959c5ce033";
    const PIH_500000_PAYROLL: &str =
        "0xf19de4ca84abbed9f1bdd02b7ce2952da7a7ad598530fae7c48ec72c7c3f0b86";
    const PIH_500000_OPERATIONAL: &str =
        "0xe910f2502d7cd10d2510eac5dcfd2f970b5ad51151c3ebc3e658d7ae72fecb62";
    const PIH_499999_PAYROLL: &str =
        "0xf5d1987289f0e2e656ab3f2f7ad45db4d393499634383d8588dcbdecff62d796";
    const PIH_2000000_PAYROLL: &str =
        "0x4d3197baede201b251f37ee37bb0c7491de966932d271bf5961b3a622756b405";

    fn address(env: &Env, strkey: &str) -> Address {
        Address::from_string(&soroban_sdk::String::from_str(env, strkey))
    }

    fn recipient(env: &Env) -> Address {
        address(env, RECIPIENT)
    }

    fn root(env: &Env) -> BytesN<32> {
        some_hash(env, 0x11)
    }

    fn commitment(env: &Env) -> BytesN<32> {
        some_hash(env, 0x22)
    }

    /// Funds the vault, wires the registry, and registers proof `hash` with
    /// the given public-inputs hash and proof type.
    fn ready<'a>(env: &'a Env, hash: &BytesN<32>, pih: &str, proof_type: &str) -> Setup<'a> {
        let setup = setup(env);
        fund_vault(env, &setup, 1_000_000);
        setup.vault.set_proof_registry(&setup.registry.address);
        setup.registry.register_proof(
            &setup.admin,
            hash,
            &hex32(env, pih),
            &Symbol::new(env, proof_type),
        );
        setup
    }

    #[test]
    fn deposit_and_disburse_keep_state_alive() {
        let env = Env::default();
        // The test token isn't bumped by anyone (the real USDC SAC is), so
        // give new entries 25 days by default; ours must still reach 30.
        env.ledger().with_mut(|l| l.min_persistent_entry_ttl = 25 * DAY_IN_LEDGERS);
        env.mock_all_auths();
        let hash = some_hash(&env, 1);
        let setup = ready(&env, &hash, PIH_500000_PAYROLL, "payroll");
        env.ledger().with_mut(|l| l.sequence_number += 20 * DAY_IN_LEDGERS);

        setup.vault.disburse(&recipient(&env), &500_000, &hash, &root(&env), &commitment(&env));

        let id = setup.vault.address.clone();
        let ttl = |key: DataKey| env.as_contract(&id, || env.storage().persistent().get_ttl(&key));
        assert_eq!(ttl(DataKey::SpentProof(hash.clone())), TTL_EXTEND_TO);
        assert_eq!(ttl(DataKey::TotalDisbursed), TTL_EXTEND_TO);
        assert_eq!(env.as_contract(&id, || env.storage().instance().get_ttl()), TTL_EXTEND_TO);

        // The deposit happened 20 days ago; extend_ttl renews it, no auth needed.
        assert!(ttl(DataKey::TotalRaised) < TTL_THRESHOLD);
        env.set_auths(&[]);
        setup.vault.extend_ttl();
        assert_eq!(ttl(DataKey::TotalRaised), TTL_EXTEND_TO);
    }


    #[test]
    fn admin_handover_takes_two_steps() {
        let env = Env::default();
        env.mock_all_auths();
        let setup = setup(&env);
        let (client, admin) = (&setup.vault, setup.admin.clone());
        let nominee = Address::generate(&env);

        client.propose_admin(&nominee);
        assert_eq!(env.auths()[0].0, admin);
        assert_eq!(client.get_admin(), admin);
        assert_eq!(client.get_pending_admin(), Some(nominee.clone()));

        client.accept_admin();
        assert_eq!(env.auths()[0].0, nominee);
        assert_eq!(client.get_admin(), nominee);
        assert_eq!(client.get_pending_admin(), None);
    }

    #[test]
    fn only_the_admin_can_propose_and_only_the_nominee_can_accept() {
        let env = Env::default();
        env.mock_all_auths();
        let setup = setup(&env);
        let (client, admin) = (&setup.vault, setup.admin.clone());
        let nominee = Address::generate(&env);

        env.set_auths(&[]);
        assert!(client.try_propose_admin(&nominee).is_err());

        env.mock_all_auths();
        client.propose_admin(&nominee);
        env.set_auths(&[]);
        assert!(client.try_accept_admin().is_err());
        assert_eq!(client.get_admin(), admin);
    }

    #[test]
    #[should_panic(expected = "no pending admin")]
    fn accept_without_a_proposal_panics() {
        let env = Env::default();
        env.mock_all_auths();
        let setup = setup(&env);
        let (client, admin) = (&setup.vault, setup.admin.clone());
        let _ = admin;
        client.accept_admin();
    }

    #[test]
    fn cancelling_a_proposal_blocks_acceptance() {
        let env = Env::default();
        env.mock_all_auths();
        let setup = setup(&env);
        let (client, admin) = (&setup.vault, setup.admin.clone());
        client.propose_admin(&Address::generate(&env));
        client.cancel_admin_transfer();
        assert_eq!(client.get_pending_admin(), None);
        assert!(client.try_accept_admin().is_err());
        assert_eq!(client.get_admin(), admin);
    }

    #[test]
    fn wiring_and_disbursing_emit_events() {
        let env = Env::default();
        env.mock_all_auths();
        let hash = some_hash(&env, 1);
        let setup = ready(&env, &hash, PIH_500000_PAYROLL, "payroll");

        // events().all() holds the latest invocation's events, so re-wire and check.
        setup.vault.set_proof_registry(&setup.registry.address);
        let (contract, topics, data) = env.events().all().last().unwrap();
        assert_eq!(contract, setup.vault.address);
        assert_eq!(topics, vec![&env, symbol_short!("set_reg").into_val(&env), setup.admin.into_val(&env)]);
        assert_eq!(Address::from_val(&env, &data), setup.registry.address);

        setup.vault.disburse(&recipient(&env), &500_000, &hash, &root(&env), &commitment(&env));
        let (contract, topics, data) = env.events().all().last().unwrap();
        assert_eq!(contract, setup.vault.address);
        assert_eq!(topics, vec![&env, symbol_short!("disburse").into_val(&env), recipient(&env).into_val(&env)]);
        let (amount, proof): (i128, BytesN<32>) = data.into_val(&env);
        assert_eq!((amount, proof), (500_000, hash));
    }

    #[test]
    fn deposits_and_disbursements_must_be_positive() {
        let env = Env::default();
        env.mock_all_auths();
        let hash = some_hash(&env, 1);
        let setup = ready(&env, &hash, PIH_500000_PAYROLL, "payroll");
        let depositor = Address::generate(&env);
        for amount in [0i128, -1, i128::MIN] {
            assert!(setup.vault.try_deposit(&depositor, &amount).is_err());
            assert!(setup
                .vault
                .try_disburse(&recipient(&env), &amount, &hash, &root(&env), &commitment(&env))
                .is_err());
        }
        assert!(!setup.vault.is_proof_spent(&hash));
        assert_eq!(setup.vault.get_stats().total_raised, 1_000_000);
    }

    #[test]
    #[should_panic(expected = "insufficient vault balance")]
    fn disbursing_more_than_the_vault_holds_panics() {
        let env = Env::default();
        env.mock_all_auths();
        let hash = some_hash(&env, 1);
        // Valid proof for 2,000,000 against a vault funded with 1,000,000.
        let setup = ready(&env, &hash, PIH_2000000_PAYROLL, "payroll");
        setup.vault.disburse(&recipient(&env), &2_000_000, &hash, &root(&env), &commitment(&env));
    }

    #[test]
    fn deposits_and_disbursements_keep_totals_consistent() {
        let env = Env::default();
        env.mock_all_auths();
        let hash = some_hash(&env, 1);
        let setup = ready(&env, &hash, PIH_500000_PAYROLL, "payroll");
        fund_vault(&env, &setup, 250_000);
        setup.vault.disburse(&recipient(&env), &500_000, &hash, &root(&env), &commitment(&env));

        let stats = setup.vault.get_stats();
        assert_eq!(stats.total_raised, 1_250_000);
        assert_eq!(stats.total_disbursed, 500_000);
        assert_eq!(stats.vault_balance, stats.total_raised - stats.total_disbursed);
    }

    #[test]
    fn constructor_sets_admin_and_token() {
        let env = Env::default();
        let setup = setup(&env);
        assert_eq!(setup.vault.get_admin(), setup.admin);
        assert_eq!(setup.vault.get_token(), setup.token.address);
    }

    #[test]
    fn there_is_no_initialize_entrypoint_to_front_run() {
        let env = Env::default();
        let setup = setup(&env);
        let attacker = Address::generate(&env);
        let res = env.try_invoke_contract::<(), soroban_sdk::Error>(
            &setup.vault.address,
            &Symbol::new(&env, "initialize"),
            vec![&env, attacker.into_val(&env), setup.token.address.into_val(&env)],
        );
        assert!(res.is_err());
        assert_eq!(setup.vault.get_admin(), setup.admin);
    }

    #[test]
    fn hashing_matches_proof_server() {
        let env = Env::default();
        let cases = [
            (G_RECIPIENT, G_RECIPIENT_FIELD, [G_PIH_500000_PAYROLL, G_PIH_500000_OPERATIONAL, G_PIH_499999_PAYROLL]),
            (RECIPIENT, RECIPIENT_FIELD, [PIH_500000_PAYROLL, PIH_500000_OPERATIONAL, PIH_499999_PAYROLL]),
        ];
        for (strkey, field, [payroll, operational, smaller]) in cases {
            let who = address(&env, strkey);
            assert_eq!(recipient_field(&env, &who), hex32(&env, field));
            let pih = |amount: i128, pt: u32| {
                public_inputs_hash(&env, &root(&env), &commitment(&env), &who, amount, pt)
            };
            assert_eq!(pih(500_000, 0), hex32(&env, payroll));
            assert_eq!(pih(500_000, 1), hex32(&env, operational));
            assert_eq!(pih(499_999, 0), hex32(&env, smaller));
        }
    }

    #[test]
    #[should_panic(expected = "proof registry not configured")]
    fn disburse_without_registry_configured_panics() {
        let env = Env::default();
        env.mock_all_auths();
        let setup = setup(&env);
        fund_vault(&env, &setup, 1_000_000);

        setup.vault.disburse(&recipient(&env), &500_000, &some_hash(&env, 1), &root(&env), &commitment(&env));
    }

    #[test]
    #[should_panic(expected = "proof_hash not registered in proof_registry")]
    fn disburse_with_unregistered_hash_panics() {
        let env = Env::default();
        env.mock_all_auths();
        let setup = setup(&env);
        fund_vault(&env, &setup, 1_000_000);
        setup.vault.set_proof_registry(&setup.registry.address);

        setup.vault.disburse(&recipient(&env), &500_000, &some_hash(&env, 1), &root(&env), &commitment(&env));
    }

    #[test]
    fn disburse_with_matching_proof_succeeds() {
        let env = Env::default();
        env.mock_all_auths();
        let hash = some_hash(&env, 1);
        let setup = ready(&env, &hash, PIH_500000_PAYROLL, "payroll");

        assert!(!setup.vault.is_proof_spent(&hash));
        setup.vault.disburse(&recipient(&env), &500_000, &hash, &root(&env), &commitment(&env));

        assert_eq!(setup.token.balance(&recipient(&env)), 500_000);
        assert_eq!(setup.vault.get_stats().total_disbursed, 500_000);
        assert!(setup.vault.is_proof_spent(&hash));
    }

    #[test]
    #[should_panic(expected = "proof already used for a disbursement")]
    fn replaying_a_proof_panics() {
        let env = Env::default();
        env.mock_all_auths();
        let hash = some_hash(&env, 1);
        let setup = ready(&env, &hash, PIH_500000_PAYROLL, "payroll");

        setup.vault.disburse(&recipient(&env), &500_000, &hash, &root(&env), &commitment(&env));
        setup.vault.disburse(&recipient(&env), &500_000, &hash, &root(&env), &commitment(&env));
    }

    #[test]
    #[should_panic(expected = "proof does not match this recipient and amount")]
    fn wrong_amount_panics() {
        let env = Env::default();
        env.mock_all_auths();
        let hash = some_hash(&env, 1);
        let setup = ready(&env, &hash, PIH_499999_PAYROLL, "payroll");

        setup.vault.disburse(&recipient(&env), &500_000, &hash, &root(&env), &commitment(&env));
    }

    #[test]
    #[should_panic(expected = "proof does not match this recipient and amount")]
    fn wrong_recipient_panics() {
        let env = Env::default();
        env.mock_all_auths();
        let hash = some_hash(&env, 1);
        let setup = ready(&env, &hash, PIH_500000_PAYROLL, "payroll");

        setup.vault.disburse(&address(&env, G_RECIPIENT), &500_000, &hash, &root(&env), &commitment(&env));
    }

    #[test]
    #[should_panic(expected = "proof does not match this recipient and amount")]
    fn wrong_proof_type_panics() {
        let env = Env::default();
        env.mock_all_auths();
        let hash = some_hash(&env, 1);
        // Hash was computed for "operational" but the proof is registered as "payroll".
        let setup = ready(&env, &hash, PIH_500000_OPERATIONAL, "payroll");

        setup.vault.disburse(&recipient(&env), &500_000, &hash, &root(&env), &commitment(&env));
    }

    #[test]
    #[should_panic(expected = "proof does not match this recipient and amount")]
    fn wrong_merkle_root_panics() {
        let env = Env::default();
        env.mock_all_auths();
        let hash = some_hash(&env, 1);
        let setup = ready(&env, &hash, PIH_500000_PAYROLL, "payroll");

        setup.vault.disburse(&recipient(&env), &500_000, &hash, &some_hash(&env, 0x33), &commitment(&env));
    }

    #[test]
    fn a_failed_attempt_does_not_burn_the_proof() {
        let env = Env::default();
        env.mock_all_auths();
        let hash = some_hash(&env, 1);
        let setup = ready(&env, &hash, PIH_500000_PAYROLL, "payroll");

        let bad = setup.vault.try_disburse(&recipient(&env), &499_999, &hash, &root(&env), &commitment(&env));
        assert!(bad.is_err());
        assert!(!setup.vault.is_proof_spent(&hash));

        setup.vault.disburse(&recipient(&env), &500_000, &hash, &root(&env), &commitment(&env));
        assert_eq!(setup.token.balance(&recipient(&env)), 500_000);
    }
}
