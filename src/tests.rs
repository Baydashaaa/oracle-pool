use cosmwasm_std::testing::{mock_dependencies, mock_env, mock_info, MockQuerier};
use cosmwasm_std::{
    from_json, to_json_binary, Binary, ContractResult, QuerierResult, SystemResult, Uint128,
    WasmQuery,
};

use crate::contract::{execute, instantiate, query};
use crate::error::ContractError;
use crate::msg::{
    ConfigResponse, ExecuteMsg, FreeEntryItem, InstantiateMsg, MigrateMsg, PotResponse,
    QueryMsg, RoundResponse,
};
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
                to_json_binary(&serde_json::json!({ "owner": owner, "approvals": [] }))
                    .unwrap(),
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
        // The pot follows the contract's real balance, so a test that expects
        // a payout has to fund it. Tests about the shortfall override this.
        d.querier.update_balance(
            mock_env().contract.address,
            vec![cosmwasm_std::coin(1_000_000_000_000u128, DENOM)],
        );
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
            // Боевое значение: 6 часов. Меньше суток, поэтому daily-раунды
            // через 24 часа проходят проверку OpenRound.
            stale_after_secs: 6 * HOUR,
            operator: None,
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
    // Один билет при пороге в пять: разыграть нечего, значит перенос уместен.
    record(deps.as_mut(), at(10), "alice", 1, "common-1");

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

    // Внутри окна раскрытия: ровно на 30-м часу (закрытие + 6) оно уже
    // закрыто. Тест не про срок, а про поздний минт.
    execute(
        deps.as_mut(),
        at(29 * HOUR),
        mock_info("anyone", &[]),
        ExecuteMsg::ExecuteDraw {
            round_id: 1,
            secret: secret_of(1),
        },
    )
    .unwrap();

    let r = round(deps.as_ref(), at(29 * HOUR), 1);
    assert_eq!(r.total_entries, Some(1), "the late mint must not join round 1");
    assert_eq!(r.last_entry_id, Some(1));
}

/// Entries record what the minter sent, but the burn tax means the contract
/// receives less. The pot must follow the balance, or the shortfall compounds
/// until a draw tries to pay out money that is not there.
#[test]
fn pot_never_exceeds_the_balance() {
    use cosmwasm_std::{coin, BankMsg, CosmosMsg};

    let mut deps = deps_with_owner!("alice");
    // the contract holds 1% less than the entry claims — as on chain
    deps.querier
        .update_balance(mock_env().contract.address, vec![coin(24_750_000_000u128, DENOM)]);
    init(deps.as_mut(), vec![8000], 5);
    record(deps.as_mut(), at(10), "alice", 1, "common-1");
    record(deps.as_mut(), at(11), "alice", 4, "rare-1");

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

    let sent: u128 = res
        .messages
        .iter()
        .map(|m| match &m.msg {
            CosmosMsg::Bank(BankMsg::Send { amount, .. }) => amount[0].amount.u128(),
            _ => 0,
        })
        .sum();

    assert!(
        sent <= 24_750_000_000u128,
        "paid out {sent} with only 24750000000 on hand"
    );
}

/// SEC-03: раунд, который можно было разыграть, после окна раскрытия тоже
/// переносится. Раскрыть его уже нельзя, так что перенос - единственный путь,
/// и он ничего не отнимает: входы и деньги уходят в следующий раунд целиком.
#[test]
fn a_stale_round_rolls_over_even_if_it_could_be_drawn() {
    let mut deps = deps_with_owner!("alice");
    init(deps.as_mut(), vec![8000], 5);
    record(deps.as_mut(), at(10), "alice", 5, "common-1");
    execute(
        deps.as_mut(),
        at(24 * HOUR + 6 * HOUR),
        mock_info("anyone", &[]),
        ExecuteMsg::RolloverRound { round_id: 1 },
    )
    .unwrap();
    let r = round(deps.as_ref(), at(24 * HOUR + 6 * HOUR), 1);
    assert_eq!(r.status, RoundStatus::RolledOver);
    assert!(r.winners.is_empty(), "перенос никого не награждает");
}

