use cosmwasm_schema::cw_serde;
use cosmwasm_std::{Addr, Binary, Timestamp, Uint128};
use cw_storage_plus::{Item, Map};

/// WARNING: this contract holds the prize pool. Every field here is storage;
/// changing one after the first mainnet deploy makes existing rounds
/// unreadable. Settle this file before writing anything else.
///
/// One instance per pool — deploy separately for daily and weekly. Separate
/// funds, and no pool branching anywhere in the code.
#[cw_serde]
pub struct Config {
    /// Opens rounds and edits config. Cannot influence an outcome: the seed is
    /// committed before the entries exist, and the entropy comes from minters.
    pub admin: Addr,
    /// The only address allowed to call RecordEntry — the NFT contract.
    pub nft_contract: Addr,
    pub denom: String,
    pub treasury: Addr,
    pub treasury_bps: u64,
    /// Basis points per prize place, first place first.
    /// Daily [8000]; weekly [4800, 2000, 1200].
    pub payout_bps: Vec<u64>,
    /// Paid to whoever calls ExecuteDraw. This is what lets the draw happen
    /// without us — anyone can trigger it and cover their gas.
    pub caller_bps: u64,
    /// Below this many tickets the round is skipped and consumes nothing.
    pub min_entries: u64,
    /// Below this pot the round is skipped too. Zero disables the check.
    pub min_pot: Uint128,
    /// How long past close_time an unrevealed round may be rolled over by
    /// anyone. The valve that keeps funds from being trapped if the master
    /// key is ever lost.
    pub stale_after_secs: u64,
    /// Stops RecordEntry only. Never stops ExecuteDraw or RolloverRound:
    /// pausing must not be able to trap money that is already in.
    pub paused: bool,
}

/// One mint, written by the NFT contract inside the mint transaction, so the
/// entry and the payment either both land or neither does.
#[cw_serde]
pub struct Entry {
    pub token_id: String,
    /// Owner at mint time. The prize goes to whoever holds the token when the
    /// round is settled — resolved then with a single owner_of query.
    pub minter: Addr,
    /// Ticket weight: 1 / 5 / 10.
    pub entries: u32,
    /// Pool share of this mint. Summing these gives the round's pot without
    /// reading the contract balance, which by then also holds money belonging
    /// to the next round.
    pub amount: Uint128,
    /// 32 bytes from the minter's browser. This is what stops the operator —
    /// who knows the secret — from minting until the result points where they
    /// want: they cannot predict what later minters will add.
    pub entropy: Binary,
    pub recorded_at: Timestamp,
}

#[cw_serde]
pub enum RoundStatus {
    /// Committed, taking entries until close_time.
    Open,
    /// Past close_time, waiting for the secret. Derived in queries, never stored.
    Closed,
    /// Settled and paid. Immutable.
    Drawn,
    /// Closed below min_entries or min_pot. Consumes nothing — its entries
    /// belong to the next round instead.
    Skipped,
    /// Nobody revealed within stale_after_secs and someone called
    /// RolloverRound. Also consumes nothing.
    RolledOver,
}

#[cw_serde]
pub struct Round {
    /// sha256(secret), fixed at open. The contract forces a round to be opened
    /// while the previous one is still taking entries, so the commitment is
    /// always older than every entry it governs.
    pub seed_hash: Binary,
    pub opened_at: Timestamp,
    pub close_time: Timestamp,
    pub status: RoundStatus,

    // ── filled in at settlement ──
    /// Ranges are NOT stored at open. A round is settled only after the
    /// previous one, and starts where that one ended. A skipped round leaves
    /// the boundary untouched, so rolling entries over needs no bookkeeping.
    pub first_entry_id: Option<u64>,
    pub last_entry_id: Option<u64>,
    pub secret: Option<Binary>,
    /// sha256 fold over the round's entries, recomputed from storage at
    /// settlement rather than accumulated — nothing to repair after a skip.
    pub entropy: Option<Binary>,
    pub result: Option<Binary>,
    pub total_entries: Option<u64>,
    pub winner_indexes: Vec<u64>,
    pub winners: Vec<Addr>,
    pub pot: Option<Uint128>,
    pub settled_at: Option<Timestamp>,
    /// True when some of the round's entries were recorded before it was
    /// opened — only possible if we let every round close without opening the
    /// next. The commitment is then younger than those entries, so the round
    /// says so out loud instead of hiding it.
    pub has_late_entries: bool,
}

pub const CONFIG: Item<Config> = Item::new("config");

pub const ENTRIES: Map<u64, Entry> = Map::new("entries");
pub const NEXT_ENTRY_ID: Item<u64> = Item::new("next_entry_id");

pub const ROUNDS: Map<u64, Round> = Map::new("rounds");
/// Lowest round id not yet settled. Settlement is strictly in order.
pub const NEXT_UNSETTLED_ID: Item<u64> = Item::new("next_unsettled_id");
/// Highest round id opened so far.
pub const LAST_ROUND_ID: Item<u64> = Item::new("last_round_id");
/// Money left over from settled rounds — the seed remainder plus the pots of
/// rounds that were skipped after their entries had already been consumed.
pub const CARRY: Item<Uint128> = Item::new("carry");
