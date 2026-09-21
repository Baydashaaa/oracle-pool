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
    /// Automation key. Omitted - the admin acts as operator too.
    #[serde(default)]
    pub operator: Option<String>,
    /// Round 1 is committed here, so entries can never arrive with no
    /// commitment covering them.
    pub first_seed_hash: Binary,
    pub first_close_time: Timestamp,
}

/// Один бесплатный билет: кошелёк, сколько билетов и хеш транзакции, которой
/// он заработан. Хеш служит и ссылкой на причину, и энтропией - он существует
/// раньше, чем билет попадает в контракт, и оператор его не выбирает.
#[cw_serde]
pub struct FreeEntryItem {
    pub wallet: String,
    pub tickets: u32,
    pub tx_hash: String,
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

    /// Admin. Free entries earned off-chain - chat, questions, streaks.
    ///
    /// Refused once the current round has closed. Not flagged, refused: after
    /// close the operator knows the outcome, so adding tickets then would let
    /// them aim it, and the whole commitment would be for nothing.
    ///
    /// These carry no money into the pot. The activities behind them already
    /// fund it - message and question fees go to the same pool - so they are
    /// free to the participant, not to the protocol.
    RecordFreeEntries { entries: Vec<FreeEntryItem> },

    /// Operator. Commits the next round.
    ///
    /// The new round must close after the previous round's reveal deadline
    /// (its close_time + stale_after_secs). That is what makes withholding a
    /// reveal pointless: at the deadline the next round is still open, so the
    /// outcome it would roll into is not knowable yet.
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
    /// Must happen within stale_after_secs of close_time (the reveal window,
    /// frozen into the round at open). After that it is refused and the round
    /// can only be rolled over.
    ///
    /// Earlier versions had no deadline, reasoning that the outcome is fixed
    /// at close. It is - but an open-ended reveal let the secret holder wait
    /// for the next round to close, compute both outcomes and choose (SEC-03).
    /// A CI outage longer than the window now costs a rollover, not money:
    /// the entries and the pot move to the next round intact.
    ExecuteDraw { round_id: u64, secret: Binary },

    /// Kept for compatibility with existing callers; identical to
    /// RolloverRound.
    ///
    /// It used to settle an unrevealed round by a second, public formula. That
    /// handed the secret holder a choice between two known outcomes - reveal,
    /// or wait and take the other one (SEC-03). A stale round now rolls over.
    SettleStale { round_id: u64 },

    /// Permissionless, once the reveal window has closed. Consumes nothing:
    /// the round's entries and their money move to the next round, which
    /// draws with its own secret over everything.
    ///
    /// Drawable rounds are rolled over too. That used to be refused, to stop
    /// anyone voiding a round while the reveal was merely late - but past the
    /// window a reveal is no longer possible, so rolling over is the only way
    /// forward, and it takes nothing from anyone.
    RolloverRound { round_id: u64 },

    /// Admin: any field. Operator: only min_entries, min_pot and paused -
    /// changes that affect future rounds only, since every round freezes its
    /// terms at open.
    UpdateConfig {
        admin: Option<String>,
        operator: Option<String>,
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

/// Every field optional, so `{}` still migrates. On the first migration to
/// this version the operator defaults to the current admin - on the live pools
/// that is the automation key - before `admin` is replaced.
#[cw_serde]
#[derive(Default)]
pub struct MigrateMsg {
    #[serde(default)]
    pub admin: Option<String>,
    #[serde(default)]
    pub operator: Option<String>,
    #[serde(default)]
    pub stale_after_secs: Option<u64>,
}

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
    pub operator: String,
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