/// Запасной путь открывается только после того, как раскрыть было пора.
/// Раньше срока он обходил бы фиксацию: посторонний считал бы оба исхода и
/// вызывал тот, что ему выгоднее.
#[test]
fn stale_settlement_needs_the_round_to_be_stale() {
    let mut deps = deps_with_owner!("alice");
    init(deps.as_mut(), vec![8000], 1);
    record(deps.as_mut(), at(10), "alice", 5, "common-1");
    let err = execute(
        deps.as_mut(),
        at(25 * HOUR),
        mock_info("anyone", &[]),
        ExecuteMsg::SettleStale { round_id: 1 },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::NotStale { .. }));
}

/// SEC-03: SettleStale больше не разыгрывает раунд второй формулой. Та
/// формула давала держателю секрета два известных исхода на выбор. Теперь
/// это перенос, как RolloverRound, и розыгрыша без секрета не бывает.
#[test]
fn settle_stale_now_rolls_over_instead_of_drawing() {
    let mut deps = deps_with_owner!("alice");
    init(deps.as_mut(), vec![8000], 1);
    record(deps.as_mut(), at(10), "alice", 3, "common-1");
    record(deps.as_mut(), at(20), "bob", 2, "common-2");
    execute(
        deps.as_mut(),
        at(24 * HOUR + 6 * HOUR),
        mock_info("anyone", &[]),
        ExecuteMsg::SettleStale { round_id: 1 },
    )
    .unwrap();
    let r = round(deps.as_ref(), at(24 * HOUR + 6 * HOUR), 1);
    assert_eq!(r.status, RoundStatus::RolledOver);
    assert!(r.result.is_none(), "без секрета результата нет");
    assert!(r.winners.is_empty());
}

/// Перенесённые входы не теряются: следующий раунд разыгрывает их вместе со
/// своими, по своему секрету.
#[test]
fn rolled_over_entries_are_drawn_in_the_next_round() {
    let mut deps = deps_with_owner!("alice");
    init(deps.as_mut(), vec![8000], 1);
    record(deps.as_mut(), at(10), "alice", 5, "common-1");
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
        at(24 * HOUR + 6 * HOUR),
        mock_info("anyone", &[]),
        ExecuteMsg::RolloverRound { round_id: 1 },
    )
    .unwrap();
    execute(
        deps.as_mut(),
        at(48 * HOUR + 1),
        mock_info("anyone", &[]),
        ExecuteMsg::ExecuteDraw { round_id: 2, secret: secret_of(2) },
    )
    .unwrap();
    let r2 = round(deps.as_ref(), at(48 * HOUR + 2), 2);
    assert_eq!(r2.status, RoundStatus::Drawn);
    assert_eq!(r2.total_entries, Some(5), "входы раунда 1 разыграны в раунде 2");
}

/// SEC-03: граница окна точная. За секунду до срока можно раскрыть и нельзя
/// переносить; ровно в срок - наоборот. Нет мгновения, когда доступно и то, и
/// другое, иначе в это мгновение снова был бы выбор.
#[test]
fn the_reveal_window_boundary_is_exact() {
    let deadline = 24 * HOUR + 6 * HOUR;

    let mut deps = deps_with_owner!("alice");
    init(deps.as_mut(), vec![8000], 1);
    record(deps.as_mut(), at(10), "alice", 5, "common-1");
    let err = execute(
        deps.as_mut(),
        at(deadline - 1),
        mock_info("anyone", &[]),
        ExecuteMsg::RolloverRound { round_id: 1 },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::NotStale { .. }));
    execute(
        deps.as_mut(),
        at(deadline - 1),
        mock_info("anyone", &[]),
        ExecuteMsg::ExecuteDraw { round_id: 1, secret: secret_of(1) },
    )
    .unwrap();

    let mut deps = deps_with_owner!("alice");
    init(deps.as_mut(), vec![8000], 1);
    record(deps.as_mut(), at(10), "alice", 5, "common-1");
    let err = execute(
        deps.as_mut(),
        at(deadline),
        mock_info("anyone", &[]),
        ExecuteMsg::ExecuteDraw { round_id: 1, secret: secret_of(1) },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::RevealWindowClosed { round_id: 1 }));
    execute(
        deps.as_mut(),
        at(deadline),
        mock_info("anyone", &[]),
        ExecuteMsg::RolloverRound { round_id: 1 },
    )
    .unwrap();
}

