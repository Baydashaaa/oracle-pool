#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{
    coins, to_json_binary, Addr, BankMsg, Binary, Deps, DepsMut, Env, MessageInfo, Order, Response,
    StdError, StdResult, Storage, Timestamp, Uint128, WasmQuery, QueryRequest,
};
use cw_storage_plus::Bound;
use sha2::{Digest, Sha256};

use crate::error::ContractError;
use crate::msg::{
    ConfigResponse, EntriesResponse, EntryResponse, ExecuteMsg, InstantiateMsg, MigrateMsg,
    PotResponse, ProofResponse, QueryMsg, RoundResponse, RoundsResponse,
};
use crate::state::{
    Config, Entry, Round, RoundStatus, CARRY, CONFIG, ENTRIES, LAST_ROUND_ID, NEXT_ENTRY_ID,
    NEXT_UNSETTLED_ID, ROUNDS,
};

const CONTRACT_NAME: &str = "crates.io:oracle-pool";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

const MAX_ENTROPY_BYTES: usize = 256;
const DEFAULT_LIMIT: u32 = 30;
const MAX_LIMIT: u32 = 100;
/// Guard on the per-settlement scan. Rounds are days long, not years.
const MAX_SCAN: usize = 5_000;

// ── helpers ─────────────────────────────────────────────────────────────────

fn sha256(parts: &[&[u8]]) -> Binary {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    Binary::from(h.finalize().as_slice())
}

fn bps(amount: Uint128, bps: u64) -> Uint128 {
    amount.multiply_ratio(bps as u128, 10_000u128)
}

fn check_bps(cfg: &Config) -> Result<(), ContractError> {
    let total: u64 = cfg.payout_bps.iter().sum::<u64>() + cfg.treasury_bps + cfg.caller_bps;
    if total > 10_000 {
        return Err(ContractError::InvalidConfig {
            reason: format!("payout + treasury + caller = {total} bps, must be <= 10000"),
        });
    }
    if cfg.payout_bps.is_empty() {
        return Err(ContractError::InvalidConfig {
            reason: "payout_bps must have at least one place".into(),
        });
    }
    Ok(())
}

/// cw721 owner_of, trimmed to the one field we need.
#[cosmwasm_schema::cw_serde]
enum Cw721Query {
    OwnerOf {
        token_id: String,
        include_expired: Option<bool>,
    },
}

#[cosmwasm_schema::cw_serde]
struct OwnerOfResponse {
    owner: String,
}

fn token_owner(deps: Deps, nft: &Addr, token_id: &str) -> StdResult<Addr> {
    let res: OwnerOfResponse = deps.querier.query(&QueryRequest::Wasm(WasmQuery::Smart {
        contract_addr: nft.to_string(),
        msg: to_json_binary(&Cw721Query::OwnerOf {
            token_id: token_id.to_string(),
            include_expired: Some(false),
        })?,
    }))?;
    deps.api.addr_validate(&res.owner)
}

fn load_round(store: &dyn Storage, round_id: u64) -> Result<Round, ContractError> {
    ROUNDS
        .may_load(store, round_id)?
        .ok_or(ContractError::RoundNotFound { round_id })
}

/// The first entry id belonging to `round_id`. Derived from the previous
/// round, which the caller has already checked is settled — that is why a
/// skipped round rolls its entries over for free: it simply leaves the
/// boundary where it was.
fn first_entry_of(store: &dyn Storage, round_id: u64) -> Result<u64, ContractError> {
    if round_id <= 1 {
        return Ok(1);
    }
    let prev = load_round(store, round_id - 1)?;
    Ok(prev.last_entry_id.unwrap_or(0) + 1)
}

struct Scan {
    first_entry_id: u64,
    last_entry_id: u64,
    total_entries: u64,
    amount: Uint128,
    entropy: Binary,
    has_late_entries: bool,
}

