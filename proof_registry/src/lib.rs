#![no_std]
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, symbol_short,
    Address, BytesN, Env, IntoVal, Symbol, Val, Vec,
};

/// Every way a call can fail. Clients receive these as `Error(Contract, #code)`.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    /// The caller isn't the admin.
    NotAdmin = 1,
    /// `proof_type` isn't payroll, operational or relief.
    UnknownProofType = 2,
    /// This proof hash is already registered.
    AlreadyRegistered = 3,
    /// No proof with this id or hash.
    ProofNotFound = 4,
    /// There's no admin handover to accept or cancel.
    NoPendingAdmin = 5,
}

fn ensure(env: &Env, condition: bool, error: Error) {
    if !condition {
        panic_with_error!(env, error);
    }
}

// ── Storage keys ──────────────────────────────────────────────────────────────

#[contracttype]
enum DataKey {
    Admin,
    PendingAdmin,
    ProofCount,
    Proof(u32),
    // Secondary index: proof_hash → proof id, for existence checks.
    HashIndex(BytesN<32>),
}

// ── Storage TTL ──────────────────────────────────────────────────────────────
//
// Soroban archives entries whose TTL runs out. Instance storage (admin, token,
// wiring) is bumped on every call; persistent entries are bumped whenever they
// are written or used, and anyone can bump them explicitly via `extend_ttl`.

const DAY_IN_LEDGERS: u32 = 17_280; // ~5s ledgers
const TTL_EXTEND_TO: u32 = 30 * DAY_IN_LEDGERS;
const TTL_THRESHOLD: u32 = TTL_EXTEND_TO - DAY_IN_LEDGERS;

/// Largest page `get_proofs` returns, keeping reads well inside resource limits.
pub const MAX_PAGE: u32 = 50;

fn bump_instance(env: &Env) {
    env.storage().instance().extend_ttl(TTL_THRESHOLD, TTL_EXTEND_TO);
}

fn bump<K: IntoVal<Env, Val>>(env: &Env, key: &K) {
    env.storage().persistent().extend_ttl(key, TTL_THRESHOLD, TTL_EXTEND_TO);
}

// ── Public types ─────────────────────────────────────────────────────────────

/// A ZK proof registration anchored on Stellar.
///
/// `proof_hash`        — 32-byte Poseidon/Keccak hash of the full Noir proof
///                       (computed off-chain by the proof server).
/// `public_inputs_hash`— 32-byte hash of the public inputs used to generate
///                       the proof. Allows third-party re-verification.
/// `proof_type`        — one of: "payroll" | "operational" | "relief"
///                       (9-char max, stored as Symbol for efficient lookup).
/// `timestamp`         — ledger Unix timestamp at registration.
/// `submitter`         — Stellar account that submitted the proof.
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

// ── Contract ─────────────────────────────────────────────────────────────────

#[contract]
pub struct ProofRegistryContract;

#[contractimpl]
impl ProofRegistryContract {
    /// Runs once, atomically with deployment, so there is no window in which
    /// someone else could claim the admin role on an uninitialised contract.
    pub fn __constructor(env: Env, admin: Address) {
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().persistent().set(&DataKey::ProofCount, &0u32);
        bump_instance(&env);
        bump(&env, &DataKey::ProofCount);
        env.events().publish((symbol_short!("init"),), admin);
    }

    /// Register a ZK proof on-chain.
    ///
    /// Called by the proof backend after successfully generating and locally
    /// verifying the Noir proof. Restricted to the admin so a random address
    /// can't anchor an unverified hash into the registry that
    /// treasury_vault::disburse() would then trust.
    ///
    /// Panics if `submitter` is not the admin, or if the same `proof_hash` is
    /// registered twice.
    pub fn register_proof(
        env: Env,
        submitter: Address,
        proof_hash: BytesN<32>,
        public_inputs_hash: BytesN<32>,
        proof_type: Symbol,
    ) -> u32 {
        submitter.require_auth();
        bump_instance(&env);

        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        ensure(&env, submitter == admin, Error::NotAdmin);
        ensure(
            &env,
            proof_type == Symbol::new(&env, "payroll")
                || proof_type == Symbol::new(&env, "operational")
                || proof_type == Symbol::new(&env, "relief"),
            Error::UnknownProofType,
        );

        // Guard against duplicate registrations.
        ensure(
            &env,
            !env.storage()
                .persistent()
                .has(&DataKey::HashIndex(proof_hash.clone())),
            Error::AlreadyRegistered,
        );

        let id: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::ProofCount)
            .unwrap_or(0);