/// SEC-03: следующий раунд обязан закрыться после срока раскрытия текущего.
/// Иначе к сроку его входы были бы известны, и держатель секрета снова мог бы
/// посчитать оба исхода.
#[test]
fn next_round_must_close_after_the_reveal_deadline() {
    let mut deps = deps_with_owner!("alice");
    init(deps.as_mut(), vec![8000], 1);
    // Раунд 1 закрывается на 24-м часу, срок раскрытия - 30-й.
    let err = execute(
        deps.as_mut(),
        at(10),
        mock_info(ADMIN, &[]),
        ExecuteMsg::OpenRound {
            seed_hash: hash(secret_of(2).as_slice()),
            close_time: mock_env().block.time.plus_seconds(30 * HOUR),
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::CloseBeforeRevealDeadline { .. }));
    execute(
        deps.as_mut(),
        at(10),
        mock_info(ADMIN, &[]),
        ExecuteMsg::OpenRound {
            seed_hash: hash(secret_of(2).as_slice()),
            close_time: mock_env().block.time.plus_seconds(30 * HOUR + 1),
        },
    )
    .unwrap();
}

fn free(deps: cosmwasm_std::DepsMut, env: cosmwasm_std::Env, who: &str, wallet: &str, tickets: u32, tx: &str)
    -> Result<cosmwasm_std::Response, ContractError>
{
    execute(
        deps,
        env,
        mock_info(who, &[]),
        ExecuteMsg::RecordFreeEntries {
            entries: vec![FreeEntryItem {
                wallet: wallet.into(),
                tickets,
                tx_hash: tx.into(),
            }],
        },
    )
}

/// Записывать бесплатные билеты может только админ: это единственный вид
/// билета, который появляется не из оплаченного минта.
#[test]
fn free_entries_are_admin_only() {
    let mut deps = deps_with_owner!("alice");
    init(deps.as_mut(), vec![8000], 1);
    let err = free(deps.as_mut(), at(10), "someone", "alice", 1, "AABB").unwrap_err();
    assert!(matches!(err, ContractError::Unauthorized {}));
}

/// Главная защита. После закрытия приёма оператор уже знает, куда указывает
/// результат, поэтому дописать билет нельзя - иначе фиксация секрета ничего
/// не стоит.
#[test]
fn free_entries_are_refused_after_close() {
    let mut deps = deps_with_owner!("alice");
    init(deps.as_mut(), vec![8000], 1);
    free(deps.as_mut(), at(10), ADMIN, "alice", 2, "AABB").unwrap();
    let err = free(deps.as_mut(), at(25 * HOUR), ADMIN, "bob", 2, "CCDD").unwrap_err();
    assert!(matches!(err, ContractError::FreeEntriesClosed { round_id: 1 }));
}

