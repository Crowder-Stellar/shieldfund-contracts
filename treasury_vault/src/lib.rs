#![no_std]
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, vec,
    token, Address, Bytes, BytesN, Env, IntoVal, Symbol, U256,
};

// ── Storage keys ──────────────────────────────────────────────────────────────

#[contracttype]
enum DataKey {
    Admin,
    // Address proposed by `propose_admin`, waiting to `accept_admin`.
    PendingAdmin,
    Token,
    TotalRaised,
    TotalDisbursed,
    ProofRegistry,
    // A proof_hash that has already paid out — each proof is single-use.
    SpentProof(BytesN<32>),
}

// ── Errors ────────────────────────────────────────────────────────────────────

/// Typed errors returned to clients (surfaced as `Error(Contract, #n)`).
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    /// `amount` is zero or negative.
    InvalidAmount = 1,
    /// `set_proof_registry` has not been called.
    RegistryNotConfigured = 2,
    /// `proof_hash` is not registered in proof_registry.
    ProofNotRegistered = 3,
    /// This proof has already paid out a disbursement.
    ProofAlreadySpent = 4,
    /// The proof's public inputs don't match this recipient / amount / root / commitment.
    ProofMismatch = 5,
    /// The vault holds less than `amount`.
    InsufficientBalance = 6,
    /// The registered proof_type isn't one the circuit knows.
    UnknownProofType = 7,
    /// The recipient isn't a G... or C... address (e.g. a muxed M... address).
    InvalidRecipient = 8,
    /// A running total would overflow i128.
    Overflow = 9,
    /// `accept_admin` / `cancel_admin_transfer` with no transfer pending.
    NoPendingAdmin = 10,
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
fn proof_type_id(env: &Env, proof_type: &Symbol) -> Result<u32, Error> {
    if *proof_type == Symbol::new(env, "payroll") {
        Ok(0)
    } else if *proof_type == Symbol::new(env, "operational") {
        Ok(1)
    } else if *proof_type == Symbol::new(env, "relief") {
        Ok(2)
    } else {
        Err(Error::UnknownProofType)
    }
}

/// The circuit's `recipient_id` for a Stellar address: keccak256 of the
/// strkey's UTF-8 bytes, reduced into the field. Matches
/// `addressToField()` in shieldfund-proof-server/src/hash.js.
fn recipient_field(env: &Env, recipient: &Address) -> Result<BytesN<32>, Error> {
    let strkey = recipient.to_string();
    let mut buf = [0u8; 56];
    if strkey.len() as usize != buf.len() {
        return Err(Error::InvalidRecipient);
    }
    strkey.copy_into_slice(&mut buf);
    let digest: Bytes = env.crypto().keccak256(&Bytes::from_slice(env, &buf)).into();
    let modulus = U256::from_be_bytes(env, &Bytes::from_array(env, &FIELD_MODULUS));
    Ok(field_bytes(env, &U256::from_be_bytes(env, &digest).rem_euclid(&modulus)))
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
) -> Result<BytesN<32>, Error> {
    // Callers reject amount <= 0 first, so the cast to u128 is lossless.
    let amount = u128::try_from(amount).map_err(|_| Error::InvalidAmount)?;
    let mut data = Bytes::new(env);
    data.append(&merkle_root.clone().into());
    data.append(&budget_commitment.clone().into());
    data.append(&recipient_field(env, recipient)?.into());
    data.append(&field_bytes(env, &U256::from_u128(env, amount)).into());
    data.append(&field_bytes(env, &U256::from_u32(env, proof_type_id)).into());
    Ok(env.crypto().keccak256(&data).into())
}

