#![no_std]
use soroban_sdk::{
    contract, contractimpl, contracttype, symbol_short, vec,
    token, Address, BytesN, Env, IntoVal, Symbol,
};

// ── Storage keys ──────────────────────────────────────────────────────────────

#[contracttype]
enum DataKey {
    Admin,
    Token,
    TotalRaised,
    TotalDisbursed,
    ProofRegistry,
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
    /// One-time setup. Reverts if called again.
    pub fn initialize(env: Env, admin: Address, usdc_token: Address) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic!("already initialized");
        }
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
    pub fn deposit(env: Env, depositor: Address, amount: i128) {
        depositor.require_auth();
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
        env.storage().instance().set(&DataKey::ProofRegistry, &registry);
    }

    pub fn get_proof_registry(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::ProofRegistry)
            .expect("proof registry not configured")
    }

    /// Admin-only: disburse USDC to a recipient.
    ///
    /// `proof_hash` must already be registered in ProofRegistry — this is
    /// what makes the disbursement actually ZK-gated rather than just
    /// carrying a hash nobody checks. Panics if the registry hasn't been
    /// configured via `set_proof_registry`, or if the hash isn't found.
    pub fn disburse(
        env: Env,
        recipient: Address,
        amount: i128,
        proof_hash: BytesN<32>,
    ) {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
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

    pub fn get_admin(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Admin).unwrap()
    }

    pub fn get_token(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Token).unwrap()
    }

    /// Transfer admin rights to a new address. Requires current admin auth.
    pub fn transfer_admin(env: Env, new_admin: Address) {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &new_admin);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proof_registry::ProofRegistryContract;
    use soroban_sdk::{
        testutils::Address as _,
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

        let vault_id = env.register(TreasuryVaultContract, ());
        let vault = TreasuryVaultContractClient::new(env, &vault_id);
        vault.initialize(&admin, &token_id);

        let registry_id = env.register(ProofRegistryContract, ());
        let registry = proof_registry::ProofRegistryContractClient::new(env, &registry_id);
        registry.initialize(&admin);

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

    #[test]
    #[should_panic(expected = "proof registry not configured")]
    fn disburse_without_registry_configured_panics() {
        let env = Env::default();
        env.mock_all_auths();
        let setup = setup(&env);
        fund_vault(&env, &setup, 1_000_000);

        let recipient = Address::generate(&env);
        setup.vault.disburse(&recipient, &500_000, &some_hash(&env, 1));
    }

    #[test]
    #[should_panic(expected = "proof_hash not registered in proof_registry")]
    fn disburse_with_unregistered_hash_panics() {
        let env = Env::default();
        env.mock_all_auths();
        let setup = setup(&env);
        fund_vault(&env, &setup, 1_000_000);
        setup.vault.set_proof_registry(&setup.registry.address);

        let recipient = Address::generate(&env);
        setup.vault.disburse(&recipient, &500_000, &some_hash(&env, 1));
    }

    #[test]
    fn disburse_with_registered_proof_succeeds() {
        let env = Env::default();
        env.mock_all_auths();
        let setup = setup(&env);
        fund_vault(&env, &setup, 1_000_000);
        setup.vault.set_proof_registry(&setup.registry.address);

        let hash = some_hash(&env, 1);
        let inputs_hash = some_hash(&env, 2);
        setup.registry.register_proof(
            &setup.admin,
            &hash,
            &inputs_hash,
            &Symbol::new(&env, "payroll"),
        );

        let recipient = Address::generate(&env);
        setup.vault.disburse(&recipient, &500_000, &hash);

        assert_eq!(setup.token.balance(&recipient), 500_000);
        assert_eq!(setup.vault.get_stats().total_disbursed, 500_000);
    }
}
