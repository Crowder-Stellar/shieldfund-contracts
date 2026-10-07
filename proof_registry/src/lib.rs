#![no_std]
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short,
    Address, BytesN, Env, Symbol, Vec,
};

// ── Storage keys ──────────────────────────────────────────────────────────────

#[contracttype]
enum DataKey {
    Admin,
    // Address proposed by `propose_admin`, waiting to `accept_admin`.
    PendingAdmin,
    ProofCount,
    Proof(u32),
    // Secondary index: proof_hash → proof id, for existence checks.
    HashIndex(BytesN<32>),
}

// ── Errors ────────────────────────────────────────────────────────────────────

/// Typed errors returned to clients (surfaced as `Error(Contract, #n)`).
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    /// `submitter` is not the registry admin.
    NotAdmin = 1,
    /// A proof with this `proof_hash` is already registered.
    AlreadyRegistered = 2,
    /// No proof with this id / hash.
    ProofNotFound = 3,
    /// `proof_type` is not one of "payroll" | "operational" | "relief".
    InvalidProofType = 4,
    /// `accept_admin` / `cancel_admin_transfer` with no transfer pending.
    NoPendingAdmin = 5,
    /// Page `limit` is 0 or larger than `MAX_PAGE_SIZE`.
    InvalidPageSize = 6,
}

/// Largest page `get_proofs` will return in one call.
pub const MAX_PAGE_SIZE: u32 = 50;

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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProofEntry {
    pub id: u32,
    pub proof_hash: BytesN<32>,
    pub public_inputs_hash: BytesN<32>,
    pub proof_type: Symbol,
    pub timestamp: u64,
    pub submitter: Address,
}

fn admin(env: &Env) -> Address {
    env.storage().instance().get(&DataKey::Admin).unwrap()
}