fn admin(env: &Env) -> Address {
    env.storage().instance().get(&DataKey::Admin).unwrap()
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
    }

    /// Deposit USDC into the vault.
    ///
    /// The depositor must authorise this invocation (Freighter will prompt the
    /// user). Soroban's auth model propagates that authorisation into the
    /// sub-call to `token.transfer`, so no separate `approve` is needed.
    ///
    /// `amount` is in token stroops (7 decimal places: 1 USDC = 10_000_000).
    ///
    /// Errors: `InvalidAmount`, `Overflow`.
    /// Event: `("deposit", depositor)` → `amount`.
    pub fn deposit(env: Env, depositor: Address, amount: i128) -> Result<(), Error> {
        depositor.require_auth();
        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }

        let raised: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::TotalRaised)
            .unwrap_or(0);
        let new_raised = raised.checked_add(amount).ok_or(Error::Overflow)?;

        let token_addr: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        let token_client = token::Client::new(&env, &token_addr);

        // Transfer USDC from depositor → this contract.
        // Because `depositor.require_auth()` was called above, Soroban's auth
        // framework authorises the sub-invocation automatically when the
        // transaction's auth entries are built by `server.prepareTransaction`.
        token_client.transfer(&depositor, &env.current_contract_address(), &amount);

        env.storage()
            .persistent()
            .set(&DataKey::TotalRaised, &new_raised);

        env.events()
            .publish((symbol_short!("deposit"), depositor), amount);
        Ok(())
    }

    /// Admin-only: set (or update) the ProofRegistry contract this vault
    /// checks disbursements against. Must be called before `disburse` will
    /// accept anything, since a freshly-deployed vault has no registry
    /// configured.
    ///
    /// Event: `("reg_set", admin)` → `registry`.
    pub fn set_proof_registry(env: Env, registry: Address) {
        let admin = admin(&env);
        admin.require_auth();
        env.storage().instance().set(&DataKey::ProofRegistry, &registry);
        env.events().publish((symbol_short!("reg_set"), admin), registry);
    }

    pub fn get_proof_registry(env: Env) -> Result<Address, Error> {
        env.storage()
            .instance()
            .get(&DataKey::ProofRegistry)
            .ok_or(Error::RegistryNotConfigured)
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
    ///
    /// Errors: `InvalidAmount`, `RegistryNotConfigured`, `ProofNotRegistered`,
    /// `ProofAlreadySpent`, `UnknownProofType`, `InvalidRecipient`,
    /// `ProofMismatch`, `InsufficientBalance`, `Overflow`.
    /// Event: `("disburse", recipient)` → `(amount, proof_hash)`.
    pub fn disburse(
        env: Env,
        recipient: Address,
        amount: i128,
        proof_hash: BytesN<32>,
        merkle_root: BytesN<32>,
        budget_commitment: BytesN<32>,
    ) -> Result<(), Error> {
        admin(&env).require_auth();
        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }

        let registry = Self::get_proof_registry(env.clone())?;
        let proof_exists: bool = env.invoke_contract(
            &registry,
            &Symbol::new(&env, "verify_proof_exists"),
            vec![&env, proof_hash.into_val(&env)],
        );
        if !proof_exists {
            return Err(Error::ProofNotRegistered);
        }

        let spent_key = DataKey::SpentProof(proof_hash.clone());
        if env.storage().persistent().has(&spent_key) {
            return Err(Error::ProofAlreadySpent);
        }

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
            proof_type_id(&env, &entry.proof_type)?,
        )?;
        if entry.public_inputs_hash != expected {
            return Err(Error::ProofMismatch);
        }

        let token_addr: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        let token_client = token::Client::new(&env, &token_addr);

        // Verify the vault can cover this disbursement.
        let balance = token_client.balance(&env.current_contract_address());
        if balance < amount {
            return Err(Error::InsufficientBalance);
        }

        let disbursed: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::TotalDisbursed)
            .unwrap_or(0);
        let new_disbursed = disbursed.checked_add(amount).ok_or(Error::Overflow)?;

        env.storage().persistent().set(&spent_key, &true);

        // The vault contract is the owner of its own USDC balance, so no
        // extra auth entry is required for this transfer.
        token_client.transfer(&env.current_contract_address(), &recipient, &amount);

        env.storage()
            .persistent()
            .set(&DataKey::TotalDisbursed, &new_disbursed);

        env.events()
            .publish((symbol_short!("disburse"), recipient), (amount, proof_hash));
        Ok(())
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

    /// True if this proof has already paid out a disbursement.
    pub fn is_proof_spent(env: Env, proof_hash: BytesN<32>) -> bool {
        env.storage().persistent().has(&DataKey::SpentProof(proof_hash))
    }

    pub fn get_admin(env: Env) -> Address {
        admin(&env)
    }

    pub fn get_token(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Token).unwrap()
    }

    // ── Two-step admin transfer ──────────────────────────────────────────────

    /// Step 1 (current admin): nominate `new_admin`. Nothing changes until the
    /// nominee calls `accept_admin`, so a mistyped address can't take over.
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
    use proof_registry::ProofRegistryContract;
    use soroban_sdk::{
        testutils::{Address as _, Events},
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
            assert_eq!(recipient_field(&env, &who).unwrap(), hex32(&env, field));
            let pih = |amount: i128, pt: u32| {
                public_inputs_hash(&env, &root(&env), &commitment(&env), &who, amount, pt).unwrap()
            };
            assert_eq!(pih(500_000, 0), hex32(&env, payroll));
            assert_eq!(pih(500_000, 1), hex32(&env, operational));
            assert_eq!(pih(499_999, 0), hex32(&env, smaller));
        }
    }

    fn try_pay(setup: &Setup, env: &Env, amount: i128, hash: &BytesN<32>) -> Result<(), Error> {
        match setup.vault.try_disburse(&recipient(env), &amount, hash, &root(env), &commitment(env)) {
            Ok(_) => Ok(()),
            Err(Ok(e)) => Err(e),
            Err(Err(e)) => panic!("unexpected host error: {e:?}"),
        }
    }

    #[test]
    fn disburse_without_registry_configured_fails() {
        let env = Env::default();
        env.mock_all_auths();
        let setup = setup(&env);
        fund_vault(&env, &setup, 1_000_000);

        assert_eq!(try_pay(&setup, &env, 500_000, &some_hash(&env, 1)), Err(Error::RegistryNotConfigured));
        assert_eq!(setup.vault.try_get_proof_registry(), Err(Ok(Error::RegistryNotConfigured)));
    }

    #[test]
    fn disburse_with_unregistered_hash_fails() {
        let env = Env::default();
        env.mock_all_auths();
        let setup = setup(&env);
        fund_vault(&env, &setup, 1_000_000);
        setup.vault.set_proof_registry(&setup.registry.address);

        assert_eq!(try_pay(&setup, &env, 500_000, &some_hash(&env, 1)), Err(Error::ProofNotRegistered));
    }

    #[test]
    fn disburse_with_matching_proof_succeeds() {
        let env = Env::default();
        env.mock_all_auths();
        let hash = some_hash(&env, 1);
        let setup = ready(&env, &hash, PIH_500000_PAYROLL, "payroll");

        assert!(!setup.vault.is_proof_spent(&hash));
        setup.vault.disburse(&recipient(&env), &500_000, &hash, &root(&env), &commitment(&env));

        // events().all() covers the most recent invocation, so check it first.
        let (_, topics, data) = env.events().all().last().unwrap();
        assert_eq!(topics, vec![&env, symbol_short!("disburse").into_val(&env), recipient(&env).into_val(&env)]);
        let data: (i128, BytesN<32>) = data.into_val(&env);
        assert_eq!(data, (500_000, hash.clone()));

        assert_eq!(setup.token.balance(&recipient(&env)), 500_000);
        assert_eq!(setup.vault.get_stats().total_disbursed, 500_000);
        assert!(setup.vault.is_proof_spent(&hash));
    }

    #[test]
    fn replaying_a_proof_fails() {
        let env = Env::default();
        env.mock_all_auths();
        let hash = some_hash(&env, 1);
        let setup = ready(&env, &hash, PIH_500000_PAYROLL, "payroll");

        setup.vault.disburse(&recipient(&env), &500_000, &hash, &root(&env), &commitment(&env));
        assert_eq!(try_pay(&setup, &env, 500_000, &hash), Err(Error::ProofAlreadySpent));
    }

    #[test]
    fn wrong_amount_fails() {
        let env = Env::default();
        env.mock_all_auths();
        let hash = some_hash(&env, 1);
        let setup = ready(&env, &hash, PIH_499999_PAYROLL, "payroll");

        assert_eq!(try_pay(&setup, &env, 500_000, &hash), Err(Error::ProofMismatch));
    }

    #[test]
    fn wrong_recipient_fails() {
        let env = Env::default();
        env.mock_all_auths();
        let hash = some_hash(&env, 1);
        let setup = ready(&env, &hash, PIH_500000_PAYROLL, "payroll");

        let res = setup.vault.try_disburse(&address(&env, G_RECIPIENT), &500_000, &hash, &root(&env), &commitment(&env));
        assert_eq!(res, Err(Ok(Error::ProofMismatch)));
    }

    #[test]
    fn wrong_proof_type_fails() {
        let env = Env::default();
        env.mock_all_auths();
        let hash = some_hash(&env, 1);
        // Hash was computed for "operational" but the proof is registered as "payroll".
        let setup = ready(&env, &hash, PIH_500000_OPERATIONAL, "payroll");

        assert_eq!(try_pay(&setup, &env, 500_000, &hash), Err(Error::ProofMismatch));
    }

    #[test]
    fn wrong_merkle_root_fails() {
        let env = Env::default();
        env.mock_all_auths();
        let hash = some_hash(&env, 1);
        let setup = ready(&env, &hash, PIH_500000_PAYROLL, "payroll");

        let res = setup.vault.try_disburse(&recipient(&env), &500_000, &hash, &some_hash(&env, 0x33), &commitment(&env));
        assert_eq!(res, Err(Ok(Error::ProofMismatch)));
    }

    #[test]
    fn a_failed_attempt_does_not_burn_the_proof() {
        let env = Env::default();
        env.mock_all_auths();
        let hash = some_hash(&env, 1);
        let setup = ready(&env, &hash, PIH_500000_PAYROLL, "payroll");

        assert!(try_pay(&setup, &env, 499_999, &hash).is_err());
        assert!(!setup.vault.is_proof_spent(&hash));

        setup.vault.disburse(&recipient(&env), &500_000, &hash, &root(&env), &commitment(&env));
        assert_eq!(setup.token.balance(&recipient(&env)), 500_000);
    }

    // ── Amount edge cases ────────────────────────────────────────────────────

    #[test]
    fn deposit_rejects_zero_and_negative_amounts() {
        let env = Env::default();
        env.mock_all_auths();
        let setup = setup(&env);
        let depositor = Address::generate(&env);
        setup.token_admin.mint(&depositor, &100);

        for amount in [0i128, -1, i128::MIN] {
            assert_eq!(setup.vault.try_deposit(&depositor, &amount), Err(Ok(Error::InvalidAmount)));
        }
        assert_eq!(setup.token.balance(&depositor), 100);
        assert_eq!(setup.vault.get_stats().total_raised, 0);
    }

    #[test]
    fn deposit_emits_event_and_tracks_total() {
        let env = Env::default();
        env.mock_all_auths();
        let setup = setup(&env);
        let depositor = Address::generate(&env);
        setup.token_admin.mint(&depositor, &300);

        setup.vault.deposit(&depositor, &100);
        let (_, topics, data) = env.events().all().last().unwrap();
        assert_eq!(topics, vec![&env, symbol_short!("deposit").into_val(&env), depositor.into_val(&env)]);
        let amount: i128 = data.into_val(&env);
        assert_eq!(amount, 100);

        setup.vault.deposit(&depositor, &200);
        assert_eq!(setup.vault.get_stats().total_raised, 300);
        assert_eq!(setup.vault.get_balance(), 300);
    }

    #[test]
    fn deposit_at_i128_max_then_overflow_is_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let setup = setup(&env);
        let whale = Address::generate(&env);
        let other = Address::generate(&env);
        setup.token_admin.mint(&whale, &i128::MAX);
        setup.token_admin.mint(&other, &1);

        setup.vault.deposit(&whale, &i128::MAX);
        assert_eq!(setup.vault.get_stats().total_raised, i128::MAX);

        assert_eq!(setup.vault.try_deposit(&other, &1), Err(Ok(Error::Overflow)));
        assert_eq!(setup.token.balance(&other), 1, "failed deposit must not move funds");
    }

    #[test]
    fn disburse_rejects_zero_and_negative_amounts() {
        let env = Env::default();
        env.mock_all_auths();
        let hash = some_hash(&env, 1);
        let setup = ready(&env, &hash, PIH_500000_PAYROLL, "payroll");

        for amount in [0i128, -500_000, i128::MIN] {
            assert_eq!(try_pay(&setup, &env, amount, &hash), Err(Error::InvalidAmount));
        }
        assert!(!setup.vault.is_proof_spent(&hash));
    }

    #[test]
    fn disburse_more_than_vault_balance_fails_without_burning_proof() {
        let env = Env::default();
        env.mock_all_auths();
        let hash = some_hash(&env, 1);
        let setup = setup(&env);
        fund_vault(&env, &setup, 100_000);
        setup.vault.set_proof_registry(&setup.registry.address);
        setup.registry.register_proof(&setup.admin, &hash, &hex32(&env, PIH_500000_PAYROLL), &Symbol::new(&env, "payroll"));

        assert_eq!(try_pay(&setup, &env, 500_000, &hash), Err(Error::InsufficientBalance));
        assert!(!setup.vault.is_proof_spent(&hash));
    }

    #[test]
    fn disburse_entire_balance_succeeds() {
        let env = Env::default();
        env.mock_all_auths();
        let hash = some_hash(&env, 1);
        let setup = setup(&env);
        fund_vault(&env, &setup, 500_000);
        setup.vault.set_proof_registry(&setup.registry.address);
        setup.registry.register_proof(&setup.admin, &hash, &hex32(&env, PIH_500000_PAYROLL), &Symbol::new(&env, "payroll"));

        setup.vault.disburse(&recipient(&env), &500_000, &hash, &root(&env), &commitment(&env));
        assert_eq!(setup.vault.get_balance(), 0);
    }

    #[test]
    fn disburse_requires_admin_auth() {
        let env = Env::default();
        let hash = some_hash(&env, 1);
        env.mock_all_auths();
        let setup = ready(&env, &hash, PIH_500000_PAYROLL, "payroll");
        env.set_auths(&[]);

        assert!(setup.vault.try_disburse(&recipient(&env), &500_000, &hash, &root(&env), &commitment(&env)).is_err());
        assert!(!setup.vault.is_proof_spent(&hash));
    }

    // ── Registry wiring & admin ──────────────────────────────────────────────

    #[test]
    fn set_proof_registry_emits_event() {
        let env = Env::default();
        env.mock_all_auths();
        let setup = setup(&env);

        setup.vault.set_proof_registry(&setup.registry.address);
        let (_, topics, data) = env.events().all().last().unwrap();
        assert_eq!(topics, vec![&env, symbol_short!("reg_set").into_val(&env), setup.admin.into_val(&env)]);
        let registry: Address = data.into_val(&env);
        assert_eq!(registry, setup.registry.address);
        assert_eq!(setup.vault.get_proof_registry(), setup.registry.address);
    }

    #[test]
    fn admin_transfer_is_two_step() {
        let env = Env::default();
        env.mock_all_auths();
        let setup = setup(&env);
        let next = Address::generate(&env);

        setup.vault.propose_admin(&next);
        assert_eq!(setup.vault.get_admin(), setup.admin);
        assert_eq!(setup.vault.get_pending_admin(), Some(next.clone()));

        setup.vault.accept_admin();
        assert_eq!(setup.vault.get_admin(), next);
        assert_eq!(setup.vault.get_pending_admin(), None);
        assert_eq!(setup.vault.try_accept_admin(), Err(Ok(Error::NoPendingAdmin)));
    }

    #[test]
    fn accept_admin_requires_nominee_auth() {
        let env = Env::default();
        let setup = setup(&env);
        env.mock_all_auths();
        setup.vault.propose_admin(&Address::generate(&env));
        env.set_auths(&[]);

        assert!(setup.vault.try_accept_admin().is_err());
        assert_eq!(setup.vault.get_admin(), setup.admin);
    }

    #[test]
    fn cancel_admin_transfer_clears_nominee() {
        let env = Env::default();
        env.mock_all_auths();
        let setup = setup(&env);

        assert_eq!(setup.vault.try_cancel_admin_transfer(), Err(Ok(Error::NoPendingAdmin)));
        setup.vault.propose_admin(&Address::generate(&env));
        setup.vault.cancel_admin_transfer();
        assert_eq!(setup.vault.get_pending_admin(), None);
        assert_eq!(setup.vault.get_admin(), setup.admin);
    }
}