/// Walk the round's entries once: count tickets, sum the pot, fold the
/// entropy. Recomputed from storage instead of accumulated, so a skip or a
/// rollover needs no repair.
fn scan_round(
    store: &dyn Storage,
    round_id: u64,
    round: &Round,
) -> Result<Scan, ContractError> {
    let first = first_entry_of(store, round_id)?;
    let mut last = first - 1;
    let mut total: u64 = 0;
    let mut amount = Uint128::zero();
    let mut entropy = sha256(&[&round_id.to_be_bytes(), round.seed_hash.as_slice()]);
    let mut late = false;

    for item in ENTRIES
        .range(store, Some(Bound::inclusive(first)), None, Order::Ascending)
        .take(MAX_SCAN)
    {
        let (id, e) = item?;
        if e.recorded_at >= round.close_time {
            break;
        }
        if e.recorded_at < round.opened_at {
            late = true;
        }
        entropy = sha256(&[entropy.as_slice(), e.minter.as_bytes(), e.entropy.as_slice()]);
        total += e.entries as u64;
        amount += e.amount;
        last = id;
    }

    Ok(Scan {
        first_entry_id: first,
        last_entry_id: last,
        total_entries: total,
        amount,
        entropy,
        has_late_entries: late,
    })
}

/// Which entry owns ticket position `index`.
fn entry_at_index(
    store: &dyn Storage,
    first: u64,
    last: u64,
    index: u64,
) -> Result<(u64, Entry), ContractError> {
    let mut seen: u64 = 0;
    for item in ENTRIES.range(
        store,
        Some(Bound::inclusive(first)),
        Some(Bound::inclusive(last)),
        Order::Ascending,
    ) {
        let (id, e) = item?;
        let w = e.entries as u64;
        if index < seen + w {
            return Ok((id, e));
        }
        seen += w;
    }
    Err(ContractError::Std(StdError::generic_err(
        "ticket index out of range",
    )))
}

fn derived_status(round: &Round, now: Timestamp) -> RoundStatus {
    match round.status {
        RoundStatus::Open if now >= round.close_time => RoundStatus::Closed,
        ref other => other.clone(),
    }
}

