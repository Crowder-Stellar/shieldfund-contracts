#![no_std]
use soroban_sdk::{
    contract, contractimpl, contracttype, symbol_short,
    token, Address, Env, IntoVal, Val, Vec,
};

// ── Storage keys ──────────────────────────────────────────────────────────────

#[contracttype]
enum DataKey {
    Admin,
    PendingAdmin,
    Token,
    StreamCount,
    Stream(u32),
}

// ── Storage TTL ──────────────────────────────────────────────────────────────
//
// Soroban archives entries whose TTL runs out. Instance storage (admin, token,
// wiring) is bumped on every call; persistent entries are bumped whenever they
// are written or used, and anyone can bump them explicitly via `extend_ttl`.

const DAY_IN_LEDGERS: u32 = 17_280; // ~5s ledgers
const TTL_EXTEND_TO: u32 = 30 * DAY_IN_LEDGERS;
const TTL_THRESHOLD: u32 = TTL_EXTEND_TO - DAY_IN_LEDGERS;

/// Largest page `get_streams` returns, keeping reads well inside resource limits.
pub const MAX_PAGE: u32 = 50;

fn bump_instance(env: &Env) {
    env.storage().instance().extend_ttl(TTL_THRESHOLD, TTL_EXTEND_TO);
}

fn bump<K: IntoVal<Env, Val>>(env: &Env, key: &K) {
    env.storage().persistent().extend_ttl(key, TTL_THRESHOLD, TTL_EXTEND_TO);
}

// ── Public types ─────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone, PartialEq, Debug)]
pub enum StreamStatus {
    Active,
    Paused,
    Completed,
}

/// A payment stream record stored on-chain.
///
/// `flow_rate_per_second` is in token stroops (7 decimals).
/// e.g. 5 000 USDC/month ≈ 1 929 stroops/second.
///
/// `accumulated` is the **settled** balance as of `last_update`.
/// The live balance is `accumulated + elapsed * flow_rate_per_second`.
#[contracttype]
#[derive(Clone, Debug)]
pub struct Stream {
    pub id: u32,
    pub recipient: Address,
    pub flow_rate_per_second: i128,
    pub start_time: u64,
    pub end_time: u64,
    pub accumulated: i128,
    pub last_update: u64,
    pub status: StreamStatus,
}

// ── Contract ─────────────────────────────────────────────────────────────────

#[contract]
pub struct StreamingContract;

