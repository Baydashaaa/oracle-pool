use cosmwasm_std::testing::{mock_dependencies, mock_env, mock_info, MockQuerier};
use cosmwasm_std::{
    from_json, to_json_binary, Binary, ContractResult, QuerierResult, SystemResult, Uint128,
    WasmQuery,
};

use crate::contract::{execute, instantiate, query};
use crate::error::ContractError;
use crate::msg::{ExecuteMsg, InstantiateMsg, QueryMsg, RoundResponse};
use crate::state::RoundStatus;

const NFT: &str = "nft_contract";
const ADMIN: &str = "admin";
const DENOM: &str = "uluna";
const HOUR: u64 = 3600;

fn secret_of(round: u64) -> Binary {
    Binary::from(format!("secret-for-round-{round}").into_bytes())
}

fn hash(b: &[u8]) -> Binary {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b);
    Binary::from(h.finalize().as_slice())
}

/// Every token is owned by its minter unless a test says otherwise.
fn owner_querier(owner: &'static str) -> MockQuerier {
    let mut q = MockQuerier::new(&[]);
    q.update_wasm(move |req| -> QuerierResult {
        match req {
            WasmQuery::Smart { .. } => SystemResult::Ok(ContractResult::Ok(
                to_json_binary(&serde_json::json!({ "owner": owner })).unwrap(),
            )),
            _ => SystemResult::Ok(ContractResult::Err("unexpected".into())),
        }
    });
    q
}

macro_rules! deps_with_owner {
    ($owner:expr) => {{
        let mut d = mock_dependencies();
        d.querier = owner_querier($owner);
        d
    }};
}

fn init(deps: cosmwasm_std::DepsMut, payout: Vec<u64>, min_entries: u64) {
    let env = mock_env();
    instantiate(
        deps,
        env.clone(),
        mock_info(ADMIN, &[]),
        InstantiateMsg {
            admin: ADMIN.into(),
            nft_contract: NFT.into(),
            denom: DENOM.into(),
            treasury: "treasury".into(),
            treasury_bps: 1000,
            payout_bps: payout,
            caller_bps: 10,
            min_entries,
            min_pot: Uint128::zero(),
            stale_after_secs: 14 * 24 * HOUR,
            first_seed_hash: hash(secret_of(1).as_slice()),
            first_close_time: env.block.time.plus_seconds(24 * HOUR),
        },
    )
    .unwrap();
}

fn at(offset: u64) -> cosmwasm_std::Env {
    let mut e = mock_env();
    e.block.time = e.block.time.plus_seconds(offset);
    e
}

fn record(deps: cosmwasm_std::DepsMut, env: cosmwasm_std::Env, minter: &str, n: u32, id: &str) {
    execute(
        deps,
        env,
        mock_info(NFT, &[]),
        ExecuteMsg::RecordEntry {
            token_id: id.into(),
            minter: minter.into(),
            entries: n,
            amount: Uint128::new(25_000_000_000u128) * Uint128::from(n),
            entropy: Binary::from(format!("e-{id}").into_bytes()),
        },
    )
    .unwrap();
}

fn round(deps: cosmwasm_std::Deps, env: cosmwasm_std::Env, id: u64) -> RoundResponse {
    from_json(query(deps, env, QueryMsg::Round { round_id: id }).unwrap()).unwrap()
}

#[test]
fn only_the_nft_contract_may_record() {
    let mut deps = deps_with_owner!("alice");
    init(deps.as_mut(), vec![8000], 5);
    let err = execute(
        deps.as_mut(),
        at(10),
        mock_info("someone", &[]),
        ExecuteMsg::RecordEntry {
            token_id: "t1".into(),
            minter: "alice".into(),
            entries: 1,
            amount: Uint128::new(1),
            entropy: Binary::from(b"x".to_vec()),
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::Unauthorized {}));
}