/// Бесплатный билет считается наравне с остальными и не приносит денег в пул.
#[test]
fn free_entries_add_tickets_but_no_pot() {
    let mut deps = deps_with_owner!("alice");
    init(deps.as_mut(), vec![8000], 1);
    record(deps.as_mut(), at(10), "alice", 2, "common-1");
    free(deps.as_mut(), at(20), ADMIN, "bob", 3, "AABB").unwrap();

    execute(
        deps.as_mut(),
        at(25 * HOUR),
        mock_info("anyone", &[]),
        ExecuteMsg::ExecuteDraw { round_id: 1, secret: secret_of(1) },
    )
    .unwrap();

    let r = round(deps.as_ref(), at(25 * HOUR), 1);
    assert_eq!(r.total_entries, Some(5), "2 платных + 3 бесплатных");
    // В пул попал только оплаченный минт: 25 LUNC за билет, два билета.
    assert_eq!(r.pot, Some(Uint128::new(50_000_000_000u128)));
}

/// У бесплатного билета нет токена, поэтому приз должен уйти кошельку,
/// который его заработал, а не владельцу несуществующего NFT.
#[test]
fn a_free_ticket_can_win_and_is_paid_to_its_wallet() {
    // Заглушка CW721 отвечает "alice" на любой токен. Если бы контракт
    // спросил владельца для бесплатного билета, победителем стала бы alice -
    // тест на этом и поймает ошибку.
    let mut deps = deps_with_owner!("alice");
    init(deps.as_mut(), vec![8000], 1);
    free(deps.as_mut(), at(10), ADMIN, "bob", 5, "AABB").unwrap();

    let res = execute(
        deps.as_mut(),
        at(25 * HOUR),
        mock_info("anyone", &[]),
        ExecuteMsg::ExecuteDraw { round_id: 1, secret: secret_of(1) },
    )
    .unwrap();

    let r = round(deps.as_ref(), at(25 * HOUR), 1);
    assert_eq!(r.winners, vec!["bob".to_string()], "приз идёт заработавшему кошельку");
    assert_eq!(r.total_entries, Some(5));
    assert_eq!(r.pot, Some(Uint128::zero()), "бесплатные билеты денег не приносят");
    // Пул пустой, поэтому и переводов быть не должно.
    assert!(res.messages.is_empty());
}

/// Пачка пишется целиком и одним заходом.
#[test]
fn free_entries_accept_a_batch() {
    let mut deps = deps_with_owner!("alice");
    init(deps.as_mut(), vec![8000], 1);
    execute(
        deps.as_mut(),
        at(10),
        mock_info(ADMIN, &[]),
        ExecuteMsg::RecordFreeEntries {
            entries: vec![
                FreeEntryItem { wallet: "alice".into(), tickets: 1, tx_hash: "AA".into() },
                FreeEntryItem { wallet: "bob".into(), tickets: 2, tx_hash: "BB".into() },
                FreeEntryItem { wallet: "carol".into(), tickets: 3, tx_hash: "CC".into() },
            ],
        },
    )
    .unwrap();
    execute(
        deps.as_mut(),
        at(25 * HOUR),
        mock_info("anyone", &[]),
        ExecuteMsg::ExecuteDraw { round_id: 1, secret: secret_of(1) },
    )
    .unwrap();
    let r = round(deps.as_ref(), at(25 * HOUR), 1);
    assert_eq!(r.total_entries, Some(6));
}

/// Пустая пачка и ноль билетов - ошибка, а не тихо принятая запись.
#[test]
fn free_entries_reject_empty_input() {
    let mut deps = deps_with_owner!("alice");
    init(deps.as_mut(), vec![8000], 1);
    let err = execute(
        deps.as_mut(),
        at(10),
        mock_info(ADMIN, &[]),
        ExecuteMsg::RecordFreeEntries { entries: vec![] },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::BadFreeBatch { .. }));

    let err = free(deps.as_mut(), at(10), ADMIN, "alice", 0, "AABB").unwrap_err();
    assert!(matches!(err, ContractError::ZeroEntries {}));
}


fn pot(deps: cosmwasm_std::Deps, env: cosmwasm_std::Env) -> PotResponse {
    from_json(query(deps, env, QueryMsg::Pot {}).unwrap()).unwrap()
}

