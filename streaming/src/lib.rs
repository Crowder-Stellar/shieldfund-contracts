#![no_std]
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, symbol_short,
    token, Address, Env, IntoVal, Val, Vec,
};

/// Every way a call can fail. Clients receive these as `Error(Contract, #code)`.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    /// `flow_rate_per_second` must be positive.
    InvalidFlowRate = 1,
    /// `end_time` must be in the future.
    InvalidEndTime = 2,
    /// The stream's total doesn't fit in an i128.
    Overflow = 3,
    /// The unreserved balance can't cover this stream.
    InsufficientBalance = 4,
    /// No stream with this id.
    StreamNotFound = 5,
    /// The stream's `end_time` has passed, so it can't be resumed.
    StreamEnded = 6,
    /// The stream is completed.
    StreamCompleted = 7,
    /// Nothing has accrued since the last withdrawal.
    NothingToWithdraw = 8,
    /// There's no admin handover to accept or cancel.
    NoPendingAdmin = 9,
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
    Token,
    StreamCount,
    Stream(u32),
    // Sum of every stream's outstanding obligation — tokens the contract has
    // already promised and must not promise again.
    Reserved,
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

// ── Accounting ───────────────────────────────────────────────────────────────

/// Tokens accrued since `last_update` and not yet folded into `accumulated`.
/// Never counts time past `end_time`, and nothing accrues while paused.
fn pending_accrual(stream: &Stream, now: u64) -> i128 {
    match stream.status {
        StreamStatus::Active => {
            let elapsed = now.min(stream.end_time).saturating_sub(stream.last_update);
            stream.flow_rate_per_second * elapsed as i128
        }
        StreamStatus::Paused | StreamStatus::Completed => 0,
    }
}

/// The most this stream can still pay out: what it holds plus everything it
/// could accrue until `end_time`. Pausing and withdrawing only lower it.
fn obligation(stream: &Stream) -> i128 {
    match stream.status {
        StreamStatus::Completed => stream.accumulated,
        StreamStatus::Active | StreamStatus::Paused => {
            let remaining = stream.end_time.saturating_sub(stream.last_update);
            stream.accumulated + stream.flow_rate_per_second * remaining as i128
        }
    }
}

fn reserved(env: &Env) -> i128 {
    env.storage().persistent().get(&DataKey::Reserved).unwrap_or(0)
}

fn set_reserved(env: &Env, value: i128) {
    env.storage().persistent().set(&DataKey::Reserved, &value);
    bump(env, &DataKey::Reserved);
}