// ── entry points ────────────────────────────────────────────────────────────

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    env: Env,
    _info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response, ContractError> {
    cw2::set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

    if msg.first_seed_hash.len() != 32 {
        return Err(ContractError::BadSeedHash {
            got: msg.first_seed_hash.len(),
        });
    }
    if msg.first_close_time <= env.block.time {
        return Err(ContractError::CloseTimeInPast {});
    }
    if msg.denom.is_empty() {
        return Err(ContractError::InvalidConfig {
            reason: "denom must not be empty".into(),
        });
    }

    let cfg = Config {
        admin: deps.api.addr_validate(&msg.admin)?,
        nft_contract: deps.api.addr_validate(&msg.nft_contract)?,
        denom: msg.denom,
        treasury: deps.api.addr_validate(&msg.treasury)?,
        treasury_bps: msg.treasury_bps,
        payout_bps: msg.payout_bps,
        caller_bps: msg.caller_bps,
        min_entries: msg.min_entries,
        min_pot: msg.min_pot,
        stale_after_secs: msg.stale_after_secs,
        paused: false,
    };
    check_bps(&cfg)?;
    CONFIG.save(deps.storage, &cfg)?;

    ROUNDS.save(
        deps.storage,
        1,
        &Round {
            seed_hash: msg.first_seed_hash,
            opened_at: env.block.time,
            close_time: msg.first_close_time,
            status: RoundStatus::Open,
            first_entry_id: None,
            last_entry_id: None,
            secret: None,
            entropy: None,
            result: None,
            total_entries: None,
            winner_indexes: vec![],
            winners: vec![],
            pot: None,
            settled_at: None,
            has_late_entries: false,
        },
    )?;
    NEXT_ENTRY_ID.save(deps.storage, &1u64)?;
    NEXT_UNSETTLED_ID.save(deps.storage, &1u64)?;
    LAST_ROUND_ID.save(deps.storage, &1u64)?;
    CARRY.save(deps.storage, &Uint128::zero())?;

    Ok(Response::new()
        .add_attribute("action", "instantiate")
        .add_attribute("admin", cfg.admin)
        .add_attribute("first_close_time", msg.first_close_time.seconds().to_string()))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn migrate(deps: DepsMut, _env: Env, _msg: MigrateMsg) -> Result<Response, ContractError> {
    cw2::set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
    Ok(Response::new().add_attribute("action", "migrate"))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    match msg {
        ExecuteMsg::RecordEntry {
            token_id,
            minter,
            entries,
            amount,
            entropy,
        } => record_entry(deps, env, info, token_id, minter, entries, amount, entropy),
        ExecuteMsg::OpenRound {
            seed_hash,
            close_time,
        } => open_round(deps, env, info, seed_hash, close_time),
        ExecuteMsg::ExecuteDraw { round_id, secret } => {
            execute_draw(deps, env, info, round_id, secret)
        }
        ExecuteMsg::RolloverRound { round_id } => rollover_round(deps, env, round_id),
        ExecuteMsg::UpdateConfig { .. } => update_config(deps, info, msg),
    }
}

#[allow(clippy::too_many_arguments)]
fn record_entry(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    token_id: String,
    minter: String,
    entries: u32,
    amount: Uint128,
    entropy: Binary,
) -> Result<Response, ContractError> {
    let cfg = CONFIG.load(deps.storage)?;
    if info.sender != cfg.nft_contract {
        return Err(ContractError::Unauthorized {});
    }
    if cfg.paused {
        return Err(ContractError::Paused {});
    }
    if entries == 0 {
        return Err(ContractError::ZeroEntries {});
    }
    if entropy.is_empty() || entropy.len() > MAX_ENTROPY_BYTES {
        return Err(ContractError::BadEntropy {
            max: MAX_ENTROPY_BYTES,
        });
    }

    let id = NEXT_ENTRY_ID.load(deps.storage)?;
    ENTRIES.save(
        deps.storage,
        id,
        &Entry {
            token_id: token_id.clone(),
            minter: deps.api.addr_validate(&minter)?,
            entries,
            amount,
            entropy,
            recorded_at: env.block.time,
        },
    )?;
    NEXT_ENTRY_ID.save(deps.storage, &(id + 1))?;

    Ok(Response::new()
        .add_attribute("action", "record_entry")
        .add_attribute("entry_id", id.to_string())
        .add_attribute("token_id", token_id)
        .add_attribute("entries", entries.to_string())
        .add_attribute("amount", amount))
}

fn open_round(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    seed_hash: Binary,
    close_time: Timestamp,
) -> Result<Response, ContractError> {
    let cfg = CONFIG.load(deps.storage)?;
    if info.sender != cfg.admin {
        return Err(ContractError::Unauthorized {});
    }
    if seed_hash.len() != 32 {
        return Err(ContractError::BadSeedHash {
            got: seed_hash.len(),
        });
    }
    if close_time <= env.block.time {
        return Err(ContractError::CloseTimeInPast {});
    }

    let last_id = LAST_ROUND_ID.load(deps.storage)?;
    let last = load_round(deps.storage, last_id)?;
    if close_time <= last.close_time {
        return Err(ContractError::CloseTimeNotAfterPrevious {
            previous: last.close_time.seconds(),
        });
    }

    let id = last_id + 1;
    ROUNDS.save(
        deps.storage,
        id,
        &Round {
            seed_hash,
            opened_at: env.block.time,
            close_time,
            status: RoundStatus::Open,
            first_entry_id: None,
            last_entry_id: None,
            secret: None,
            entropy: None,
            result: None,
            total_entries: None,
            winner_indexes: vec![],
            winners: vec![],
            pot: None,
            settled_at: None,
            has_late_entries: false,
        },
    )?;
    LAST_ROUND_ID.save(deps.storage, &id)?;

    // Opening late is allowed — refusing would deadlock the contract — but the
    // previous round having already closed means this one may cover entries
    // older than its own commitment. scan_round() flags that at settlement.
    let late = env.block.time >= last.close_time;

    Ok(Response::new()
        .add_attribute("action", "open_round")
        .add_attribute("round_id", id.to_string())
        .add_attribute("close_time", close_time.seconds().to_string())
        .add_attribute("opened_after_previous_closed", late.to_string()))
}

fn check_settleable(
    store: &dyn Storage,
    env: &Env,
    round_id: u64,
) -> Result<Round, ContractError> {
    let next = NEXT_UNSETTLED_ID.load(store)?;
    if round_id < next {
        return Err(ContractError::AlreadySettled { round_id });
    }
    if round_id > next {
        return Err(ContractError::OutOfOrder { expected: next });
    }
    let round = load_round(store, round_id)?;
    if env.block.time < round.close_time {
        return Err(ContractError::NotClosed { round_id });
    }
    Ok(round)
}

fn execute_draw(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    round_id: u64,
    secret: Binary,
) -> Result<Response, ContractError> {
    let cfg = CONFIG.load(deps.storage)?;
    let mut round = check_settleable(deps.storage, &env, round_id)?;

    if sha256(&[secret.as_slice()]).as_slice() != round.seed_hash.as_slice() {
        return Err(ContractError::SecretMismatch {});
    }

    let scan = scan_round(deps.storage, round_id, &round)?;
    let carry = CARRY.load(deps.storage)?;
    let pot = scan.amount + carry;

    round.first_entry_id = Some(scan.first_entry_id);
    round.entropy = Some(scan.entropy.clone());
    round.total_entries = Some(scan.total_entries);
    round.has_late_entries = scan.has_late_entries;
    round.secret = Some(secret.clone());
    round.settled_at = Some(env.block.time);

    // Not enough of a round to run: consume nothing, leave the boundary where
    // it was, and the entries simply belong to the next round.
    if scan.total_entries < cfg.min_entries || pot < cfg.min_pot {
        round.status = RoundStatus::Skipped;
        round.last_entry_id = Some(scan.first_entry_id - 1);
        ROUNDS.save(deps.storage, round_id, &round)?;
        NEXT_UNSETTLED_ID.save(deps.storage, &(round_id + 1))?;
        return Ok(Response::new()
            .add_attribute("action", "skip_round")
            .add_attribute("round_id", round_id.to_string())
            .add_attribute("entries", scan.total_entries.to_string())
            .add_attribute("pot", pot));
    }

    let result = sha256(&[
        secret.as_slice(),
        scan.entropy.as_slice(),
        &round_id.to_be_bytes(),
    ]);

    // Distinct winners are picked by minter, not by current owner: the owner
    // costs a query per candidate, the minter is already in storage. Someone
    // minting from two wallets can therefore take two places — stated plainly
    // rather than paid for with a loop of queries.
    let places = (cfg.payout_bps.len() as u64).min(scan.total_entries);
    let mut winner_indexes: Vec<u64> = vec![];
    let mut winner_minters: Vec<Addr> = vec![];
    let mut winner_tokens: Vec<String> = vec![];
    let mut seed = result.clone();

    for place in 0u64..places {
        seed = sha256(&[seed.as_slice(), &place.to_be_bytes()]);
        let mut buf = [0u8; 16];
        buf.copy_from_slice(&seed.as_slice()[0..16]);
        let mut idx = (u128::from_be_bytes(buf) % scan.total_entries as u128) as u64;

        let mut hops = 0u64;
        loop {
            let (_, e) = entry_at_index(
                deps.storage,
                scan.first_entry_id,
                scan.last_entry_id,
                idx,
            )?;
            if !winner_minters.contains(&e.minter) {
                winner_indexes.push(idx);
                winner_minters.push(e.minter.clone());
                winner_tokens.push(e.token_id.clone());
                break;
            }
            idx = (idx + 1) % scan.total_entries;
            hops += 1;
            if hops >= scan.total_entries {
                break;
            }
        }
    }

    // The prize follows the token, so it is paid to whoever holds it now.
    let mut msgs: Vec<BankMsg> = vec![];
    let mut winners: Vec<Addr> = vec![];
    let mut paid = Uint128::zero();

    for (i, token_id) in winner_tokens.iter().enumerate() {
        let owner = token_owner(deps.as_ref(), &cfg.nft_contract, token_id)?;
        let amount = bps(pot, cfg.payout_bps[i]);
        if !amount.is_zero() {
            msgs.push(BankMsg::Send {
                to_address: owner.to_string(),
                amount: coins(amount.u128(), &cfg.denom),
            });
            paid += amount;
        }
        winners.push(owner);
    }

    let to_treasury = bps(pot, cfg.treasury_bps);
    if !to_treasury.is_zero() {
        msgs.push(BankMsg::Send {
            to_address: cfg.treasury.to_string(),
            amount: coins(to_treasury.u128(), &cfg.denom),
        });
        paid += to_treasury;
    }

    let to_caller = bps(pot, cfg.caller_bps);
    if !to_caller.is_zero() {
        msgs.push(BankMsg::Send {
            to_address: info.sender.to_string(),
            amount: coins(to_caller.u128(), &cfg.denom),
        });
        paid += to_caller;
    }

    round.status = RoundStatus::Drawn;
    round.last_entry_id = Some(scan.last_entry_id);
    round.result = Some(result.clone());
    round.winner_indexes = winner_indexes.clone();
    round.winners = winners.clone();
    round.pot = Some(pot);
    ROUNDS.save(deps.storage, round_id, &round)?;

    NEXT_UNSETTLED_ID.save(deps.storage, &(round_id + 1))?;
    CARRY.save(deps.storage, &(pot - paid))?;

    Ok(Response::new()
        .add_messages(msgs)
        .add_attribute("action", "execute_draw")
        .add_attribute("round_id", round_id.to_string())
        .add_attribute("entries", scan.total_entries.to_string())
        .add_attribute("pot", pot)
        .add_attribute("paid", paid)
        .add_attribute("result", result.to_base64())
        .add_attribute(
            "winner_indexes",
            winner_indexes
                .iter()
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(","),
        )
        .add_attribute(
            "winners",
            winners
                .iter()
                .map(|w| w.to_string())
                .collect::<Vec<_>>()
                .join(","),
        ))
}

fn rollover_round(deps: DepsMut, env: Env, round_id: u64) -> Result<Response, ContractError> {
    let cfg = CONFIG.load(deps.storage)?;
    let mut round = check_settleable(deps.storage, &env, round_id)?;

    let opens_at = round.close_time.plus_seconds(cfg.stale_after_secs);
    if env.block.time < opens_at {
        return Err(ContractError::NotStale {
            round_id,
            secs: cfg.stale_after_secs,
        });
    }

    let first = first_entry_of(deps.storage, round_id)?;
    round.status = RoundStatus::RolledOver;
    round.first_entry_id = Some(first);
    round.last_entry_id = Some(first - 1); // consumes nothing
    round.settled_at = Some(env.block.time);
    ROUNDS.save(deps.storage, round_id, &round)?;
    NEXT_UNSETTLED_ID.save(deps.storage, &(round_id + 1))?;

    Ok(Response::new()
        .add_attribute("action", "rollover_round")
        .add_attribute("round_id", round_id.to_string()))
}

fn update_config(
    deps: DepsMut,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    let ExecuteMsg::UpdateConfig {
        admin,
        nft_contract,
        treasury,
        treasury_bps,
        payout_bps,
        caller_bps,
        min_entries,
        min_pot,
        stale_after_secs,
        paused,
    } = msg
    else {
        unreachable!()
    };

    let mut cfg = CONFIG.load(deps.storage)?;
    if info.sender != cfg.admin {
        return Err(ContractError::Unauthorized {});
    }
    if let Some(v) = admin {
        cfg.admin = deps.api.addr_validate(&v)?;
    }
    if let Some(v) = nft_contract {
        cfg.nft_contract = deps.api.addr_validate(&v)?;
    }
    if let Some(v) = treasury {
        cfg.treasury = deps.api.addr_validate(&v)?;
    }
    if let Some(v) = treasury_bps {
        cfg.treasury_bps = v;
    }
    if let Some(v) = payout_bps {
        cfg.payout_bps = v;
    }
    if let Some(v) = caller_bps {
        cfg.caller_bps = v;
    }
    if let Some(v) = min_entries {
        cfg.min_entries = v;
    }
    if let Some(v) = min_pot {
        cfg.min_pot = v;
    }
    if let Some(v) = stale_after_secs {
        cfg.stale_after_secs = v;
    }
    if let Some(v) = paused {
        cfg.paused = v;
    }
    check_bps(&cfg)?;
    CONFIG.save(deps.storage, &cfg)?;

    Ok(Response::new()
        .add_attribute("action", "update_config")
        .add_attribute("admin", cfg.admin))
}

// ── queries ─────────────────────────────────────────────────────────────────

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Config {} => to_json_binary(&query_config(deps)?),
        QueryMsg::Round { round_id } => to_json_binary(&query_round(deps, env, round_id)?),
        QueryMsg::Rounds { start_after, limit } => {
            to_json_binary(&query_rounds(deps, env, start_after, limit)?)
        }
        QueryMsg::CurrentRound {} => to_json_binary(&query_current(deps, env)?),
        QueryMsg::Entries { start_after, limit } => {
            to_json_binary(&query_entries(deps, start_after, limit)?)
        }
        QueryMsg::Proof { round_id } => to_json_binary(&query_proof(deps, round_id)?),
        QueryMsg::Pot {} => to_json_binary(&query_pot(deps, env)?),
    }
}