/// Деньги, пришедшие на контракт переводом, в записях входов не видны:
/// перевод не запускает кода, поэтому RecordEntry для них никто не вызывает.
/// Подобрать их должен пропуск раунда - иначе они не попадут в приз никогда,
/// потому что выплата, которая их подбирает, не случится из-за них же.
#[test]
fn a_skip_absorbs_money_that_arrived_by_transfer() {
    let mut deps = deps_with_owner!("alice");
    init(deps.as_mut(), vec![8000], 5);

    // 100k на балансе против одного входа на 25k: разница пришла переводом.
    deps.querier.update_balance(
        mock_env().contract.address,
        vec![cosmwasm_std::coin(100_000_000_000u128, DENOM)],
    );
    record(deps.as_mut(), at(10), "alice", 1, "common-1");

    let before = pot(deps.as_ref(), at(11));
    assert_eq!(before.carry, Uint128::zero());
    assert_eq!(before.pending, Uint128::new(25_000_000_000));

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

    // Один вход против min_entries = 5, значит раунд пропускается.
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

    let r = round(deps.as_ref(), at(24 * HOUR + 2), 1);
    assert_eq!(r.status, RoundStatus::Skipped);

    let after = pot(deps.as_ref(), at(24 * HOUR + 2));
    assert_eq!(
        after.pending,
        Uint128::new(25_000_000_000),
        "вход не израсходован, его деньги остаются в pending"
    );
    assert_eq!(
        after.carry,
        Uint128::new(75_000_000_000),
        "переводом пришло 75k, они должны оказаться в carry"
    );
    assert_eq!(
        after.pending + after.carry,
        after.balance,
        "после подбора весь баланс участвует в поте, ничего не лежит мимо"
    );
}

/// То же самое для отмены раунда: она тоже ничего не выплачивает и тоже не
/// должна оставлять деньги вне пота.
#[test]
fn a_rollover_absorbs_money_that_arrived_by_transfer() {
    let mut deps = deps_with_owner!("alice");
    init(deps.as_mut(), vec![8000], 5);

    deps.querier.update_balance(
        mock_env().contract.address,
        vec![cosmwasm_std::coin(100_000_000_000u128, DENOM)],
    );
    record(deps.as_mut(), at(10), "alice", 1, "common-1");

    let stale = 24 * HOUR + 14 * 24 * HOUR + 1;
    execute(
        deps.as_mut(),
        at(stale),
        mock_info("anyone", &[]),
        ExecuteMsg::RolloverRound { round_id: 1 },
    )
    .unwrap();

    let r = round(deps.as_ref(), at(stale + 1), 1);
    assert_eq!(r.status, RoundStatus::RolledOver);

    let after = pot(deps.as_ref(), at(stale + 1));
    assert_eq!(after.carry, Uint128::new(75_000_000_000));
    assert_eq!(after.pending + after.carry, after.balance);
}

/// Подобранные деньги не должны посчитаться дважды. Вход переезжает в
/// следующий раунд вместе со своими деньгами, поэтому его сумма в carry
/// попадать не должна.
#[test]
fn absorbing_does_not_double_count_the_entries() {
    let mut deps = deps_with_owner!("alice");
    init(deps.as_mut(), vec![8000], 5);

    deps.querier.update_balance(
        mock_env().contract.address,
        vec![cosmwasm_std::coin(100_000_000_000u128, DENOM)],
    );
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

    let after = pot(deps.as_ref(), at(24 * HOUR + 2));
    assert!(
        after.pending + after.carry <= after.balance,
        "pending + carry = {} против баланса {} - это задвоение",
        after.pending + after.carry,
        after.balance
    );
}


// ══════════════════ SEC-04 / SEC-05 / миграция ══════════════════

const OPERATOR: &str = "operator";

fn config(deps: cosmwasm_std::Deps) -> ConfigResponse {
    from_json(query(deps, mock_env(), QueryMsg::Config {}).unwrap()).unwrap()
}

/// Частичное обновление конфига: всё, что не задано, остаётся None.
#[derive(Default)]
struct Upd {
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
}

