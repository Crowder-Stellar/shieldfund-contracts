#![no_std]
use soroban_sdk::{
    contract, contractimpl, contracttype, symbol_short,
    token, Address, BytesN, Env,
};

// ── Storage keys ──────────────────────────────────────────────────────────────

#[contracttype]
enum DataKey {
    Admin,
    Token,
    TotalRaised,
    TotalDisbursed,
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

    /// Admin-only: disburse USDC to a recipient.
    ///
    /// `proof_hash` is the 32-byte Noir proof hash anchored in ProofRegistry.
    /// Including it here creates an on-chain audit link between disbursement
    /// and its ZK justification.
    pub fn disburse(
        env: Env,
        recipient: Address,
        amount: i128,
        proof_hash: BytesN<32>,
    ) {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
        assert!(amount > 0, "amount must be positive");

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