fn query_config(deps: Deps) -> StdResult<ConfigResponse> {
    let c = CONFIG.load(deps.storage)?;
    Ok(ConfigResponse {
        admin: c.admin.to_string(),
        nft_contract: c.nft_contract.to_string(),
        denom: c.denom,
        treasury: c.treasury.to_string(),
        treasury_bps: c.treasury_bps,
        payout_bps: c.payout_bps,
        caller_bps: c.caller_bps,
        min_entries: c.min_entries,
        min_pot: c.min_pot,
        stale_after_secs: c.stale_after_secs,
        paused: c.paused,
        next_unsettled_id: NEXT_UNSETTLED_ID.load(deps.storage)?,
        last_round_id: LAST_ROUND_ID.load(deps.storage)?,
        next_entry_id: NEXT_ENTRY_ID.load(deps.storage)?,
        carry: CARRY.load(deps.storage)?,
    })
}

fn to_response(round_id: u64, r: Round, now: Timestamp) -> RoundResponse {
    RoundResponse {
        round_id,
        seed_hash: r.seed_hash.clone(),
        opened_at: r.opened_at,
        close_time: r.close_time,
        status: derived_status(&r, now),
        first_entry_id: r.first_entry_id,
        last_entry_id: r.last_entry_id,
        secret: r.secret,
        entropy: r.entropy,
        result: r.result,
        total_entries: r.total_entries,
        winner_indexes: r.winner_indexes,
        winners: r.winners.iter().map(|w| w.to_string()).collect(),
        pot: r.pot,
        settled_at: r.settled_at,
        has_late_entries: r.has_late_entries,
    }
}