impl Upd {
    fn msg(self) -> ExecuteMsg {
        ExecuteMsg::UpdateConfig {
            admin: self.admin,
            operator: self.operator,
            nft_contract: self.nft_contract,
            treasury: self.treasury,
            treasury_bps: self.treasury_bps,
            payout_bps: self.payout_bps,
            caller_bps: self.caller_bps,
            min_entries: self.min_entries,
            min_pot: self.min_pot,
            stale_after_secs: self.stale_after_secs,
            paused: self.paused,
        }
    }
}

fn update(deps: cosmwasm_std::DepsMut, who: &str, u: Upd) -> Result<cosmwasm_std::Response, ContractError> {
    execute(deps, mock_env(), mock_info(who, &[]), u.msg())
}

/// Кому ушли деньги в ответе расчёта.
fn recipients(res: &cosmwasm_std::Response) -> Vec<(String, u128)> {
    use cosmwasm_std::{BankMsg, CosmosMsg};
    res.messages
        .iter()
        .filter_map(|m| match &m.msg {
            CosmosMsg::Bank(BankMsg::Send { to_address, amount }) => {
                Some((to_address.clone(), amount[0].amount.u128()))
            }
            _ => None,
        })
        .collect()
}

/// Контракт с отдельным ключом оператора, как будет в проде после миграции.
fn init_split(deps: cosmwasm_std::DepsMut) {
    init(deps, vec![8000], 1);
}

/// SEC-04: оператор - горячий ключ автоматики. Ему нельзя трогать деньги и
/// роли: казну, доли, контракт масок, окно раскрытия, админа, оператора.
#[test]
fn the_operator_cannot_touch_money_or_roles() {
    let mut deps = deps_with_owner!("alice");
    init_split(deps.as_mut());
    update(deps.as_mut(), ADMIN, Upd { operator: Some(OPERATOR.into()), ..Default::default() })
        .unwrap();

    let forbidden: Vec<Upd> = vec![
        Upd { treasury: Some("thief".into()), ..Default::default() },
        Upd { treasury_bps: Some(9000), ..Default::default() },
        Upd { payout_bps: Some(vec![100]), ..Default::default() },
        Upd { caller_bps: Some(500), ..Default::default() },
        Upd { nft_contract: Some("fake_nft".into()), ..Default::default() },
        Upd { stale_after_secs: Some(30 * 24 * HOUR), ..Default::default() },
        Upd { admin: Some(OPERATOR.into()), ..Default::default() },
        Upd { operator: Some("someone".into()), ..Default::default() },
    ];
    for u in forbidden {
        let err = update(deps.as_mut(), OPERATOR, u).unwrap_err();
        assert!(matches!(err, ContractError::Unauthorized {}));
    }
    let c = config(deps.as_ref());
    assert_eq!(c.treasury, "treasury", "казна не изменилась");
    assert_eq!(c.admin, ADMIN);
}

/// SEC-04: пороги и пауза оператору можно - это кнопка set-limits в keeper.
/// Они действуют только на будущие раунды: каждый раунд замораживает условия.
#[test]
fn the_operator_may_set_limits_and_pause() {
    let mut deps = deps_with_owner!("alice");
    init_split(deps.as_mut());
    update(deps.as_mut(), ADMIN, Upd { operator: Some(OPERATOR.into()), ..Default::default() })
        .unwrap();

    update(
        deps.as_mut(),
        OPERATOR,
        Upd {
            min_entries: Some(7),
            min_pot: Some(Uint128::new(123)),
            paused: Some(true),
            ..Default::default()
        },
    )
    .unwrap();
    let c = config(deps.as_ref());
    assert_eq!(c.min_entries, 7);
    assert_eq!(c.min_pot, Uint128::new(123));
    assert!(c.paused);
}