#[test]
fn draw_pays_and_is_reproducible() {
    let mut deps = deps_with_owner!("alice");
    init(deps.as_mut(), vec![8000], 5);
    record(deps.as_mut(), at(10), "alice", 5, "common-1");

    let res = execute(
        deps.as_mut(),
        at(24 * HOUR + 1),
        mock_info("anyone", &[]),
        ExecuteMsg::ExecuteDraw {
            round_id: 1,
            secret: secret_of(1),
        },
    )
    .unwrap();

    // winner, treasury, caller
    assert_eq!(res.messages.len(), 3);

    let r = round(deps.as_ref(), at(24 * HOUR + 2), 1);
    assert_eq!(r.status, RoundStatus::Drawn);
    assert_eq!(r.total_entries, Some(5));
    assert_eq!(r.winners, vec!["alice".to_string()]);
    assert!(r.winner_indexes[0] < 5);
    assert!(!r.has_late_entries);
}

#[test]
fn wrong_secret_is_rejected() {
    let mut deps = deps_with_owner!("alice");
    init(deps.as_mut(), vec![8000], 5);
    record(deps.as_mut(), at(10), "alice", 5, "common-1");
    let err = execute(
        deps.as_mut(),
        at(24 * HOUR + 1),
        mock_info("anyone", &[]),
        ExecuteMsg::ExecuteDraw {
            round_id: 1,
            secret: Binary::from(b"nope".to_vec()),
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::SecretMismatch {}));
}

#[test]
fn cannot_draw_before_close() {
    let mut deps = deps_with_owner!("alice");
    init(deps.as_mut(), vec![8000], 5);
    record(deps.as_mut(), at(10), "alice", 5, "common-1");
    let err = execute(
        deps.as_mut(),
        at(HOUR),
        mock_info("anyone", &[]),
        ExecuteMsg::ExecuteDraw {
            round_id: 1,
            secret: secret_of(1),
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::NotClosed { round_id: 1 }));
}

/// A round below the threshold must consume nothing, so the next round starts
/// where this one did. This is the whole rollover mechanism.
#[test]
fn skipped_round_rolls_entries_over() {
    let mut deps = deps_with_owner!("alice");
    init(deps.as_mut(), vec![8000], 5);
    record(deps.as_mut(), at(10), "alice", 1, "common-1");

    execute(
        deps.as_mut(),
        at(10),
        mock_info(ADMIN, &[]),
        ExecuteMsg::OpenRound {
            seed_hash: hash(secret_of(2).as_slice()),
            close_time: mock_env().block.time.plus_seconds(48 * HOUR),
        },
    )
    .unwrap();

    execute(
        deps.as_mut(),
        at(24 * HOUR + 1),
        mock_info("anyone", &[]),
        ExecuteMsg::ExecuteDraw {
            round_id: 1,
            secret: secret_of(1),
        },
    )
    .unwrap();

    let r1 = round(deps.as_ref(), at(24 * HOUR + 2), 1);
    assert_eq!(r1.status, RoundStatus::Skipped);
    assert_eq!(r1.first_entry_id, Some(1));
    assert_eq!(r1.last_entry_id, Some(0), "a skip must consume nothing");

    // four more entries arrive, round 2 now has five
    record(deps.as_mut(), at(25 * HOUR), "bob", 4, "rare-1");
    execute(
        deps.as_mut(),
        at(48 * HOUR + 1),
        mock_info("anyone", &[]),
        ExecuteMsg::ExecuteDraw {
            round_id: 2,
            secret: secret_of(2),
        },
    )
    .unwrap();

    let r2 = round(deps.as_ref(), at(48 * HOUR + 2), 2);
    assert_eq!(r2.status, RoundStatus::Drawn);
    assert_eq!(r2.total_entries, Some(5), "the rolled-over ticket must count");
    assert_eq!(r2.first_entry_id, Some(1));
}