fn query_round(deps: Deps, env: Env, round_id: u64) -> StdResult<RoundResponse> {
    let r = ROUNDS
        .may_load(deps.storage, round_id)?
        .ok_or_else(|| StdError::not_found(format!("round {round_id}")))?;
    Ok(to_response(round_id, r, env.block.time))
}

fn query_rounds(
    deps: Deps,
    env: Env,
    start_after: Option<u64>,
    limit: Option<u32>,
) -> StdResult<RoundsResponse> {
    let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as usize;
    let start = start_after.map(Bound::exclusive);
    let rounds = ROUNDS
        .range(deps.storage, start, None, Order::Ascending)
        .take(limit)
        .map(|item| {
            let (id, r) = item?;
            Ok(to_response(id, r, env.block.time))
        })
        .collect::<StdResult<Vec<_>>>()?;
    Ok(RoundsResponse { rounds })
}

fn query_current(deps: Deps, env: Env) -> StdResult<RoundResponse> {
    let last = LAST_ROUND_ID.load(deps.storage)?;
    let next = NEXT_UNSETTLED_ID.load(deps.storage)?;
    // The round taking entries is the first open one that has not closed.
    for id in next..=last {
        if let Some(r) = ROUNDS.may_load(deps.storage, id)? {
            if env.block.time < r.close_time {
                return Ok(to_response(id, r, env.block.time));
            }
        }
    }
    query_round(deps, env, last)
}