        let entry = ProofEntry {
            id,
            proof_hash: proof_hash.clone(),
            public_inputs_hash,
            proof_type: proof_type.clone(),
            timestamp: env.ledger().timestamp(),
            submitter: submitter.clone(),
        };

        env.storage()
            .persistent()
            .set(&DataKey::Proof(id), &entry);
        env.storage()
            .persistent()
            .set(&DataKey::HashIndex(proof_hash.clone()), &id);
        env.storage()
            .persistent()
            .set(&DataKey::ProofCount, &(id + 1));
        bump(&env, &DataKey::Proof(id));
        bump(&env, &DataKey::HashIndex(proof_hash.clone()));
        bump(&env, &DataKey::ProofCount);

        env.events().publish(
            (symbol_short!("p_reg"), submitter),
            (id, proof_hash, proof_type),
        );

        id
    }

    /// Returns a single proof entry by sequential ID.
    pub fn get_proof(env: Env, id: u32) -> ProofEntry {
        env.storage()
            .persistent()
            .get(&DataKey::Proof(id))
            .unwrap_or_else(|| panic_with_error!(&env, Error::ProofNotFound))
    }

    /// Returns up to `limit` proofs (max `MAX_PAGE`) starting at id `start`,
    /// in id order. Page through with `start += returned.len()` until
    /// `start >= get_proof_count()`.
    pub fn get_proofs(env: Env, start: u32, limit: u32) -> Vec<ProofEntry> {
        let count = Self::get_proof_count(env.clone());
        let end = start.saturating_add(limit.min(MAX_PAGE)).min(count);
        let mut proofs = Vec::new(&env);
        for i in start..end {
            if let Some(p) = env
                .storage()
                .persistent()
                .get::<DataKey, ProofEntry>(&DataKey::Proof(i))
            {
                proofs.push_back(p);
            }
        }
        proofs
    }

    pub fn get_proof_count(env: Env) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::ProofCount)
            .unwrap_or(0)
    }

    /// Returns all registered proofs. Unbounded: once there are many proofs
    /// this exceeds per-call resource limits — use `get_proofs` instead.
    pub fn get_all_proofs(env: Env) -> Vec<ProofEntry> {
        let count: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::ProofCount)
            .unwrap_or(0);
        let mut proofs = Vec::new(&env);
        for i in 0..count {
            if let Some(p) = env
                .storage()
                .persistent()
                .get::<DataKey, ProofEntry>(&DataKey::Proof(i))
            {
                proofs.push_back(p);
            }
        }
        proofs
    }

    /// Returns true if a proof with this exact hash has been registered.
    /// Useful for quick on-chain existence checks from the frontend or other
    /// contracts (e.g. vault.disburse can optionally call this before paying).
    pub fn verify_proof_exists(env: Env, proof_hash: BytesN<32>) -> bool {
        bump_instance(&env);
        let key = DataKey::HashIndex(proof_hash);
        let exists = env.storage().persistent().has(&key);
        if exists {
            bump(&env, &key);
        }
        exists
    }

    /// Returns the proof ID for a given hash, or panics if not found.
    pub fn get_id_by_hash(env: Env, proof_hash: BytesN<32>) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::HashIndex(proof_hash))
            .unwrap_or_else(|| panic_with_error!(&env, Error::ProofNotFound))
    }

    /// Returns the full entry for a given proof hash, or panics if not found.
    /// treasury_vault::disburse() uses this to check the registered
    /// public_inputs_hash and proof_type against the payment it's making.
    pub fn get_proof_by_hash(env: Env, proof_hash: BytesN<32>) -> ProofEntry {
        let id = Self::get_id_by_hash(env.clone(), proof_hash);
        bump(&env, &DataKey::Proof(id));
        Self::get_proof(env, id)
    }

    /// Keeps a proof's entries (and the contract instance) alive. Anyone may
    /// call this; it only extends TTLs, it changes no data.
    pub fn extend_ttl(env: Env, id: u32) {
        bump_instance(&env);
        bump(&env, &DataKey::ProofCount);
        let entry = Self::get_proof(env.clone(), id);
        bump(&env, &DataKey::Proof(id));
        bump(&env, &DataKey::HashIndex(entry.proof_hash));
    }

    pub fn get_admin(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Admin).unwrap()
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
            .unwrap_or_else(|| panic_with_error!(&env, Error::NoPendingAdmin));
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
        ensure(&env, env.storage().instance().has(&DataKey::PendingAdmin), Error::NoPendingAdmin);
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
    use soroban_sdk::{
        testutils::{storage::{Instance as _, Persistent as _}, Address as _, Events as _, Ledger},
        BytesN, Env, FromVal, IntoVal, Symbol,
    };

    fn deploy(env: &Env) -> (ProofRegistryContractClient<'_>, Address) {
        let admin = Address::generate(env);
        let contract_id = env.register(ProofRegistryContract, (admin.clone(),));
        let client = ProofRegistryContractClient::new(env, &contract_id);
        (client, admin)
    }

    fn zero_hash(env: &Env) -> BytesN<32> {
        BytesN::from_array(env, &[0u8; 32])
    }

    fn one_hash(env: &Env) -> BytesN<32> {
        BytesN::from_array(env, &[1u8; 32])
    }

    #[test]
    fn register_and_verify_proof() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = deploy(&env);

        let hash = zero_hash(&env);
        let inputs_hash = one_hash(&env);
        let proof_type = Symbol::new(&env, "payroll");

        let id = client.register_proof(&admin, &hash, &inputs_hash, &proof_type);
        assert_eq!(id, 0);
        assert!(client.verify_proof_exists(&hash));
    }

    #[test]
    fn verify_nonexistent_proof_returns_false() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, _) = deploy(&env);

        assert!(!client.verify_proof_exists(&zero_hash(&env)));
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #3)")]
    fn duplicate_registration_panics() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = deploy(&env);

        let hash = zero_hash(&env);
        let inputs = one_hash(&env);
        let pt = Symbol::new(&env, "payroll");

        client.register_proof(&admin, &hash, &inputs, &pt);
        // Second call with same hash must panic
        client.register_proof(&admin, &hash, &inputs, &pt);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #1)")]
    fn non_admin_submitter_panics() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, _admin) = deploy(&env);

        let outsider = Address::generate(&env);
        let hash = zero_hash(&env);
        let inputs = one_hash(&env);
        let pt = Symbol::new(&env, "payroll");

        client.register_proof(&outsider, &hash, &inputs, &pt);
    }

    #[test]
    fn proof_count_increments_per_registration() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = deploy(&env);

        let pt = Symbol::new(&env, "payroll");
        let id0 = client.register_proof(&admin, &zero_hash(&env), &one_hash(&env), &pt);
        let id1 = client.register_proof(&admin, &one_hash(&env), &zero_hash(&env), &pt);

        assert_eq!(id0, 0);
        assert_eq!(id1, 1);
        assert_eq!(client.get_all_proofs().len(), 2);
    }

    fn ttl(env: &Env, contract: &Address, key: &DataKey) -> u32 {
        env.as_contract(contract, || env.storage().persistent().get_ttl(key))
    }

    fn instance_ttl(env: &Env, contract: &Address) -> u32 {
        env.as_contract(contract, || env.storage().instance().get_ttl())
    }

    fn advance_days(env: &Env, days: u32) {
        env.ledger().with_mut(|l| l.sequence_number += days * DAY_IN_LEDGERS);
    }

    #[test]
    fn registered_proof_gets_a_long_ttl() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = deploy(&env);
        client.register_proof(&admin, &one_hash(&env), &zero_hash(&env), &Symbol::new(&env, "payroll"));

        let id = &client.address;
        assert_eq!(ttl(&env, id, &DataKey::Proof(0)), TTL_EXTEND_TO);
        assert_eq!(ttl(&env, id, &DataKey::HashIndex(one_hash(&env))), TTL_EXTEND_TO);
        assert_eq!(ttl(&env, id, &DataKey::ProofCount), TTL_EXTEND_TO);
        assert_eq!(instance_ttl(&env, id), TTL_EXTEND_TO);
    }

    #[test]
    fn using_a_proof_renews_its_ttl() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = deploy(&env);
        client.register_proof(&admin, &one_hash(&env), &zero_hash(&env), &Symbol::new(&env, "payroll"));

        // Well past the network's default TTL, still inside ours.
        advance_days(&env, 20);
        assert!(client.verify_proof_exists(&one_hash(&env)));
        client.get_proof_by_hash(&one_hash(&env));

        let id = &client.address;
        assert_eq!(ttl(&env, id, &DataKey::Proof(0)), TTL_EXTEND_TO);
        assert_eq!(ttl(&env, id, &DataKey::HashIndex(one_hash(&env))), TTL_EXTEND_TO);
        assert_eq!(instance_ttl(&env, id), TTL_EXTEND_TO);
    }

    #[test]
    fn anyone_can_extend_a_proofs_ttl() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = deploy(&env);
        client.register_proof(&admin, &one_hash(&env), &zero_hash(&env), &Symbol::new(&env, "payroll"));
        advance_days(&env, 20);

        // No auth needed: drop the mocked auths before calling.
        env.set_auths(&[]);
        client.extend_ttl(&0);

        let id = &client.address;
        assert_eq!(ttl(&env, id, &DataKey::Proof(0)), TTL_EXTEND_TO);
        assert_eq!(ttl(&env, id, &DataKey::HashIndex(one_hash(&env))), TTL_EXTEND_TO);
        assert_eq!(ttl(&env, id, &DataKey::ProofCount), TTL_EXTEND_TO);
    }


    #[test]
    fn admin_handover_takes_two_steps() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = deploy(&env);
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
        let (client, admin) = deploy(&env);
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
    #[should_panic(expected = "Error(Contract, #5)")]
    fn accept_without_a_proposal_panics() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = deploy(&env);
        let _ = admin;
        client.accept_admin();
    }

    #[test]
    fn cancelling_a_proposal_blocks_acceptance() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = deploy(&env);
        client.propose_admin(&Address::generate(&env));
        client.cancel_admin_transfer();
        assert_eq!(client.get_pending_admin(), None);
        assert!(client.try_accept_admin().is_err());
        assert_eq!(client.get_admin(), admin);
    }

    #[test]
    fn previous_admin_loses_rights_after_handover() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = deploy(&env);
        let nominee = Address::generate(&env);
        client.propose_admin(&nominee);
        client.accept_admin();

        let pt = Symbol::new(&env, "payroll");
        assert!(client.try_register_proof(&admin, &one_hash(&env), &zero_hash(&env), &pt).is_err());
        client.register_proof(&nominee, &one_hash(&env), &zero_hash(&env), &pt);
    }

    #[test]
    fn registering_a_proof_emits_p_reg() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = deploy(&env);
        let pt = Symbol::new(&env, "payroll");
        client.register_proof(&admin, &one_hash(&env), &zero_hash(&env), &pt);

        let (contract, topics, data) = env.events().all().last().unwrap();
        assert_eq!(contract, client.address);
        assert_eq!(topics, soroban_sdk::vec![&env, symbol_short!("p_reg").into_val(&env), admin.into_val(&env)]);
        let (id, hash, ty): (u32, BytesN<32>, Symbol) = data.into_val(&env);
        assert_eq!((id, hash, ty), (0, one_hash(&env), pt));
    }

    #[test]
    fn admin_handover_emits_events() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = deploy(&env);
        let nominee = Address::generate(&env);

        client.propose_admin(&nominee);
        let (_, topics, data) = env.events().all().last().unwrap();
        assert_eq!(topics, soroban_sdk::vec![&env, symbol_short!("adm_prop").into_val(&env), admin.into_val(&env)]);
        assert_eq!(Address::from_val(&env, &data), nominee);

        client.accept_admin();
        let (_, topics, data) = env.events().all().last().unwrap();
        assert_eq!(topics, soroban_sdk::vec![&env, symbol_short!("adm_acc").into_val(&env), admin.into_val(&env)]);
        assert_eq!(Address::from_val(&env, &data), nominee);
    }

    #[test]
    fn proofs_page_in_id_order_and_cap_at_max_page() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = deploy(&env);
        let pt = Symbol::new(&env, "payroll");
        for i in 0..60u8 {
            client.register_proof(&admin, &BytesN::from_array(&env, &[i; 32]), &zero_hash(&env), &pt);
        }
        assert_eq!(client.get_proof_count(), 60);

        let first = client.get_proofs(&0, &20);
        assert_eq!(first.len(), 20);
        assert_eq!(first.get(0).unwrap().id, 0);
        assert_eq!(first.get(19).unwrap().id, 19);

        // limit is capped, and the last page is short
        assert_eq!(client.get_proofs(&0, &1_000).len(), MAX_PAGE);
        let last = client.get_proofs(&50, &50);
        assert_eq!(last.len(), 10);
        assert_eq!(last.get(9).unwrap().id, 59);

        // walking the pages visits every proof exactly once
        let mut seen = 0u32;
        let mut start = 0u32;
        while start < client.get_proof_count() {
            let page = client.get_proofs(&start, &MAX_PAGE);
            for p in page.iter() {
                assert_eq!(p.id, seen);
                seen += 1;
            }
            start += page.len();
        }
        assert_eq!(seen, 60);

        assert_eq!(client.get_proofs(&60, &10).len(), 0);
        assert_eq!(client.get_proofs(&u32::MAX, &u32::MAX).len(), 0);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #2)")]
    fn unknown_proof_types_are_rejected_at_registration() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = deploy(&env);
        client.register_proof(&admin, &one_hash(&env), &zero_hash(&env), &Symbol::new(&env, "payrol"));
    }

    #[test]
    fn all_three_proof_types_are_accepted() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = deploy(&env);
        for (i, ty) in ["payroll", "operational", "relief"].iter().enumerate() {
            let hash = BytesN::from_array(&env, &[i as u8 + 10; 32]);
            client.register_proof(&admin, &hash, &zero_hash(&env), &Symbol::new(&env, ty));
        }
        assert_eq!(client.get_proof_count(), 3);
    }

    #[test]
    fn clients_get_typed_errors() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = deploy(&env);
        let pt = Symbol::new(&env, "payroll");
        client.register_proof(&admin, &one_hash(&env), &zero_hash(&env), &pt);

        assert_eq!(
            client.try_register_proof(&admin, &one_hash(&env), &zero_hash(&env), &pt),
            Err(Ok(soroban_sdk::Error::from(Error::AlreadyRegistered)))
        );
        assert_eq!(
            client.try_register_proof(&Address::generate(&env), &zero_hash(&env), &zero_hash(&env), &pt),
            Err(Ok(soroban_sdk::Error::from(Error::NotAdmin)))
        );
        assert_eq!(client.try_get_proof(&9).err(), Some(Ok(soroban_sdk::Error::from(Error::ProofNotFound))));
        assert_eq!(client.try_accept_admin(), Err(Ok(soroban_sdk::Error::from(Error::NoPendingAdmin))));
    }

    #[test]
    fn constructor_sets_admin() {
        let env = Env::default();
        let (client, admin) = deploy(&env);
        assert_eq!(client.get_admin(), admin);
    }

    #[test]
    fn there_is_no_initialize_entrypoint_to_front_run() {
        let env = Env::default();
        let (client, _) = deploy(&env);
        let attacker = Address::generate(&env);
        let res = env.try_invoke_contract::<(), soroban_sdk::Error>(
            &client.address,
            &Symbol::new(&env, "initialize"),
            soroban_sdk::vec![&env, attacker.into_val(&env)],
        );
        assert!(res.is_err());
    }

    #[test]
    fn get_proof_by_hash_returns_registered_entry() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = deploy(&env);

        let pt = Symbol::new(&env, "relief");
        client.register_proof(&admin, &zero_hash(&env), &zero_hash(&env), &pt);
        client.register_proof(&admin, &one_hash(&env), &zero_hash(&env), &pt);

        let entry = client.get_proof_by_hash(&one_hash(&env));
        assert_eq!(entry.id, 1);
        assert_eq!(entry.proof_hash, one_hash(&env));
        assert_eq!(entry.proof_type, pt);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #4)")]
    fn get_proof_by_hash_unknown_panics() {
        let env = Env::default();
        let (client, _) = deploy(&env);
        client.get_proof_by_hash(&one_hash(&env));
    }
}