/// Посторонний не может ничего - ни как админ, ни как оператор.
#[test]
fn a_stranger_cannot_update_config() {
    let mut deps = deps_with_owner!("alice");
    init_split(deps.as_mut());
    let err = update(deps.as_mut(), "stranger", Upd { min_entries: Some(1), ..Default::default() })
        .unwrap_err();
    assert!(matches!(err, ContractError::Unauthorized {}));
}

/// SEC-04: открывать раунды и писать бесплатные билеты может оператор, а
/// админ после разделения - уже нет. Роли не пересекаются.
#[test]
fn only_the_operator_opens_rounds_once_split() {
    let mut deps = deps_with_owner!("alice");
    init_split(deps.as_mut());
    update(deps.as_mut(), ADMIN, Upd { operator: Some(OPERATOR.into()), ..Default::default() })
        .unwrap();
    let open = |_who: &str| ExecuteMsg::OpenRound {
        seed_hash: hash(secret_of(2).as_slice()),
        close_time: mock_env().block.time.plus_seconds(48 * HOUR),
    };
    let err = execute(deps.as_mut(), at(10), mock_info(ADMIN, &[]), open(ADMIN)).unwrap_err();
    assert!(matches!(err, ContractError::Unauthorized {}));
    execute(deps.as_mut(), at(10), mock_info(OPERATOR, &[]), open(OPERATOR)).unwrap();
}

/// SEC-04: условия замораживаются при открытии. Смена казны и долей после
/// того, как раунд начал принимать входы, на его расчёт не действует. Раньше
/// расчёт читал конфиг в момент выплаты, и смена казны перенаправляла пот.
#[test]
fn round_terms_are_frozen_at_open() {
    let mut deps = deps_with_owner!("alice");
    init(deps.as_mut(), vec![8000], 1);
    record(deps.as_mut(), at(10), "alice", 5, "common-1");

    update(
        deps.as_mut(),
        ADMIN,
        Upd {
            treasury: Some("thief".into()),
            treasury_bps: Some(3000),
            payout_bps: Some(vec![6000]),
            ..Default::default()
        },
    )
    .unwrap();

    let res = execute(
        deps.as_mut(),
        at(24 * HOUR + 1),
        mock_info("anyone", &[]),
        ExecuteMsg::ExecuteDraw { round_id: 1, secret: secret_of(1) },
    )
    .unwrap();
    let paid = recipients(&res);
    assert!(paid.iter().any(|(to, _)| to == "treasury"), "казна раунда - прежняя");
    assert!(!paid.iter().any(|(to, _)| to == "thief"), "новая казна на этот раунд не действует");

    // Доли тоже прежние: 80% победителю и 10% в казну от пота.
    let pot = round(deps.as_ref(), at(24 * HOUR + 2), 1).pot.unwrap().u128();
    let to_alice: u128 = paid.iter().filter(|(to, _)| to == "alice").map(|(_, a)| a).sum();
    let to_treasury: u128 = paid.iter().filter(|(to, _)| to == "treasury").map(|(_, a)| a).sum();
    assert_eq!(to_alice, pot * 8000 / 10_000);
    assert_eq!(to_treasury, pot * 1000 / 10_000);
}

/// SEC-05: если владельца токена узнать нельзя - маску сожгли, контракт
/// масок сохраняет стандартный Burn, - приз уходит минтеру, а расчёт не
/// падает. Раньше ошибка валила расчёт, и раз он идёт строго по порядку,
/// один сожжённый токен навсегда останавливал все следующие раунды.
#[test]
fn a_burned_token_pays_the_minter_instead_of_halting() {
    let mut deps = mock_dependencies();
    let mut q = MockQuerier::new(&[]);
    q.update_wasm(|_| SystemResult::Ok(ContractResult::Err("token not found".into())));
    deps.querier = q;
    deps.querier.update_balance(
        mock_env().contract.address,
        vec![cosmwasm_std::coin(1_000_000_000_000u128, DENOM)],
    );

    init(deps.as_mut(), vec![8000], 1);
    record(deps.as_mut(), at(10), "alice", 5, "common-1");

    let res = execute(
        deps.as_mut(),
        at(24 * HOUR + 1),
        mock_info("anyone", &[]),
        ExecuteMsg::ExecuteDraw { round_id: 1, secret: secret_of(1) },
    )
    .expect("расчёт не должен падать из-за недоступного владельца");

    assert!(recipients(&res).iter().any(|(to, _)| to == "alice"), "приз ушёл минтеру");
    assert!(res
        .attributes
        .iter()
        .any(|a| a.key == "paid_to_minter_places" && a.value == "1"));
    let r = round(deps.as_ref(), at(24 * HOUR + 2), 1);
    assert_eq!(r.status, RoundStatus::Drawn);
}