fn query_entries(
    deps: Deps,
    start_after: Option<u64>,
    limit: Option<u32>,
) -> StdResult<EntriesResponse> {
    let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as usize;
    let start = start_after.map(Bound::exclusive);
    let entries = ENTRIES
        .range(deps.storage, start, None, Order::Ascending)
        .take(limit)
        .map(|item| {
            let (entry_id, entry) = item?;
            Ok(EntryResponse { entry_id, entry })
        })
        .collect::<StdResult<Vec<_>>>()?;
    Ok(EntriesResponse { entries })
}

fn query_proof(deps: Deps, round_id: u64) -> StdResult<ProofResponse> {
    let r = ROUNDS
        .may_load(deps.storage, round_id)?
        .ok_or_else(|| StdError::not_found(format!("round {round_id}")))?;

    let mut entries = vec![];
    if let (Some(first), Some(last)) = (r.first_entry_id, r.last_entry_id) {
        if last >= first {
            for item in ENTRIES.range(
                deps.storage,
                Some(Bound::inclusive(first)),
                Some(Bound::inclusive(last)),
                Order::Ascending,
            ) {
                let (entry_id, entry) = item?;
                entries.push(EntryResponse { entry_id, entry });
            }
        }
    }

    Ok(ProofResponse {
        round_id,
        seed_hash: r.seed_hash,
        secret: r.secret,
        entropy: r.entropy,
        result: r.result,
        total_entries: r.total_entries,
        winner_indexes: r.winner_indexes,
        winners: r.winners.iter().map(|w| w.to_string()).collect(),
        entries,
    })
}

fn query_pot(deps: Deps, env: Env) -> StdResult<PotResponse> {
    let c = CONFIG.load(deps.storage)?;
    let balance = deps
        .querier
        .query_balance(env.contract.address, &c.denom)?
        .amount;
    let carry = CARRY.load(deps.storage)?;

    let next = NEXT_UNSETTLED_ID.load(deps.storage)?;
    let first = if next <= 1 {
        1
    } else {
        ROUNDS
            .may_load(deps.storage, next - 1)?
            .and_then(|r| r.last_entry_id)
            .unwrap_or(0)
            + 1
    };
    let mut pending = Uint128::zero();
    for item in ENTRIES
        .range(deps.storage, Some(Bound::inclusive(first)), None, Order::Ascending)
        .take(MAX_SCAN)
    {
        let (_, e) = item?;
        pending += e.amount;
    }

    Ok(PotResponse {
        denom: c.denom,
        balance,
        carry,
        pending,
    })
}
