use cosmwasm_std::testing::{mock_dependencies, mock_env, mock_info, MockQuerier};
use cosmwasm_std::{
    from_json, to_json_binary, Binary, ContractResult, QuerierResult, SystemResult, Uint128,
    WasmQuery,
};

use crate::contract::{execute, instantiate, query};
use crate::error::ContractError;
use crate::msg::{
    ExecuteMsg, FreeEntryItem, InstantiateMsg, PotResponse, QueryMsg, RoundResponse,
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

/// Перенос больше не отменяет раунд, который можно разыграть. Раньше это был
/// открытый путь для постороннего: дождаться, пока раскрытие задержится, и
/// обнулить раунд с билетами и призом.
#[test]
fn rollover_refuses_a_drawable_round() {
    let mut deps = deps_with_owner!("alice");
    init(deps.as_mut(), vec![8000], 5);
    record(deps.as_mut(), at(10), "alice", 5, "common-1");
    let err = execute(
        deps.as_mut(),
        at(24 * HOUR + 15 * 24 * HOUR),
        mock_info("anyone", &[]),
        ExecuteMsg::RolloverRound { round_id: 1 },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::RoundIsDrawable { round_id: 1 }));
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

/// Главное свойство схемы: момент вызова на исход не влияет. Всё, из чего
/// считается запасной результат, зафиксировано к закрытию приёма, поэтому
/// перебирать блоки в поисках удобного ответа бесполезно.
#[test]
fn stale_settlement_is_the_same_whenever_it_is_called() {
    fn settle_at(offset: u64) -> RoundResponse {
        let mut deps = deps_with_owner!("alice");
        init(deps.as_mut(), vec![8000], 1);
        record(deps.as_mut(), at(10), "alice", 3, "common-1");
        record(deps.as_mut(), at(20), "bob", 2, "common-2");
        execute(
            deps.as_mut(),
            at(offset),
            mock_info("anyone", &[]),
            ExecuteMsg::SettleStale { round_id: 1 },
        )
        .unwrap();
        round(deps.as_ref(), at(offset), 1)
    }

    let early = settle_at(24 * HOUR + 15 * 24 * HOUR);
    let late = settle_at(24 * HOUR + 40 * 24 * HOUR);
    assert_eq!(early.result, late.result, "результат не должен зависеть от момента вызова");
    assert_eq!(early.winner_indexes, late.winner_indexes);
    assert_eq!(early.status, RoundStatus::Drawn);
}

/// Раунд, посчитанный без раскрытия, отличим от обычного: секрета в нём нет.
/// По этому полю его помечает зеркало, а человек видит в истории.
#[test]
fn stale_settlement_records_no_secret() {
    let mut deps = deps_with_owner!("alice");
    init(deps.as_mut(), vec![8000], 1);
    record(deps.as_mut(), at(10), "alice", 5, "common-1");
    execute(
        deps.as_mut(),
        at(24 * HOUR + 15 * 24 * HOUR),
        mock_info("anyone", &[]),
        ExecuteMsg::SettleStale { round_id: 1 },
    )
    .unwrap();
    let r = round(deps.as_ref(), at(24 * HOUR + 15 * 24 * HOUR), 1);
    assert_eq!(r.status, RoundStatus::Drawn);
    assert!(r.secret.is_none(), "запасной расчёт не должен записывать секрет");
    assert!(r.result.is_some());
    assert_eq!(r.total_entries, Some(5));
}

/// Запасной результат не совпадает с обычным - иначе знание секрета давало бы
/// оператору предсказание обоих исходов как одного.
#[test]
fn stale_result_differs_from_the_revealed_one() {
    fn settle(stale: bool) -> Binary {
        let mut deps = deps_with_owner!("alice");
        init(deps.as_mut(), vec![8000], 1);
        record(deps.as_mut(), at(10), "alice", 3, "common-1");
        record(deps.as_mut(), at(20), "bob", 2, "common-2");
        let when = at(24 * HOUR + 15 * 24 * HOUR);
        let msg = if stale {
            ExecuteMsg::SettleStale { round_id: 1 }
        } else {
            ExecuteMsg::ExecuteDraw { round_id: 1, secret: secret_of(1) }
        };
        execute(deps.as_mut(), when.clone(), mock_info("anyone", &[]), msg).unwrap();
        round(deps.as_ref(), when, 1).result.unwrap()
    }
    assert_ne!(settle(true), settle(false));
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