/// Миграция: данные в живом контракте старого формата - в конфиге нет поля
/// operator, в раундах нет terms. Проверяем на именно таких байтах, а не на
/// свежем экземпляре: иначе тест прошёл бы, а миграция упала бы в цепочке.
#[test]
fn migration_reads_old_data_and_splits_the_roles() {
    use cosmwasm_std::Storage;
    let mut deps = deps_with_owner!("alice");
    init(deps.as_mut(), vec![8000], 1);
    record(deps.as_mut(), at(10), "alice", 5, "common-1");

    // Переписываем хранилище в формат до выпуска: без operator и без terms.
    let mut cfg: serde_json::Value =
        serde_json::from_slice(&deps.storage.get(b"config").unwrap()).unwrap();
    cfg.as_object_mut().unwrap().remove("operator");
    deps.storage.set(b"config", &serde_json::to_vec(&cfg).unwrap());

    let key = {
        let mut k = vec![0u8, 6];
        k.extend_from_slice(b"rounds");
        k.extend_from_slice(&1u64.to_be_bytes());
        k
    };
    let mut r1: serde_json::Value =
        serde_json::from_slice(&deps.storage.get(&key).unwrap()).unwrap();
    r1.as_object_mut().unwrap().remove("terms");
    deps.storage.set(&key, &serde_json::to_vec(&r1).unwrap());

    // Как на цепочке: admin станет холодным ключом, прежний admin - оператором.
    crate::contract::migrate(
        deps.as_mut(),
        mock_env(),
        MigrateMsg {
            admin: Some("cold".into()),
            operator: None,
            stale_after_secs: Some(6 * HOUR),
        },
    )
    .unwrap();

    let c = config(deps.as_ref());
    assert_eq!(c.admin, "cold");
    assert_eq!(c.operator, ADMIN, "прежний admin остался оператором");
    assert_eq!(c.stale_after_secs, 6 * HOUR);

    // Раунд без terms, открытый до выпуска, рассчитывается по живому конфигу.
    let res = execute(
        deps.as_mut(),
        at(24 * HOUR + 1),
        mock_info("anyone", &[]),
        ExecuteMsg::ExecuteDraw { round_id: 1, secret: secret_of(1) },
    )
    .unwrap();
    assert!(recipients(&res).iter().any(|(to, _)| to == "alice"));

    // Роли разошлись: оператор больше не трогает казну, холодный ключ - может.
    assert!(update(deps.as_mut(), ADMIN, Upd { treasury: Some("new_treasury".into()), ..Default::default() }).is_err());
    update(deps.as_mut(), "cold", Upd { treasury: Some("new_treasury".into()), ..Default::default() }).unwrap();
}

/// Пустой `{}` по-прежнему мигрирует: ни одно поле MigrateMsg не обязательно.
#[test]
fn an_empty_migrate_message_still_works() {
    let mut deps = deps_with_owner!("alice");
    init(deps.as_mut(), vec![8000], 1);
    let m: MigrateMsg = from_json(b"{}").unwrap();
    crate::contract::migrate(deps.as_mut(), mock_env(), m).unwrap();
    assert_eq!(config(deps.as_ref()).operator, ADMIN);
}
