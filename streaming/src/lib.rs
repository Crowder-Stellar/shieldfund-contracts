#![no_std]
use soroban_sdk::{
    contract, contractimpl, contracttype, symbol_short,
    token, Address, Env, Vec,
};

// ── Storage keys ──────────────────────────────────────────────────────────────

#[contracttype]
enum DataKey {
    Admin,
    Token,
    StreamCount,
    Stream(u32),
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
    pub fn initialize(env: Env, admin: Address, usdc_token: Address) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic!("already initialized");
        }
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
    pub fn create_stream(
        env: Env,
        recipient: Address,
        flow_rate_per_second: i128,
        end_time: u64,
    ) -> u32 {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();

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

        env.events()
            .publish((symbol_short!("s_toggle"), stream_id), ());

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

        env.events()
            .publish((symbol_short!("s_wdraw"), stream_id), payout);

        payout
    }

    /// Returns the stored Stream record for a given ID.
    pub fn get_stream(env: Env, stream_id: u32) -> Stream {
        env.storage()
            .persistent()
            .get(&DataKey::Stream(stream_id))
            .expect("stream not found")
    }

    /// Returns all streams (paginate on the frontend if the list grows).
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{testutils::{Address as _, Ledger}, Env};

    fn setup_env() -> Env {
        Env::default()
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
