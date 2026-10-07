#![no_std]
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short,
    token, Address, Env, Vec,
};

// ── Storage keys ──────────────────────────────────────────────────────────────

#[contracttype]
enum DataKey {
    Admin,
    // Address proposed by `propose_admin`, waiting to `accept_admin`.
    PendingAdmin,
    Token,
    StreamCount,
    Stream(u32),
}

// ── Errors ────────────────────────────────────────────────────────────────────

/// Typed errors returned to clients (surfaced as `Error(Contract, #n)`).
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    /// `flow_rate_per_second` is zero or negative.
    InvalidFlowRate = 1,
    /// `end_time` is not in the future.
    InvalidEndTime = 2,
    /// The contract doesn't hold enough tokens to fund the full stream.
    InsufficientBalance = 3,
    /// No stream with this id.
    StreamNotFound = 4,
    /// The stream has ended and been fully withdrawn.
    StreamCompleted = 5,
    /// Nothing has accrued since the last withdrawal.
    NothingToWithdraw = 6,
    /// An amount calculation would overflow i128.
    Overflow = 7,
    /// `accept_admin` / `cancel_admin_transfer` with no transfer pending.
    NoPendingAdmin = 8,
    /// Page `limit` is 0 or larger than `MAX_PAGE_SIZE`.
    InvalidPageSize = 9,
}

/// Largest page `get_streams` will return in one call.
pub const MAX_PAGE_SIZE: u32 = 50;

// ── Public types ─────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone, PartialEq, Eq, Debug)]
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
/// The live balance is `accumulated + elapsed * flow_rate_per_second`, where
/// `elapsed` stops counting at `end_time`.
#[contracttype]
#[derive(Clone, PartialEq, Eq, Debug)]
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

fn admin(env: &Env) -> Address {
    env.storage().instance().get(&DataKey::Admin).unwrap()
}

fn load_stream(env: &Env, stream_id: u32) -> Result<Stream, Error> {
    env.storage()
        .persistent()
        .get(&DataKey::Stream(stream_id))
        .ok_or(Error::StreamNotFound)
}

