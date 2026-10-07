#![no_std]
use soroban_sdk::{
    contract, contractimpl, contracttype, symbol_short,
    Address, BytesN, Env, Symbol, Vec,
};

// ── Storage keys ──────────────────────────────────────────────────────────────

#[contracttype]
enum DataKey {
    Admin,
    ProofCount,
    Proof(u32),
    // Secondary index: proof_hash → proof id, for existence checks.
    HashIndex(BytesN<32>),
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
    pub fn initialize(env: Env, admin: Address) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic!("already initialized");
        }
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

        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        assert!(submitter == admin, "only admin may submit proofs");

        // Guard against duplicate registrations.
        assert!(
            !env.storage()
                .persistent()
                .has(&DataKey::HashIndex(proof_hash.clone())),
            "proof already registered"
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
            .expect("proof not found")
    }

    /// Returns all registered proofs.
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
        env.storage()
            .persistent()
            .has(&DataKey::HashIndex(proof_hash))
    }

    /// Returns the proof ID for a given hash, or panics if not found.
    pub fn get_id_by_hash(env: Env, proof_hash: BytesN<32>) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::HashIndex(proof_hash))
            .expect("proof not found")
    }

    /// Returns the full entry for a given proof hash, or panics if not found.
    /// treasury_vault::disburse() uses this to check the registered
    /// public_inputs_hash and proof_type against the payment it's making.
    pub fn get_proof_by_hash(env: Env, proof_hash: BytesN<32>) -> ProofEntry {
        let id = Self::get_id_by_hash(env.clone(), proof_hash);
        Self::get_proof(env, id)
    }

    pub fn get_admin(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Admin).unwrap()
    }

    pub fn transfer_admin(env: Env, new_admin: Address) {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &new_admin);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{
        testutils::Address as _,
        BytesN, Env, Symbol,
    };

    fn deploy(env: &Env) -> (ProofRegistryContractClient<'_>, Address) {
        let admin = Address::generate(env);
        let contract_id = env.register(ProofRegistryContract, ());
        let client = ProofRegistryContractClient::new(env, &contract_id);
        client.initialize(&admin);
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
    #[should_panic(expected = "proof already registered")]
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
    #[should_panic(expected = "only admin may submit proofs")]
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
    #[should_panic(expected = "proof not found")]
    fn get_proof_by_hash_unknown_panics() {
        let env = Env::default();
        let (client, _) = deploy(&env);
        client.get_proof_by_hash(&one_hash(&env));
    }
}