#[test]
fn settlement_is_strictly_in_order() {
    let mut deps = deps_with_owner!("alice");
    init(deps.as_mut(), vec![8000], 5);
    execute(
        deps.as_mut(),
        at(10),
        mock_info(ADMIN, &[]),
        ExecuteMsg::OpenRound {
            seed_hash: hash(secret_of(2).as_slice()),
            close_time: mock_env().block.time.plus_seconds(48 * HOUR),
        },
    )
    .unwrap();

    let err = execute(
        deps.as_mut(),
        at(48 * HOUR + 1),
        mock_info("anyone", &[]),
        ExecuteMsg::ExecuteDraw {
            round_id: 2,
            secret: secret_of(2),
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::OutOfOrder { expected: 1 }));
}

#[test]
fn rollover_needs_the_round_to_be_stale() {
    let mut deps = deps_with_owner!("alice");
    init(deps.as_mut(), vec![8000], 5);
    record(deps.as_mut(), at(10), "alice", 5, "common-1");

    let err = execute(
        deps.as_mut(),
        at(25 * HOUR),
        mock_info("anyone", &[]),
        ExecuteMsg::RolloverRound { round_id: 1 },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::NotStale { .. }));

    execute(
        deps.as_mut(),
        at(24 * HOUR + 15 * 24 * HOUR),
        mock_info("anyone", &[]),
        ExecuteMsg::RolloverRound { round_id: 1 },
    )
    .unwrap();

    let r = round(deps.as_ref(), at(24 * HOUR + 15 * 24 * HOUR), 1);
    assert_eq!(r.status, RoundStatus::RolledOver);
    assert_eq!(r.last_entry_id, Some(0), "a rollover must consume nothing");
}

/// The point of minter-supplied entropy: the operator knows the secret, so if
/// entries did not move the result they could mint until it pointed at them.
#[test]
fn entropy_from_entries_changes_the_result() {
    fn result_with(extra: bool) -> Binary {
        let mut deps = deps_with_owner!("alice");
        init(deps.as_mut(), vec![8000], 1);
        record(deps.as_mut(), at(10), "alice", 1, "common-1");
        if extra {
            record(deps.as_mut(), at(20), "bob", 1, "common-2");
        }
        execute(
            deps.as_mut(),
            at(24 * HOUR + 1),
            mock_info("anyone", &[]),
            ExecuteMsg::ExecuteDraw {
                round_id: 1,
                secret: secret_of(1),
            },
        )
        .unwrap();
        round(deps.as_ref(), at(24 * HOUR + 2), 1).result.unwrap()
    }
    assert_ne!(result_with(false), result_with(true));
    assert_eq!(result_with(true), result_with(true));
}

/// Entries recorded after close_time belong to the next round, whatever time
/// the draw is actually triggered.
#[test]
fn entries_after_close_belong_to_the_next_round() {
    let mut deps = deps_with_owner!("alice");
    init(deps.as_mut(), vec![8000], 1);
    record(deps.as_mut(), at(10), "alice", 1, "common-1");
    execute(
        deps.as_mut(),
        at(10),
        mock_info(ADMIN, &[]),
        ExecuteMsg::OpenRound {
            seed_hash: hash(secret_of(2).as_slice()),
            close_time: mock_env().block.time.plus_seconds(48 * HOUR),
        },
    )
    .unwrap();
    // arrives one second after close_time
    record(deps.as_mut(), at(24 * HOUR + 1), "bob", 1, "common-2");

    execute(
        deps.as_mut(),
        at(30 * HOUR),
        mock_info("anyone", &[]),
        ExecuteMsg::ExecuteDraw {
            round_id: 1,
            secret: secret_of(1),
        },
    )
    .unwrap();

    let r = round(deps.as_ref(), at(30 * HOUR), 1);
    assert_eq!(r.total_entries, Some(1), "the late mint must not join round 1");
    assert_eq!(r.last_entry_id, Some(1));
}