/// Total owed to the recipient at `now`: the settled amount plus, while
/// Active, accrual from `last_update` up to `min(now, end_time)`. Never
/// accrues past `end_time`, and tolerates `last_update > end_time` (a stream
/// resumed after it ended).
fn owed(stream: &Stream, now: u64) -> Result<i128, Error> {
    match stream.status {
        StreamStatus::Active => {
            let elapsed = now.min(stream.end_time).saturating_sub(stream.last_update);
            let accrued = (elapsed as i128)
                .checked_mul(stream.flow_rate_per_second)
                .ok_or(Error::Overflow)?;
            stream.accumulated.checked_add(accrued).ok_or(Error::Overflow)
        }
        StreamStatus::Paused | StreamStatus::Completed => Ok(stream.accumulated),
    }
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
    ///
    /// Errors: `InvalidFlowRate`, `InvalidEndTime`, `Overflow`, `InsufficientBalance`.
    /// Event: `("s_create", recipient)` → `(id, flow_rate_per_second, end_time)`.
    pub fn create_stream(
        env: Env,
        recipient: Address,
        flow_rate_per_second: i128,
        end_time: u64,
    ) -> Result<u32, Error> {
        admin(&env).require_auth();

        if flow_rate_per_second <= 0 {
            return Err(Error::InvalidFlowRate);
        }
        let now = env.ledger().timestamp();
        if end_time <= now {
            return Err(Error::InvalidEndTime);
        }

        // Verify the contract holds enough USDC for this stream.
        let total_needed = flow_rate_per_second
            .checked_mul((end_time - now) as i128)
            .ok_or(Error::Overflow)?;
        let token_addr: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        let contract_balance = token::Client::new(&env, &token_addr).balance(&env.current_contract_address());
        if contract_balance < total_needed {
            return Err(Error::InsufficientBalance);
        }

        let id = Self::get_stream_count(env.clone());

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

        env.events().publish(
            (symbol_short!("s_create"), recipient),
            (id, flow_rate_per_second, end_time),
        );

        Ok(id)
    }

    /// Admin toggles a stream between Active and Paused.
    ///
    /// On pause: the accrued amount (capped at `end_time`) is snapshotted into
    /// `accumulated` so the live balance stays correct with no external cron.
    /// On resume: `last_update` is set to now so the paused period doesn't accrue.
    ///
    /// Errors: `StreamNotFound`, `StreamCompleted`, `Overflow`.
    /// Event: `("s_toggle", stream_id)` → new `StreamStatus`.
    pub fn toggle_stream(env: Env, stream_id: u32) -> Result<StreamStatus, Error> {
        admin(&env).require_auth();

        let mut stream = load_stream(&env, stream_id)?;
        let now = env.ledger().timestamp();

        match stream.status {
            StreamStatus::Active => {
                stream.accumulated = owed(&stream, now)?;
                stream.last_update = now;
                stream.status = StreamStatus::Paused;
            }
            StreamStatus::Paused => {
                stream.last_update = now;
                stream.status = StreamStatus::Active;
            }
            StreamStatus::Completed => return Err(Error::StreamCompleted),
        }

        env.storage()
            .persistent()
            .set(&DataKey::Stream(stream_id), &stream);

        env.events()
            .publish((symbol_short!("s_toggle"), stream_id), stream.status.clone());

        Ok(stream.status)
    }

    /// Returns the live accumulated USDC (in stroops) for a stream.
    ///
    /// This is a read-only simulation call — no state change, no fee.
    /// The UI calls this every ~5s and uses its own 50ms interpolation tick
    /// between calls for smooth animation.
    pub fn get_accumulated(env: Env, stream_id: u32) -> Result<i128, Error> {
        let stream = load_stream(&env, stream_id)?;
        owed(&stream, env.ledger().timestamp())
    }

    /// Recipient withdraws their accumulated USDC.
    ///
    /// Marks the stream as Completed once `end_time` has passed.
    ///
    /// Errors: `StreamNotFound`, `NothingToWithdraw`, `Overflow`.
    /// Events: `("s_wdraw", stream_id)` → `(recipient, payout)`, then
    /// `("s_done", stream_id)` → `recipient` if the stream just completed.
    pub fn withdraw(env: Env, stream_id: u32) -> Result<i128, Error> {
        let mut stream = load_stream(&env, stream_id)?;
        stream.recipient.require_auth();

        let now = env.ledger().timestamp();
        let payout = owed(&stream, now)?;
        if payout <= 0 {
            return Err(Error::NothingToWithdraw);
        }

        let token_addr: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        token::Client::new(&env, &token_addr).transfer(
            &env.current_contract_address(),
            &stream.recipient,
            &payout,
        );

        stream.accumulated = 0;
        stream.last_update = now;
        let completed = now >= stream.end_time;
        if completed {
            stream.status = StreamStatus::Completed;
        }

        env.storage()
            .persistent()
            .set(&DataKey::Stream(stream_id), &stream);

        env.events()
            .publish((symbol_short!("s_wdraw"), stream_id), (stream.recipient.clone(), payout));
        if completed {
            env.events()
                .publish((symbol_short!("s_done"), stream_id), stream.recipient);
        }

        Ok(payout)
    }

    /// Returns the stored Stream record for a given ID.
    pub fn get_stream(env: Env, stream_id: u32) -> Result<Stream, Error> {
        load_stream(&env, stream_id)
    }

    /// Number of streams ever created (ids are `0..count`).
    pub fn get_stream_count(env: Env) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::StreamCount)
            .unwrap_or(0)
    }

    /// Returns up to `limit` streams with ids `start, start+1, …`.
    /// A `start` past the end returns an empty list.
    ///
    /// Errors: `InvalidPageSize` if `limit` is 0 or above `MAX_PAGE_SIZE`.
    pub fn get_streams(env: Env, start: u32, limit: u32) -> Result<Vec<Stream>, Error> {
        if limit == 0 || limit > MAX_PAGE_SIZE {
            return Err(Error::InvalidPageSize);
        }
        let end = start.saturating_add(limit).min(Self::get_stream_count(env.clone()));
        let mut streams = Vec::new(&env);
        for i in start..end {
            if let Some(s) = env.storage().persistent().get::<DataKey, Stream>(&DataKey::Stream(i)) {
                streams.push_back(s);
            }
        }
        Ok(streams)
    }

    /// Returns all streams.
    ///
    /// Deprecated: cost grows with the number of streams and will eventually
    /// exceed read limits. Use `get_stream_count` + `get_streams` instead.
    pub fn get_all_streams(env: Env) -> Vec<Stream> {
        let count = Self::get_stream_count(env.clone());
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
        admin(&env)
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
    use soroban_sdk::{
        testutils::{Address as _, Events, Ledger},
        token::{Client as TokenClient, StellarAssetClient},
        vec, Env, IntoVal, Symbol,
    };

    const T0: u64 = 1_000_000;
    const RATE: i128 = 1_000;
    const DURATION: u64 = 100;

    struct Setup<'a> {
        client: StreamingContractClient<'a>,
        token: TokenClient<'a>,
        token_admin: StellarAssetClient<'a>,
        admin: Address,
    }

    fn deploy(env: &Env) -> Setup<'_> {
        env.ledger().with_mut(|l| l.timestamp = T0);
        let admin = Address::generate(env);
        let token_id = env.register_stellar_asset_contract_v2(admin.clone()).address();
        let id = env.register(StreamingContract, (admin.clone(), token_id.clone()));
        Setup {
            client: StreamingContractClient::new(env, &id),
            token: TokenClient::new(env, &token_id),
            token_admin: StellarAssetClient::new(env, &token_id),
            admin,
        }
    }

    fn at(env: &Env, t: u64) {
        env.ledger().with_mut(|l| l.timestamp = t);
    }

    /// Funds the contract and creates a RATE/s stream running T0..T0+DURATION.
    fn funded_stream<'a>(env: &'a Env) -> (Setup<'a>, Address, u32) {
        env.mock_all_auths();
        let setup = deploy(env);
        setup.token_admin.mint(&setup.client.address, &(RATE * DURATION as i128));
        let recipient = Address::generate(env);
        let id = setup.client.create_stream(&recipient, &RATE, &(T0 + DURATION));
        (setup, recipient, id)
    }

    #[test]
    fn constructor_sets_admin() {
        let env = Env::default();
        let setup = deploy(&env);
        assert_eq!(setup.client.get_admin(), setup.admin);
        assert_eq!(setup.client.get_pending_admin(), None);
    }

    #[test]
    fn there_is_no_initialize_entrypoint_to_front_run() {
        let env = Env::default();
        let setup = deploy(&env);
        let attacker = Address::generate(&env);
        let res = env.try_invoke_contract::<(), soroban_sdk::Error>(
            &setup.client.address,
            &Symbol::new(&env, "initialize"),
            vec![&env, attacker.clone().into_val(&env), attacker.into_val(&env)],
        );
        assert!(res.is_err());
    }

    // ── create_stream ────────────────────────────────────────────────────────

    #[test]
    fn create_stream_emits_event() {
        let env = Env::default();
        env.mock_all_auths();
        let setup = deploy(&env);
        setup.token_admin.mint(&setup.client.address, &100_000);
        let recipient = Address::generate(&env);

        let id = setup.client.create_stream(&recipient, &RATE, &(T0 + DURATION));
        let (_, topics, data) = env.events().all().last().unwrap();
        assert_eq!(topics, vec![&env, symbol_short!("s_create").into_val(&env), recipient.into_val(&env)]);
        let data: (u32, i128, u64) = data.into_val(&env);
        assert_eq!(data, (id, RATE, T0 + DURATION));
        assert_eq!(setup.client.get_stream_count(), 1);
    }

    #[test]
    fn create_stream_rejects_zero_and_negative_rates() {
        let env = Env::default();
        env.mock_all_auths();
        let setup = deploy(&env);
        let recipient = Address::generate(&env);
        for rate in [0i128, -1, i128::MIN] {
            assert_eq!(
                setup.client.try_create_stream(&recipient, &rate, &(T0 + DURATION)),
                Err(Ok(Error::InvalidFlowRate)),
            );
        }
    }

    #[test]
    fn create_stream_rejects_end_time_now_or_past() {
        let env = Env::default();
        env.mock_all_auths();
        let setup = deploy(&env);
        let recipient = Address::generate(&env);
        for end in [T0, T0 - 1, 0] {
            assert_eq!(
                setup.client.try_create_stream(&recipient, &RATE, &end),
                Err(Ok(Error::InvalidEndTime)),
            );
        }
    }

    #[test]
    fn create_stream_rejects_overflowing_total() {
        let env = Env::default();
        env.mock_all_auths();
        let setup = deploy(&env);
        let recipient = Address::generate(&env);
        // i128::MAX per second for 2 seconds overflows i128.
        assert_eq!(
            setup.client.try_create_stream(&recipient, &i128::MAX, &(T0 + 2)),
            Err(Ok(Error::Overflow)),
        );
        assert_eq!(
            setup.client.try_create_stream(&recipient, &2, &u64::MAX),
            Err(Ok(Error::InsufficientBalance)),
        );
    }

    #[test]
    fn create_stream_requires_full_funding() {
        let env = Env::default();
        env.mock_all_auths();
        let setup = deploy(&env);
        setup.token_admin.mint(&setup.client.address, &(RATE * DURATION as i128 - 1));
        let recipient = Address::generate(&env);

        assert_eq!(
            setup.client.try_create_stream(&recipient, &RATE, &(T0 + DURATION)),
            Err(Ok(Error::InsufficientBalance)),
        );
        setup.token_admin.mint(&setup.client.address, &1);
        setup.client.create_stream(&recipient, &RATE, &(T0 + DURATION));
    }

    #[test]
    fn create_stream_requires_admin_auth() {
        let env = Env::default();
        let setup = deploy(&env);
        let recipient = Address::generate(&env);
        assert!(setup.client.try_create_stream(&recipient, &RATE, &(T0 + DURATION)).is_err());
    }

    // ── Accrual & end-time boundaries ────────────────────────────────────────

    #[test]
    fn accrual_is_linear_and_capped_at_end_time() {
        let env = Env::default();
        let (setup, _, id) = funded_stream(&env);
        let full = RATE * DURATION as i128;

        assert_eq!(setup.client.get_accumulated(&id), 0);
        at(&env, T0 + 1);
        assert_eq!(setup.client.get_accumulated(&id), RATE);
        at(&env, T0 + DURATION - 1);
        assert_eq!(setup.client.get_accumulated(&id), full - RATE);
        at(&env, T0 + DURATION);
        assert_eq!(setup.client.get_accumulated(&id), full);
        at(&env, T0 + DURATION + 1);
        assert_eq!(setup.client.get_accumulated(&id), full);
        at(&env, u64::MAX);
        assert_eq!(setup.client.get_accumulated(&id), full);
    }

    #[test]
    fn withdraw_one_second_before_end_keeps_stream_active() {
        let env = Env::default();
        let (setup, recipient, id) = funded_stream(&env);

        at(&env, T0 + DURATION - 1);
        assert_eq!(setup.client.withdraw(&id), RATE * (DURATION as i128 - 1));
        assert_eq!(setup.client.get_stream(&id).status, StreamStatus::Active);

        at(&env, T0 + DURATION);
        assert_eq!(setup.client.withdraw(&id), RATE);
        assert_eq!(setup.client.get_stream(&id).status, StreamStatus::Completed);
        assert_eq!(setup.token.balance(&recipient), RATE * DURATION as i128);
        assert_eq!(setup.token.balance(&setup.client.address), 0);
    }

    #[test]
    fn withdraw_exactly_at_end_completes_and_emits_events() {
        let env = Env::default();
        let (setup, recipient, id) = funded_stream(&env);

        at(&env, T0 + DURATION);
        let paid = setup.client.withdraw(&id);
        assert_eq!(paid, RATE * DURATION as i128);

        let events = env.events().all();
        let n = events.len();
        let (_, wdraw_topics, wdraw_data) = events.get(n - 2).unwrap();
        assert_eq!(wdraw_topics, vec![&env, symbol_short!("s_wdraw").into_val(&env), id.into_val(&env)]);
        let wdraw: (Address, i128) = wdraw_data.into_val(&env);
        assert_eq!(wdraw, (recipient.clone(), paid));
        let (_, done_topics, _) = events.get(n - 1).unwrap();
        assert_eq!(done_topics, vec![&env, symbol_short!("s_done").into_val(&env), id.into_val(&env)]);
    }

    #[test]
    fn withdraw_long_after_end_pays_only_the_funded_total() {
        let env = Env::default();
        let (setup, recipient, id) = funded_stream(&env);

        at(&env, T0 + DURATION * 1_000);
        assert_eq!(setup.client.withdraw(&id), RATE * DURATION as i128);
        assert_eq!(setup.token.balance(&recipient), RATE * DURATION as i128);
    }

    #[test]
    fn withdraw_after_completion_fails() {
        let env = Env::default();
        let (setup, _, id) = funded_stream(&env);

        at(&env, T0 + DURATION + 5);
        setup.client.withdraw(&id);
        at(&env, T0 + DURATION + 50);
        assert_eq!(setup.client.try_withdraw(&id), Err(Ok(Error::NothingToWithdraw)));
        assert_eq!(setup.client.try_toggle_stream(&id), Err(Ok(Error::StreamCompleted)));
    }

    #[test]
    fn withdraw_with_nothing_accrued_fails() {
        let env = Env::default();
        let (setup, _, id) = funded_stream(&env);
        assert_eq!(setup.client.try_withdraw(&id), Err(Ok(Error::NothingToWithdraw)));
    }

    #[test]
    fn withdraw_requires_recipient_auth() {
        let env = Env::default();
        let (setup, _, id) = funded_stream(&env);
        at(&env, T0 + 10);
        env.set_auths(&[]);
        assert!(setup.client.try_withdraw(&id).is_err());
    }

    // ── Pause / resume ───────────────────────────────────────────────────────

    #[test]
    fn paused_time_does_not_accrue() {
        let env = Env::default();
        let (setup, _, id) = funded_stream(&env);

        at(&env, T0 + 10);
        assert_eq!(setup.client.toggle_stream(&id), StreamStatus::Paused);
        let (_, topics, data) = env.events().all().last().unwrap();
        assert_eq!(topics, vec![&env, symbol_short!("s_toggle").into_val(&env), id.into_val(&env)]);
        let status: StreamStatus = data.into_val(&env);
        assert_eq!(status, StreamStatus::Paused);

        at(&env, T0 + 50);
        assert_eq!(setup.client.get_accumulated(&id), 10 * RATE);
        assert_eq!(setup.client.toggle_stream(&id), StreamStatus::Active);
        at(&env, T0 + 60);
        assert_eq!(setup.client.get_accumulated(&id), 20 * RATE);
    }

    #[test]
    fn pausing_after_end_does_not_over_accrue() {
        let env = Env::default();
        let (setup, _, id) = funded_stream(&env);

        // Regression: pausing used `now - last_update` without capping at
        // end_time, crediting more than the stream was funded for.
        at(&env, T0 + DURATION * 3);
        setup.client.toggle_stream(&id);
        assert_eq!(setup.client.get_accumulated(&id), RATE * DURATION as i128);
        assert_eq!(setup.client.withdraw(&id), RATE * DURATION as i128);
    }

    #[test]
    fn resuming_after_end_does_not_trap_withdraw() {
        let env = Env::default();
        let (setup, recipient, id) = funded_stream(&env);

        at(&env, T0 + 40);
        setup.client.toggle_stream(&id); // pause with 40s accrued
        at(&env, T0 + DURATION + 10);
        setup.client.toggle_stream(&id); // resume: last_update > end_time

        // Regression: `end_time - last_update` underflowed and panicked here.
        assert_eq!(setup.client.get_accumulated(&id), 40 * RATE);
        assert_eq!(setup.client.withdraw(&id), 40 * RATE);
        assert_eq!(setup.token.balance(&recipient), 40 * RATE);
        assert_eq!(setup.client.get_stream(&id).status, StreamStatus::Completed);
    }

    #[test]
    fn unknown_stream_ids_fail() {
        let env = Env::default();
        env.mock_all_auths();
        let setup = deploy(&env);
        assert_eq!(setup.client.try_get_stream(&7), Err(Ok(Error::StreamNotFound)));
        assert_eq!(setup.client.try_get_accumulated(&7), Err(Ok(Error::StreamNotFound)));
        assert_eq!(setup.client.try_withdraw(&7), Err(Ok(Error::StreamNotFound)));
        assert_eq!(setup.client.try_toggle_stream(&7), Err(Ok(Error::StreamNotFound)));
    }

    // ── Pagination ───────────────────────────────────────────────────────────

    #[test]
    fn get_streams_pages_and_validates_limit() {
        let env = Env::default();
        env.mock_all_auths();
        let setup = deploy(&env);
        setup.token_admin.mint(&setup.client.address, &(RATE * DURATION as i128));
        let recipient = Address::generate(&env);
        for _ in 0..3 {
            setup.client.create_stream(&recipient, &1, &(T0 + DURATION));
        }

        assert_eq!(setup.client.get_streams(&0, &2).len(), 2);
        let tail = setup.client.get_streams(&2, &2);
        assert_eq!(tail.len(), 1);
        assert_eq!(tail.get(0).unwrap().id, 2);
        assert_eq!(setup.client.get_streams(&3, &MAX_PAGE_SIZE).len(), 0);
        assert_eq!(setup.client.get_streams(&u32::MAX, &1).len(), 0);
        assert_eq!(setup.client.try_get_streams(&0, &0), Err(Ok(Error::InvalidPageSize)));
        assert_eq!(setup.client.try_get_streams(&0, &(MAX_PAGE_SIZE + 1)), Err(Ok(Error::InvalidPageSize)));
        assert_eq!(setup.client.get_all_streams().len(), 3);
    }

    // ── Two-step admin transfer ──────────────────────────────────────────────

    #[test]
    fn admin_transfer_is_two_step() {
        let env = Env::default();
        env.mock_all_auths();
        let setup = deploy(&env);
        let next = Address::generate(&env);

        setup.client.propose_admin(&next);
        let (_, topics, _) = env.events().all().last().unwrap();
        assert_eq!(topics, vec![&env, symbol_short!("adm_prop").into_val(&env), setup.admin.into_val(&env)]);
        assert_eq!(setup.client.get_admin(), setup.admin);

        setup.client.accept_admin();
        assert_eq!(setup.client.get_admin(), next);
        assert_eq!(setup.client.get_pending_admin(), None);
    }

    #[test]
    fn accept_admin_requires_nominee_auth_and_a_nomination() {
        let env = Env::default();
        let setup = deploy(&env);
        assert_eq!(setup.client.try_accept_admin(), Err(Ok(Error::NoPendingAdmin)));

        env.mock_all_auths();
        setup.client.propose_admin(&Address::generate(&env));
        env.set_auths(&[]);
        assert!(setup.client.try_accept_admin().is_err());
        assert_eq!(setup.client.get_admin(), setup.admin);

        env.mock_all_auths();
        setup.client.cancel_admin_transfer();
        assert_eq!(setup.client.try_accept_admin(), Err(Ok(Error::NoPendingAdmin)));
    }
}
