use cosmwasm_schema::{cw_serde, QueryResponses};
use cosmwasm_std::{Binary, Timestamp, Uint128};

use crate::state::{Entry, RoundStatus};

#[cw_serde]
pub struct InstantiateMsg {
    pub admin: String,
    pub nft_contract: String,
    pub denom: String,
    pub treasury: String,
    pub treasury_bps: u64,
    pub payout_bps: Vec<u64>,
    pub caller_bps: u64,
    pub min_entries: u64,
    pub min_pot: Uint128,
    pub stale_after_secs: u64,
    /// Round 1 is committed here, so entries can never arrive with no
    /// commitment covering them.
    pub first_seed_hash: Binary,
    pub first_close_time: Timestamp,
}

#[cw_serde]
pub enum ExecuteMsg {
    /// Called by the NFT contract inside the mint transaction. Not payable:
    /// the pool's share arrives as a plain BankMsg in the same tx, and a bank
    /// transfer runs no code here — `amount` is what ties that money to this
    /// entry.
    RecordEntry {
        token_id: String,
        minter: String,
        entries: u32,
        amount: Uint128,
        /// 32 bytes generated in the minter's browser.
        entropy: Binary,
    },

    /// Admin. Commits the next round.
    ///
    /// Meant to be called while the current round is still open — that is what
    /// keeps the commitment older than the entries. It is still allowed
    /// afterwards, because refusing would deadlock the contract, but any round
    /// that ends up covering earlier entries is flagged has_late_entries.
    OpenRound {
        seed_hash: Binary,
        close_time: Timestamp,
    },

    /// Permissionless. Anyone holding the secret settles a closed round and
    /// takes caller_bps for their gas.
    ///
    ///   entropy = fold sha256(acc || minter || entry_entropy) over the round
    ///   result  = sha256(secret || entropy || round_id)
    ///   index   = u128(result[0..16]) % total_entries
    ///
    /// There is no reveal deadline on purpose: the outcome is fixed the moment
    /// the round closes, so a late reveal changes nothing, while a deadline
    /// would turn a CI outage into a lost round.
    ExecuteDraw { round_id: u64, secret: Binary },

    /// Permissionless, only after close_time + stale_after_secs with no
    /// reveal. Consumes nothing: the entries belong to the next round instead.
    RolloverRound { round_id: u64 },

    UpdateConfig {
        admin: Option<String>,
        nft_contract: Option<String>,
        treasury: Option<String>,
        treasury_bps: Option<u64>,
        payout_bps: Option<Vec<u64>>,
        caller_bps: Option<u64>,
        min_entries: Option<u64>,
        min_pot: Option<Uint128>,
        stale_after_secs: Option<u64>,
        paused: Option<bool>,
    },
}

#[cw_serde]
pub struct MigrateMsg {}

#[cw_serde]
#[derive(QueryResponses)]
pub enum QueryMsg {
    #[returns(ConfigResponse)]
    Config {},
    #[returns(RoundResponse)]
    Round { round_id: u64 },
    #[returns(RoundsResponse)]
    Rounds { start_after: Option<u64>, limit: Option<u32> },
    /// The round taking entries right now.
    #[returns(RoundResponse)]
    CurrentRound {},
    /// Entries in id order — the order the ticket array is built in.
    #[returns(EntriesResponse)]
    Entries { start_after: Option<u64>, limit: Option<u32> },
    /// Everything needed to recheck a settled round without trusting us.
    #[returns(ProofResponse)]
    Proof { round_id: u64 },
    #[returns(PotResponse)]
    Pot {},
}

#[cw_serde]
pub struct ConfigResponse {
    pub admin: String,
    pub nft_contract: String,
    pub denom: String,
    pub treasury: String,
    pub treasury_bps: u64,
    pub payout_bps: Vec<u64>,
    pub caller_bps: u64,
    pub min_entries: u64,
    pub min_pot: Uint128,
    pub stale_after_secs: u64,
    pub paused: bool,
    pub next_unsettled_id: u64,
    pub last_round_id: u64,
    pub next_entry_id: u64,
    pub carry: Uint128,
}

#[cw_serde]
pub struct RoundResponse {
    pub round_id: u64,
    pub seed_hash: Binary,
    pub opened_at: Timestamp,
    pub close_time: Timestamp,
    pub status: RoundStatus,
    pub first_entry_id: Option<u64>,
    pub last_entry_id: Option<u64>,
    pub secret: Option<Binary>,
    pub entropy: Option<Binary>,
    pub result: Option<Binary>,
    pub total_entries: Option<u64>,
    pub winner_indexes: Vec<u64>,
    pub winners: Vec<String>,
    pub pot: Option<Uint128>,
    pub settled_at: Option<Timestamp>,
    pub has_late_entries: bool,
}

#[cw_serde]
pub struct RoundsResponse {
    pub rounds: Vec<RoundResponse>,
}

#[cw_serde]
pub struct EntryResponse {
    pub entry_id: u64,
    pub entry: Entry,
}

#[cw_serde]
pub struct EntriesResponse {
    pub entries: Vec<EntryResponse>,
}

#[cw_serde]
pub struct ProofResponse {
    pub round_id: u64,
    pub seed_hash: Binary,
    pub secret: Option<Binary>,
    pub entropy: Option<Binary>,
    pub result: Option<Binary>,
    pub total_entries: Option<u64>,
    pub winner_indexes: Vec<u64>,
    pub winners: Vec<String>,
    /// The round's entries in order, so the ticket array can be rebuilt
    /// exactly as the contract built it.
    pub entries: Vec<EntryResponse>,
}

#[cw_serde]
pub struct PotResponse {
    pub denom: String,
    /// Contract balance, including money that belongs to future rounds.
    pub balance: Uint128,
    /// Carried over from settled rounds.
    pub carry: Uint128,
    /// Sum of entry amounts not yet consumed by a settlement.
    pub pending: Uint128,
}