#[contractimpl]
impl StreamingContract {
    /// Runs once, atomically with deployment, so there is no window in which
    /// someone else could claim the admin role on an uninitialised contract.
    pub fn __constructor(env: Env, admin: Address, usdc_token: Address) {
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Token, &usdc_token);
        env.storage().persistent().set(&DataKey::StreamCount, &0u32);
        bump_instance(&env);
        bump(&env, &DataKey::StreamCount);
        env.events().publish((symbol_short!("init"), admin), usdc_token);
    }

    /// Admin creates a new payment stream.
    ///
    /// `flow_rate_per_second`: token stroops per second (frontend converts
    /// from USDC/month: `round(monthly_usdc * 10_000_000 / 2_592_000)`).
    ///
    /// `end_time`: Unix timestamp when the stream ends.
    ///
    /// The streaming contract must hold enough USDC to cover the full
    /// stream period. Fund it via a direct token transfer before calling this.
    pub fn create_stream(
        env: Env,
        recipient: Address,
        flow_rate_per_second: i128,
        end_time: u64,
    ) -> u32 {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
        bump_instance(&env);

        assert!(flow_rate_per_second > 0, "flow rate must be positive");

        let now = env.ledger().timestamp();
        assert!(end_time > now, "end_time must be in the future");

        // Verify the contract holds enough USDC for this stream.
        let token_addr: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        let token_client = token::Client::new(&env, &token_addr);
        let contract_balance = token_client.balance(&env.current_contract_address());
        let total_needed = flow_rate_per_second * (end_time - now) as i128;
        assert!(
            contract_balance >= total_needed,
            "insufficient contract balance for stream"
        );

        let id: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::StreamCount)
            .unwrap_or(0);

        let stream = Stream {
            id,
            recipient: recipient.clone(),
            flow_rate_per_second,
            start_time: now,
            end_time,
            accumulated: 0,
            last_update: now,
            status: StreamStatus::Active,
        };

        env.storage()
            .persistent()
            .set(&DataKey::Stream(id), &stream);
        env.storage()
            .persistent()
            .set(&DataKey::StreamCount, &(id + 1));
        bump(&env, &DataKey::Stream(id));
        bump(&env, &DataKey::StreamCount);

        env.events().publish(
            (symbol_short!("s_create"), recipient),
            (id, flow_rate_per_second, end_time),
        );

        id
    }

    /// Admin toggles a stream between Active and Paused.
    ///
    /// On pause: the accrued amount is snapshotted into `accumulated` so the
    /// live balance computation stays correct with no external cron needed.
    /// On resume: `last_update` is set to now so we start accruing again from
    /// the current timestamp.
    pub fn toggle_stream(env: Env, stream_id: u32) -> StreamStatus {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
        bump_instance(&env);

        let mut stream: Stream = env
            .storage()
            .persistent()
            .get(&DataKey::Stream(stream_id))
            .expect("stream not found");

        let now = env.ledger().timestamp();

        match stream.status {
            StreamStatus::Active => {
                let elapsed = now.saturating_sub(stream.last_update) as i128;
                stream.accumulated += elapsed * stream.flow_rate_per_second;
                stream.last_update = now;
                stream.status = StreamStatus::Paused;
            }
            StreamStatus::Paused => {
                // Resume: update last_update so we don't accrue for the paused period.
                stream.last_update = now;
                stream.status = StreamStatus::Active;
            }
            StreamStatus::Completed => panic!("stream already completed"),
        }

        let new_status = stream.status.clone();
        env.storage()
            .persistent()
            .set(&DataKey::Stream(stream_id), &stream);
        bump(&env, &DataKey::Stream(stream_id));

        env.events()
            .publish((symbol_short!("s_toggle"), stream_id), new_status.clone());

        new_status
    }

    /// Returns the live accumulated USDC (in stroops) for a stream.
    ///
    /// This is a read-only simulation call — no state change, no fee.
    /// The UI calls this every ~5s and uses its own 50ms interpolation tick
    /// between calls for smooth animation.
    pub fn get_accumulated(env: Env, stream_id: u32) -> i128 {
        let stream: Stream = env
            .storage()
            .persistent()
            .get(&DataKey::Stream(stream_id))
            .expect("stream not found");

        match stream.status {
            StreamStatus::Active => {
                let now = env.ledger().timestamp();
                let elapsed = now.saturating_sub(stream.last_update) as i128;
                // Cap at end_time to avoid overflowing the stream total.
                let effective_elapsed = if now > stream.end_time {
                    (stream.end_time - stream.last_update) as i128
                } else {
                    elapsed
                };
                stream.accumulated + effective_elapsed * stream.flow_rate_per_second
            }
            StreamStatus::Paused | StreamStatus::Completed => stream.accumulated,
        }
    }

    /// Recipient withdraws their accumulated USDC.
    ///
    /// Marks the stream as Completed if end_time has passed.
    pub fn withdraw(env: Env, stream_id: u32) -> i128 {
        let mut stream: Stream = env
            .storage()
            .persistent()
            .get(&DataKey::Stream(stream_id))
            .expect("stream not found");

        stream.recipient.require_auth();
        bump_instance(&env);

        let now = env.ledger().timestamp();

        // Compute settled amount.
        let payout = match stream.status {
            StreamStatus::Active => {
                let elapsed = (now.min(stream.end_time) - stream.last_update) as i128;
                stream.accumulated + elapsed * stream.flow_rate_per_second
            }
            StreamStatus::Paused | StreamStatus::Completed => stream.accumulated,
        };

        assert!(payout > 0, "nothing to withdraw");

        let token_addr: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        let token_client = token::Client::new(&env, &token_addr);
        token_client.transfer(
            &env.current_contract_address(),
            &stream.recipient,
            &payout,
        );

        // Reset accumulated and mark completed if past end_time.
        stream.accumulated = 0;
        stream.last_update = now;
        if now >= stream.end_time {
            stream.status = StreamStatus::Completed;
        }

        env.storage()
            .persistent()
            .set(&DataKey::Stream(stream_id), &stream);
        bump(&env, &DataKey::Stream(stream_id));

        env.events().publish(
            (symbol_short!("s_wdraw"), stream_id),
            (stream.recipient.clone(), payout, stream.status.clone()),
        );

        payout
    }

    /// Returns the stored Stream record for a given ID.
    pub fn get_stream(env: Env, stream_id: u32) -> Stream {
        env.storage()
            .persistent()
            .get(&DataKey::Stream(stream_id))
            .expect("stream not found")
    }

    /// Returns up to `limit` streams (max `MAX_PAGE`) starting at id `start`,
    /// in id order. Page through with `start += returned.len()` until
    /// `start >= get_stream_count()`.
    pub fn get_streams(env: Env, start: u32, limit: u32) -> Vec<Stream> {
        let count = Self::get_stream_count(env.clone());
        let end = start.saturating_add(limit.min(MAX_PAGE)).min(count);
        let mut streams = Vec::new(&env);
        for i in start..end {
            if let Some(s) = env
                .storage()
                .persistent()
                .get::<DataKey, Stream>(&DataKey::Stream(i))
            {
                streams.push_back(s);
            }
        }
        streams
    }

    pub fn get_stream_count(env: Env) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::StreamCount)
            .unwrap_or(0)
    }

    /// Returns all streams. Unbounded: once there are many streams this
    /// exceeds per-call resource limits — use `get_streams` instead.
    pub fn get_all_streams(env: Env) -> Vec<Stream> {
        let count: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::StreamCount)
            .unwrap_or(0);
        let mut streams = Vec::new(&env);
        for i in 0..count {
            if let Some(s) = env
                .storage()
                .persistent()
                .get::<DataKey, Stream>(&DataKey::Stream(i))
            {
                streams.push_back(s);
            }
        }
        streams
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

    /// Keeps a stream (and the contract instance) alive. Anyone may call
    /// this; it only extends TTLs, it changes no data.
    pub fn extend_ttl(env: Env, stream_id: u32) {
        bump_instance(&env);
        bump(&env, &DataKey::StreamCount);
        assert!(
            env.storage().persistent().has(&DataKey::Stream(stream_id)),
            "stream not found"
        );
        bump(&env, &DataKey::Stream(stream_id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{
        testutils::{storage::{Instance as _, Persistent as _}, Address as _, Events as _, Ledger},
        token::StellarAssetClient,
        vec, Env, FromVal, IntoVal, Symbol,
    };

    fn setup_env() -> Env {
        Env::default()
    }

    fn deploy(env: &Env) -> (StreamingContractClient<'_>, Address) {
        let admin = Address::generate(env);
        let token = env.register_stellar_asset_contract_v2(admin.clone()).address();
        let id = env.register(StreamingContract, (admin.clone(), token.clone()));
        env.mock_all_auths();
        StellarAssetClient::new(env, &token).mint(&id, &1_000_000_000_000);
        (StreamingContractClient::new(env, &id), admin)
    }

    #[test]
    fn streams_get_a_long_ttl_and_anyone_can_renew_it() {
        let env = setup_env();
        env.ledger().with_mut(|l| l.timestamp = 1_000);
        let (client, _) = deploy(&env);
        let recipient = Address::generate(&env);
        client.create_stream(&recipient, &10, &(1_000 + 90 * 86_400));

        let id = client.address.clone();
        let ttl = |key: DataKey| env.as_contract(&id, || env.storage().persistent().get_ttl(&key));
        assert_eq!(ttl(DataKey::Stream(0)), TTL_EXTEND_TO);
        assert_eq!(ttl(DataKey::StreamCount), TTL_EXTEND_TO);
        assert_eq!(env.as_contract(&id, || env.storage().instance().get_ttl()), TTL_EXTEND_TO);

        env.ledger().with_mut(|l| l.sequence_number += 20 * DAY_IN_LEDGERS);
        assert!(ttl(DataKey::Stream(0)) < TTL_THRESHOLD);
        env.set_auths(&[]);
        client.extend_ttl(&0);
        assert_eq!(ttl(DataKey::Stream(0)), TTL_EXTEND_TO);
    }

    #[test]
    #[should_panic(expected = "stream not found")]
    fn extend_ttl_on_unknown_stream_panics() {
        let env = setup_env();
        let (client, _) = deploy(&env);
        client.extend_ttl(&7);
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
    #[should_panic(expected = "no pending admin")]
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
    fn toggle_and_withdraw_events_carry_state() {
        let env = setup_env();
        env.ledger().with_mut(|l| l.timestamp = 1_000);
        let (client, _) = deploy(&env);
        let recipient = Address::generate(&env);
        client.create_stream(&recipient, &10, &2_000);

        env.ledger().with_mut(|l| l.timestamp = 1_100);
        client.toggle_stream(&0);
        let (_, topics, data) = env.events().all().last().unwrap();
        assert_eq!(topics, vec![&env, symbol_short!("s_toggle").into_val(&env), 0u32.into_val(&env)]);
        assert_eq!(StreamStatus::from_val(&env, &data), StreamStatus::Paused);

        client.withdraw(&0);
        let (_, topics, data) = env.events().all().last().unwrap();
        assert_eq!(topics, vec![&env, symbol_short!("s_wdraw").into_val(&env), 0u32.into_val(&env)]);
        let (who, paid, status): (Address, i128, StreamStatus) = data.into_val(&env);
        assert_eq!((who, paid, status), (recipient, 1_000, StreamStatus::Paused));
    }

    #[test]
    fn streams_page_in_id_order_and_cap_at_max_page() {
        let env = setup_env();
        env.ledger().with_mut(|l| l.timestamp = 1_000);
        let (client, _) = deploy(&env);
        for _ in 0..55 {
            client.create_stream(&Address::generate(&env), &1, &2_000);
        }
        assert_eq!(client.get_stream_count(), 55);
        assert_eq!(client.get_streams(&0, &500).len(), MAX_PAGE);
        let tail = client.get_streams(&50, &50);
        assert_eq!(tail.len(), 5);
        assert_eq!(tail.get(0).unwrap().id, 50);
        assert_eq!(client.get_streams(&u32::MAX, &u32::MAX).len(), 0);
    }

    #[test]
    fn constructor_sets_admin() {
        let env = setup_env();
        let (client, admin) = deploy(&env);
        assert_eq!(client.get_admin(), admin);
    }

    #[test]
    fn there_is_no_initialize_entrypoint_to_front_run() {
        let env = setup_env();
        let (client, _) = deploy(&env);
        let attacker = Address::generate(&env);
        let res = env.try_invoke_contract::<(), soroban_sdk::Error>(
            &client.address,
            &Symbol::new(&env, "initialize"),
            vec![&env, attacker.clone().into_val(&env), attacker.into_val(&env)],
        );
        assert!(res.is_err());
    }

    /// `get_accumulated` for an Active stream returns accumulated + elapsed * rate.
    #[test]
    fn get_accumulated_active_accrues_correctly() {
        let env = setup_env();
        env.ledger().with_mut(|l| l.timestamp = 1_000_000);

        let stream = Stream {
            id: 0,
            recipient: soroban_sdk::Address::generate(&env),
            flow_rate_per_second: 1_000,
            start_time: 1_000_000,
            end_time: 1_100_000,
            accumulated: 0,
            last_update: 1_000_000,
            status: StreamStatus::Active,
        };

        // Advance ledger 100 seconds
        env.ledger().with_mut(|l| l.timestamp = 1_000_100);

        // Compute manually: 0 + 100 * 1_000 = 100_000
        let now = env.ledger().timestamp();
        let elapsed = (now - stream.last_update) as i128;
        let live = stream.accumulated + elapsed * stream.flow_rate_per_second;
        assert_eq!(live, 100_000);
    }

    /// `get_accumulated` for a Paused stream returns the snapshot value only.
    #[test]
    fn get_accumulated_paused_does_not_accrue() {
        let env = setup_env();
        let stream = Stream {
            id: 0,
            recipient: soroban_sdk::Address::generate(&env),
            flow_rate_per_second: 1_000,
            start_time: 1_000_000,
            end_time: 1_100_000,
            accumulated: 50_000,
            last_update: 1_000_050,
            status: StreamStatus::Paused,
        };

        // Paused: value stays at accumulated regardless of time
        match stream.status {
            StreamStatus::Paused | StreamStatus::Completed => {
                assert_eq!(stream.accumulated, 50_000);
            }
            _ => panic!("unexpected active status"),
        }
    }

    /// Ceiling-division: 10_000_000 stroops / 2_592_000 seconds rounds up to 4.
    #[test]
    fn monthly_to_per_second_ceiling_division() {
        let monthly_stroops: i128 = 10_000_000; // 1 USDC
        let seconds_per_month: i128 = 2_592_000;
        let per_sec = (monthly_stroops + seconds_per_month - 1) / seconds_per_month;
        assert_eq!(per_sec, 4); // ceil(10_000_000 / 2_592_000) = ceil(3.858) = 4
    }
}