fn is_valid_proof_type(env: &Env, proof_type: &Symbol) -> bool {
    *proof_type == Symbol::new(env, "payroll")
        || *proof_type == Symbol::new(env, "operational")
        || *proof_type == Symbol::new(env, "relief")
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
    }

    /// Register a ZK proof on-chain.
    ///
    /// Called by the proof backend after successfully generating and locally
    /// verifying the Noir proof. Restricted to the admin so a random address
    /// can't anchor an unverified hash into the registry that
    /// treasury_vault::disburse() would then trust.
    ///
    /// Errors: `NotAdmin`, `InvalidProofType`, `AlreadyRegistered`.
    ///
    /// Event: `("p_reg", submitter)` → `(id, proof_hash, public_inputs_hash, proof_type)`.
    pub fn register_proof(
        env: Env,
        submitter: Address,
        proof_hash: BytesN<32>,
        public_inputs_hash: BytesN<32>,
        proof_type: Symbol,
    ) -> Result<u32, Error> {
        submitter.require_auth();
        if submitter != admin(&env) {
            return Err(Error::NotAdmin);
        }
        if !is_valid_proof_type(&env, &proof_type) {
            return Err(Error::InvalidProofType);
        }
        if env.storage().persistent().has(&DataKey::HashIndex(proof_hash.clone())) {
            return Err(Error::AlreadyRegistered);
        }

        let id: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::ProofCount)
            .unwrap_or(0);

        let entry = ProofEntry {
            id,
            proof_hash: proof_hash.clone(),
            public_inputs_hash: public_inputs_hash.clone(),
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

        env.events().publish(
            (symbol_short!("p_reg"), submitter),
            (id, proof_hash, public_inputs_hash, proof_type),
        );

        Ok(id)
    }

    /// Returns a single proof entry by sequential ID.
    pub fn get_proof(env: Env, id: u32) -> Result<ProofEntry, Error> {
        env.storage()
            .persistent()
            .get(&DataKey::Proof(id))
            .ok_or(Error::ProofNotFound)
    }

    /// Number of registered proofs (ids are `0..count`).
    pub fn get_proof_count(env: Env) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::ProofCount)
            .unwrap_or(0)
    }

    /// Returns up to `limit` proofs with ids `start, start+1, …`, oldest first.
    /// A `start` past the end returns an empty list.
    ///
    /// Errors: `InvalidPageSize` if `limit` is 0 or above `MAX_PAGE_SIZE`.
    pub fn get_proofs(env: Env, start: u32, limit: u32) -> Result<Vec<ProofEntry>, Error> {
        if limit == 0 || limit > MAX_PAGE_SIZE {
            return Err(Error::InvalidPageSize);
        }
        let count = Self::get_proof_count(env.clone());
        let end = start.saturating_add(limit).min(count);
        let mut proofs = Vec::new(&env);
        for i in start..end {
            if let Some(p) = env.storage().persistent().get::<DataKey, ProofEntry>(&DataKey::Proof(i)) {
                proofs.push_back(p);
            }
        }
        Ok(proofs)
    }

    /// Returns all registered proofs.
    ///
    /// Deprecated: cost grows with the registry and will eventually exceed
    /// read limits. Use `get_proof_count` + `get_proofs` instead.
    pub fn get_all_proofs(env: Env) -> Vec<ProofEntry> {
        let count = Self::get_proof_count(env.clone());
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
        env.storage()
            .persistent()
            .has(&DataKey::HashIndex(proof_hash))
    }

    /// Returns the proof ID for a given hash.
    pub fn get_id_by_hash(env: Env, proof_hash: BytesN<32>) -> Result<u32, Error> {
        env.storage()
            .persistent()
            .get(&DataKey::HashIndex(proof_hash))
            .ok_or(Error::ProofNotFound)
    }

    /// Returns the full entry for a given proof hash.
    /// treasury_vault::disburse() uses this to check the registered
    /// public_inputs_hash and proof_type against the payment it's making.
    pub fn get_proof_by_hash(env: Env, proof_hash: BytesN<32>) -> Result<ProofEntry, Error> {
        let id = Self::get_id_by_hash(env.clone(), proof_hash)?;
        Self::get_proof(env, id)
    }

    pub fn get_admin(env: Env) -> Address {
        admin(&env)
    }

    // ── Two-step admin transfer ──────────────────────────────────────────────

    /// Step 1 (current admin): nominate `new_admin`. Nothing changes until the
    /// nominee calls `accept_admin`, so a mistyped address can't take over.
    /// A later call replaces the pending nominee.
    ///
    /// Event: `("adm_prop", admin)` → `new_admin`.
    pub fn propose_admin(env: Env, new_admin: Address) {
        let admin = admin(&env);
        admin.require_auth();
        env.storage().instance().set(&DataKey::PendingAdmin, &new_admin);
        env.events().publish((symbol_short!("adm_prop"), admin), new_admin);
    }

    /// Step 2 (nominee): accept the admin role. Requires the nominee's auth.
    ///
    /// Errors: `NoPendingAdmin`.
    /// Event: `("adm_acpt", new_admin)` → `previous_admin`.
    pub fn accept_admin(env: Env) -> Result<(), Error> {
        let pending: Address = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .ok_or(Error::NoPendingAdmin)?;
        pending.require_auth();
        let previous = admin(&env);
        env.storage().instance().set(&DataKey::Admin, &pending);
        env.storage().instance().remove(&DataKey::PendingAdmin);
        env.events().publish((symbol_short!("adm_acpt"), pending), previous);
        Ok(())
    }

    /// Current admin withdraws a pending nomination.
    ///
    /// Errors: `NoPendingAdmin`.
    /// Event: `("adm_cncl", admin)` → `cancelled_nominee`.
    pub fn cancel_admin_transfer(env: Env) -> Result<(), Error> {
        let admin = admin(&env);
        admin.require_auth();
        let pending: Address = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .ok_or(Error::NoPendingAdmin)?;
        env.storage().instance().remove(&DataKey::PendingAdmin);
        env.events().publish((symbol_short!("adm_cncl"), admin), pending);
        Ok(())
    }

    pub fn get_pending_admin(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::PendingAdmin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Events},
        vec, BytesN, Env, IntoVal, Symbol,
    };

    fn deploy(env: &Env) -> (ProofRegistryContractClient<'_>, Address) {
        let admin = Address::generate(env);
        let contract_id = env.register(ProofRegistryContract, (admin.clone(),));
        let client = ProofRegistryContractClient::new(env, &contract_id);
        (client, admin)
    }

    fn hash(env: &Env, byte: u8) -> BytesN<32> {
        BytesN::from_array(env, &[byte; 32])
    }

    fn payroll(env: &Env) -> Symbol {
        Symbol::new(env, "payroll")
    }

    fn register_n(env: &Env, client: &ProofRegistryContractClient, admin: &Address, n: u8) {
        for i in 0..n {
            client.register_proof(admin, &hash(env, i), &hash(env, 0xff), &payroll(env));
        }
    }

    #[test]
    fn register_and_verify_proof() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = deploy(&env);

        let id = client.register_proof(&admin, &hash(&env, 0), &hash(&env, 1), &payroll(&env));
        assert_eq!(id, 0);
        assert!(client.verify_proof_exists(&hash(&env, 0)));
    }

    #[test]
    fn register_emits_indexable_event() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = deploy(&env);

        client.register_proof(&admin, &hash(&env, 7), &hash(&env, 8), &payroll(&env));
        let (contract, topics, data) = env.events().all().last().unwrap();
        assert_eq!(contract, client.address);
        assert_eq!(topics, vec![&env, symbol_short!("p_reg").into_val(&env), admin.into_val(&env)]);
        let data: (u32, BytesN<32>, BytesN<32>, Symbol) = data.into_val(&env);
        assert_eq!(data, (0, hash(&env, 7), hash(&env, 8), payroll(&env)));
    }

    #[test]
    fn verify_nonexistent_proof_returns_false() {
        let env = Env::default();
        let (client, _) = deploy(&env);
        assert!(!client.verify_proof_exists(&hash(&env, 0)));
    }

    #[test]
    fn duplicate_registration_is_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = deploy(&env);

        client.register_proof(&admin, &hash(&env, 0), &hash(&env, 1), &payroll(&env));
        let res = client.try_register_proof(&admin, &hash(&env, 0), &hash(&env, 1), &payroll(&env));
        assert_eq!(res, Err(Ok(Error::AlreadyRegistered)));
    }

    #[test]
    fn non_admin_submitter_is_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, _admin) = deploy(&env);

        let outsider = Address::generate(&env);
        let res = client.try_register_proof(&outsider, &hash(&env, 0), &hash(&env, 1), &payroll(&env));
        assert_eq!(res, Err(Ok(Error::NotAdmin)));
    }

    #[test]
    fn unknown_proof_type_is_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = deploy(&env);

        let res = client.try_register_proof(&admin, &hash(&env, 0), &hash(&env, 1), &Symbol::new(&env, "bonus"));
        assert_eq!(res, Err(Ok(Error::InvalidProofType)));
        assert_eq!(client.get_proof_count(), 0);
    }

    #[test]
    fn proof_count_increments_per_registration() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = deploy(&env);

        register_n(&env, &client, &admin, 2);
        assert_eq!(client.get_proof_count(), 2);
        assert_eq!(client.get_all_proofs().len(), 2);
    }

    #[test]
    fn constructor_sets_admin() {
        let env = Env::default();
        let (client, admin) = deploy(&env);
        assert_eq!(client.get_admin(), admin);
        assert_eq!(client.get_pending_admin(), None);
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
        client.register_proof(&admin, &hash(&env, 0), &hash(&env, 0), &pt);
        client.register_proof(&admin, &hash(&env, 1), &hash(&env, 0), &pt);

        let entry = client.get_proof_by_hash(&hash(&env, 1));
        assert_eq!(entry.id, 1);
        assert_eq!(entry.proof_hash, hash(&env, 1));
        assert_eq!(entry.proof_type, pt);
    }

    #[test]
    fn unknown_proof_lookups_return_proof_not_found() {
        let env = Env::default();
        let (client, _) = deploy(&env);
        assert_eq!(client.try_get_proof_by_hash(&hash(&env, 1)), Err(Ok(Error::ProofNotFound)));
        assert_eq!(client.try_get_id_by_hash(&hash(&env, 1)), Err(Ok(Error::ProofNotFound)));
        assert_eq!(client.try_get_proof(&0), Err(Ok(Error::ProofNotFound)));
    }

    // ── Pagination ───────────────────────────────────────────────────────────

    #[test]
    fn get_proofs_pages_through_all_entries() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = deploy(&env);
        register_n(&env, &client, &admin, 5);

        let first = client.get_proofs(&0, &2);
        let second = client.get_proofs(&2, &2);
        let last = client.get_proofs(&4, &2);
        assert_eq!((first.len(), second.len(), last.len()), (2, 2, 1));
        assert_eq!(first.get(0).unwrap().id, 0);
        assert_eq!(second.get(1).unwrap().id, 3);
        assert_eq!(last.get(0).unwrap().id, 4);
    }

    #[test]
    fn get_proofs_past_the_end_is_empty() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = deploy(&env);
        register_n(&env, &client, &admin, 3);

        assert_eq!(client.get_proofs(&3, &10).len(), 0);
        assert_eq!(client.get_proofs(&u32::MAX, &MAX_PAGE_SIZE).len(), 0);
    }

    #[test]
    fn get_proofs_rejects_bad_page_sizes() {
        let env = Env::default();
        let (client, _) = deploy(&env);
        assert_eq!(client.try_get_proofs(&0, &0), Err(Ok(Error::InvalidPageSize)));
        assert_eq!(client.try_get_proofs(&0, &(MAX_PAGE_SIZE + 1)), Err(Ok(Error::InvalidPageSize)));
    }

    // ── Two-step admin transfer ──────────────────────────────────────────────

    #[test]
    fn admin_transfer_requires_acceptance() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = deploy(&env);
        let next = Address::generate(&env);

        client.propose_admin(&next);
        assert_eq!(client.get_admin(), admin, "proposal alone must not change admin");
        assert_eq!(client.get_pending_admin(), Some(next.clone()));

        client.accept_admin();
        assert_eq!(client.get_admin(), next);
        assert_eq!(client.get_pending_admin(), None);

        // The old admin can no longer register proofs; the new one can.
        let res = client.try_register_proof(&admin, &hash(&env, 0), &hash(&env, 1), &payroll(&env));
        assert_eq!(res, Err(Ok(Error::NotAdmin)));
        client.register_proof(&next, &hash(&env, 0), &hash(&env, 1), &payroll(&env));
    }

    #[test]
    fn accept_requires_the_nominees_auth() {
        let env = Env::default();
        let (client, admin) = deploy(&env);
        let next = Address::generate(&env);

        env.mock_all_auths();
        client.propose_admin(&next);
        env.set_auths(&[]);

        assert!(client.try_accept_admin().is_err());
        assert_eq!(client.get_admin(), admin);
    }

    #[test]
    fn accept_without_proposal_fails() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, _) = deploy(&env);
        assert_eq!(client.try_accept_admin(), Err(Ok(Error::NoPendingAdmin)));
        assert_eq!(client.try_cancel_admin_transfer(), Err(Ok(Error::NoPendingAdmin)));
    }

    #[test]
    fn cancel_clears_pending_admin() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = deploy(&env);

        client.propose_admin(&Address::generate(&env));
        client.cancel_admin_transfer();
        assert_eq!(client.get_pending_admin(), None);
        assert_eq!(client.try_accept_admin(), Err(Ok(Error::NoPendingAdmin)));
        assert_eq!(client.get_admin(), admin);
    }

    #[test]
    fn admin_changes_emit_events() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = deploy(&env);
        let next = Address::generate(&env);

        client.propose_admin(&next);
        let (_, topics, data) = env.events().all().last().unwrap();
        assert_eq!(topics, vec![&env, symbol_short!("adm_prop").into_val(&env), admin.into_val(&env)]);
        let nominee: Address = data.into_val(&env);
        assert_eq!(nominee, next);

        client.accept_admin();
        let (_, topics, data) = env.events().all().last().unwrap();
        assert_eq!(topics, vec![&env, symbol_short!("adm_acpt").into_val(&env), next.into_val(&env)]);
        let previous: Address = data.into_val(&env);
        assert_eq!(previous, admin);
    }
}