/// Persists `stream` and moves `Reserved` by the change in its obligation.
fn save(env: &Env, before: i128, stream: &Stream) {
    set_reserved(env, reserved(env) - before + obligation(stream));
    env.storage().persistent().set(&DataKey::Stream(stream.id), stream);
    bump(env, &DataKey::Stream(stream.id));
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
        set_reserved(&env, 0);
        env.events().publish((symbol_short!("init"), admin), usdc_token);
    }

    /// Admin creates a new payment stream.
    ///
    /// `flow_rate_per_second`: token stroops per second (frontend converts
    /// from USDC/month: `round(monthly_usdc * 10_000_000 / 2_592_000)`).
    ///
    /// `end_time`: Unix timestamp when the stream ends.
    ///
    /// The contract's balance must cover this stream's full total *on top of*
    /// what existing streams can still pay out (`get_reserved`). Fund it via a
    /// direct token transfer before calling this.
    pub fn create_stream(
        env: Env,
        recipient: Address,
        flow_rate_per_second: i128,
        end_time: u64,
    ) -> u32 {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
        bump_instance(&env);

        ensure(&env, flow_rate_per_second > 0, Error::InvalidFlowRate);

        let now = env.ledger().timestamp();
        ensure(&env, end_time > now, Error::InvalidEndTime);

        let total = flow_rate_per_second
            .checked_mul((end_time - now) as i128)
            .unwrap_or_else(|| panic_with_error!(&env, Error::Overflow));

        let token_addr: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        let balance = token::Client::new(&env, &token_addr).balance(&env.current_contract_address());
        let committed = reserved(&env)
            .checked_add(total)
            .unwrap_or_else(|| panic_with_error!(&env, Error::Overflow));
        ensure(&env, balance >= committed, Error::InsufficientBalance);

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

        save(&env, 0, &stream);
        env.storage()
            .persistent()
            .set(&DataKey::StreamCount, &(id + 1));
        bump(&env, &DataKey::StreamCount);

        env.events().publish(
            (symbol_short!("s_create"), recipient),
            (id, flow_rate_per_second, end_time),
        );

        id
    }

    /// Admin toggles a stream between Active and Paused.
    ///
    /// On pause: what has accrued (never past `end_time`) is snapshotted into
    /// `accumulated`. On resume: accrual restarts from now; the paused time is
    /// forfeited, not added to the end. A stream can't be resumed once its
    /// `end_time` has passed.
    pub fn toggle_stream(env: Env, stream_id: u32) -> StreamStatus {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
        bump_instance(&env);

        let mut stream: Stream = env
            .storage()
            .persistent()
            .get(&DataKey::Stream(stream_id))
            .unwrap_or_else(|| panic_with_error!(&env, Error::StreamNotFound));
        let before = obligation(&stream);
        let now = env.ledger().timestamp();

        match stream.status {
            StreamStatus::Active => {
                stream.accumulated += pending_accrual(&stream, now);
                stream.last_update = now;
                stream.status = StreamStatus::Paused;
            }
            StreamStatus::Paused => {
                ensure(&env, now < stream.end_time, Error::StreamEnded);
                stream.last_update = now;
                stream.status = StreamStatus::Active;
            }
            StreamStatus::Completed => panic_with_error!(&env, Error::StreamCompleted),
        }

        save(&env, before, &stream);
        let new_status = stream.status.clone();

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
            .unwrap_or_else(|| panic_with_error!(&env, Error::StreamNotFound));
        stream.accumulated + pending_accrual(&stream, env.ledger().timestamp())
    }

    /// Tokens currently promised to streams (their combined outstanding
    /// obligation). New streams must fit in `balance - get_reserved()`.
    pub fn get_reserved(env: Env) -> i128 {
        reserved(&env)
    }

    /// Recipient withdraws their accumulated USDC.
    ///
    /// Marks the stream as Completed once end_time has passed.
    pub fn withdraw(env: Env, stream_id: u32) -> i128 {
        let mut stream: Stream = env
            .storage()
            .persistent()
            .get(&DataKey::Stream(stream_id))
            .unwrap_or_else(|| panic_with_error!(&env, Error::StreamNotFound));

        stream.recipient.require_auth();
        bump_instance(&env);

        let before = obligation(&stream);
        let now = env.ledger().timestamp();
        let payout = stream.accumulated + pending_accrual(&stream, now);
        ensure(&env, payout > 0, Error::NothingToWithdraw);

        let token_addr: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        let token_client = token::Client::new(&env, &token_addr);
        token_client.transfer(
            &env.current_contract_address(),
            &stream.recipient,
            &payout,
        );

        stream.accumulated = 0;
        stream.last_update = now;
        if now >= stream.end_time {
            stream.status = StreamStatus::Completed;
        }
        save(&env, before, &stream);

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
            .unwrap_or_else(|| panic_with_error!(&env, Error::StreamNotFound))
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

    /// Keeps a stream (and the contract instance) alive. Anyone may call
    /// this; it only extends TTLs, it changes no data.
    pub fn extend_ttl(env: Env, stream_id: u32) {
        bump_instance(&env);
        bump(&env, &DataKey::StreamCount);
        ensure(
            &env,
            env.storage().persistent().has(&DataKey::Stream(stream_id)),
            Error::StreamNotFound,
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
    #[should_panic(expected = "Error(Contract, #5)")]
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
    #[should_panic(expected = "Error(Contract, #9)")]
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
    fn clients_get_typed_errors() {
        let env = setup_env();
        let (client, _) = with_stream(&env);
        let r = Address::generate(&env);
        assert_eq!(client.try_create_stream(&r, &0, &(T0 + 1)), Err(Ok(soroban_sdk::Error::from(Error::InvalidFlowRate))));
        assert_eq!(client.try_create_stream(&r, &1, &T0), Err(Ok(soroban_sdk::Error::from(Error::InvalidEndTime))));
        assert_eq!(client.try_withdraw(&0), Err(Ok(soroban_sdk::Error::from(Error::NothingToWithdraw))));
        assert_eq!(client.try_get_stream(&9).err(), Some(Ok(soroban_sdk::Error::from(Error::StreamNotFound))));
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

    const T0: u64 = 1_000_000;

    /// Deploys with a funded contract and a 10 stroop/s stream for 1,000 s.
    fn with_stream(env: &Env) -> (StreamingContractClient<'_>, Address) {
        env.ledger().with_mut(|l| l.timestamp = T0);
        let (client, _) = deploy(env);
        let recipient = Address::generate(env);
        client.create_stream(&recipient, &10, &(T0 + 1_000));
        (client, recipient)
    }

    fn at(env: &Env, t: u64) {
        env.ledger().with_mut(|l| l.timestamp = t);
    }

    #[test]
    fn accrues_linearly_and_stops_at_end_time() {
        let env = setup_env();
        let (client, _) = with_stream(&env);
        at(&env, T0 + 100);
        assert_eq!(client.get_accumulated(&0), 1_000);
        at(&env, T0 + 5_000);
        assert_eq!(client.get_accumulated(&0), 10_000);
    }

    #[test]
    fn withdrawals_pay_exactly_what_accrued_and_complete_the_stream() {
        let env = setup_env();
        let (client, _) = with_stream(&env);
        at(&env, T0 + 250);
        assert_eq!(client.withdraw(&0), 2_500);
        at(&env, T0 + 2_000);
        assert_eq!(client.withdraw(&0), 7_500);
        assert_eq!(client.get_stream(&0).status, StreamStatus::Completed);
        assert_eq!(client.get_reserved(), 0);
    }

    #[test]
    fn pausing_after_end_time_does_not_overpay() {
        let env = setup_env();
        let (client, _) = with_stream(&env);
        at(&env, T0 + 5_000);
        client.toggle_stream(&0);
        assert_eq!(client.get_accumulated(&0), 10_000);
        assert_eq!(client.withdraw(&0), 10_000);
    }

    #[test]
    fn paused_time_does_not_accrue() {
        let env = setup_env();
        let (client, _) = with_stream(&env);
        at(&env, T0 + 100);
        client.toggle_stream(&0); // pause with 1,000 accrued
        at(&env, T0 + 400);
        assert_eq!(client.get_accumulated(&0), 1_000);
        client.toggle_stream(&0); // resume
        at(&env, T0 + 500);
        assert_eq!(client.get_accumulated(&0), 2_000);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #6)")]
    fn resuming_after_end_time_is_rejected() {
        let env = setup_env();
        let (client, _) = with_stream(&env);
        at(&env, T0 + 100);
        client.toggle_stream(&0);
        at(&env, T0 + 2_000);
        client.toggle_stream(&0);
    }

    #[test]
    fn withdrawing_a_paused_stream_after_end_time_completes_it() {
        let env = setup_env();
        let (client, _) = with_stream(&env);
        at(&env, T0 + 100);
        client.toggle_stream(&0);
        at(&env, T0 + 2_000);
        assert_eq!(client.withdraw(&0), 1_000);
        assert_eq!(client.get_stream(&0).status, StreamStatus::Completed);
        assert_eq!(client.get_reserved(), 0);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #8)")]
    fn double_withdraw_in_the_same_second_panics() {
        let env = setup_env();
        let (client, _) = with_stream(&env);
        at(&env, T0 + 10);
        client.withdraw(&0);
        client.withdraw(&0);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #8)")]
    fn withdraw_after_completion_panics() {
        let env = setup_env();
        let (client, _) = with_stream(&env);
        at(&env, T0 + 2_000);
        client.withdraw(&0);
        at(&env, T0 + 3_000);
        client.withdraw(&0);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #7)")]
    fn toggling_a_completed_stream_panics() {
        let env = setup_env();
        let (client, _) = with_stream(&env);
        at(&env, T0 + 2_000);
        client.withdraw(&0);
        client.toggle_stream(&0);
    }

    #[test]
    fn only_the_recipient_can_withdraw() {
        let env = setup_env();
        let (client, recipient) = with_stream(&env);
        at(&env, T0 + 10);
        env.set_auths(&[]);
        assert!(client.try_withdraw(&0).is_err());
        env.mock_all_auths();
        client.withdraw(&0);
        assert_eq!(env.auths()[0].0, recipient);
    }

    #[test]
    fn create_rejects_bad_parameters() {
        let env = setup_env();
        let (client, _) = with_stream(&env);
        let r = Address::generate(&env);
        assert!(client.try_create_stream(&r, &0, &(T0 + 10)).is_err());
        assert!(client.try_create_stream(&r, &-5, &(T0 + 10)).is_err());
        assert!(client.try_create_stream(&r, &1, &T0).is_err());
        assert!(client.try_create_stream(&r, &1, &(T0 - 1)).is_err());
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #3)")]
    fn huge_flow_rates_fail_cleanly() {
        let env = setup_env();
        let (client, _) = with_stream(&env);
        client.create_stream(&Address::generate(&env), &i128::MAX, &(T0 + 10));
    }

    #[test]
    fn streams_cannot_promise_the_same_tokens_twice() {
        let env = setup_env();
        env.ledger().with_mut(|l| l.timestamp = T0);
        let admin = Address::generate(&env);
        let token = env.register_stellar_asset_contract_v2(admin.clone()).address();
        let id = env.register(StreamingContract, (admin, token.clone()));
        let client = StreamingContractClient::new(&env, &id);
        env.mock_all_auths();
        StellarAssetClient::new(&env, &token).mint(&id, &10_000);

        // 10 stroops/s for 1,000 s reserves the whole balance...
        client.create_stream(&Address::generate(&env), &10, &(T0 + 1_000));
        assert_eq!(client.get_reserved(), 10_000);
        // ...so a second stream must not be able to promise it again.
        assert!(client.try_create_stream(&Address::generate(&env), &1, &(T0 + 10)).is_err());

        // Withdrawing frees nothing (it was owed), but pausing and resuming
        // forfeits time, which does free reservation.
        at(&env, T0 + 100);
        client.toggle_stream(&0);
        at(&env, T0 + 300);
        client.toggle_stream(&0);
        assert_eq!(client.get_reserved(), 10_000 - 2_000);
        client.create_stream(&Address::generate(&env), &1, &(T0 + 300 + 2_000));
        assert_eq!(client.get_reserved(), 10_000);
    }
}
