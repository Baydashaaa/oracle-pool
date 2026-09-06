# Репетиция SettleStale в rebel-2

Цель: убедиться, что раунд без раскрытия секрета действительно рассчитывается,
деньги уходят настоящему владельцу токена, и газа хватает на скан билетов.
Юнит-тесты этого не проверяют - там NFT-контракт подменён заглушкой, которая
на любой токен отвечает одним и тем же владельцем.

Все команды из `~/oracle-pool`.

## 0. Проверить, что есть на что деплоить

```
terrad q bank balances terra1ufn6575ta92xmgzrllpf5cguycwd4kyyf5uetw -o json | python3 -m json.tool
```

Нужны luna на газ и немного на приз.

## 1. Взять реальные токены из тестового CW721

Владелец при расчёте запрашивается у настоящего контракта, поэтому и токены
нужны настоящие.

```
NFT=terra1afkyfrr8adkvgfafk5097nmlqcfdk27af0h269t0u2x9567a7ydqgyseya
terrad q wasm contract-state smart "$NFT" '{"all_tokens":{"limit":5}}' -o json
```

Запиши два-три `token_id` из ответа - они понадобятся в шаге 3.

## 2. Инстанцировать репетиционный экземпляр

`init-rehearsal.json` отличается от боевого тремя вещами: `nft_contract` -
твой кошелёк (чтобы можно было записывать билеты вручную), `stale_after_secs`
всего 300 секунд (чтобы не ждать), `min_entries` равен 1.

`first_close_time` - в наносекундах. Поставь примерно через 5 минут:

```
python3 -c "import time; print(int((time.time()+300)*1e9))"
```

Подставь это число в `init-rehearsal.json`, потом:

```
bash deploy-rebel2.sh artifacts/oracle_pool.wasm init-rehearsal.json \
  "oracle-pool stale rehearsal" a0c21126789319677cec1d5db9496907c3bebba6aee4b8c7d1ab2b8a30f8ffd8
```

Хеш здесь не для красоты: скрипт откажется деплоить, если локальный wasm не
тот, что ты собрал.

Адрес из вывода сохрани:

```
POOL=<адрес_из_вывода>
```

## 3. Записать билеты и положить приз

`amount` - это то, что якобы пришло в пул с минта. Деньги отдельно шлём на
контракт, иначе расчёт упрётся в баланс.

```
KEY=oracle-dev
GAS="--gas auto --gas-adjustment 1.6 --gas-prices 28.325uluna -y"

terrad tx bank send $KEY "$POOL" 3000000uluna $GAS

terrad tx wasm execute "$POOL" '{"record_entry":{"token_id":"<ТОКЕН_1>","minter":"terra1ufn6575ta92xmgzrllpf5cguycwd4kyyf5uetw","entries":3,"amount":"1000000","entropy":"ZW50cm9weS1vbmU="}}' --from $KEY $GAS

terrad tx wasm execute "$POOL" '{"record_entry":{"token_id":"<ТОКЕН_2>","minter":"terra1ufn6575ta92xmgzrllpf5cguycwd4kyyf5uetw","entries":2,"amount":"1000000","entropy":"ZW50cm9weS10d28="}}' --from $KEY $GAS
```

## 4. Переключить nft_contract на настоящий

Билеты уже записаны, дальше контракт нужен только чтобы спросить владельца.

```
terrad tx wasm execute "$POOL" "{\"update_config\":{\"nft_contract\":\"$NFT\"}}" --from $KEY $GAS
```

## 5. Дождаться и рассчитать БЕЗ секрета

Ждём закрытия раунда плюс 300 секунд. Проверить, что раунд закрыт:

```
terrad q wasm contract-state smart "$POOL" '{"round":{"round_id":1}}' -o json | python3 -m json.tool
```

Сначала убедиться, что раньше срока путь закрыт - должна быть ошибка
`not stale yet`:

```
terrad tx wasm execute "$POOL" '{"settle_stale":{"round_id":1}}' --from $KEY $GAS
```

После истечения 300 секунд - настоящий вызов. **С другого ключа**, чтобы
заодно проверить, что путь permissionless и `caller_bps` уходит вызвавшему:

```
terrad tx wasm execute "$POOL" '{"settle_stale":{"round_id":1}}' --from alice $GAS
```

Если на `alice` нет luna в rebel-2 - отправь ей немного шагом раньше.

## 6. Что проверить в результате

```
terrad q wasm contract-state smart "$POOL" '{"round":{"round_id":1}}' -o json | python3 -m json.tool
```

- `status` = `drawn`
- `secret` = `null` - раунд посчитан без раскрытия
- `result` заполнен, `winner_indexes` не пустой
- `winners` содержит адрес, полученный запросом `owner_of` у настоящего CW721,
  а не подставленный минтер

И деньги:

```
terrad q bank balances "$POOL" -o json | python3 -m json.tool
```

Приз должен уйти, на контракте остаётся только carry.

## 7. Повторный вызов

```
terrad tx wasm execute "$POOL" '{"settle_stale":{"round_id":1}}' --from $KEY $GAS
```

Ожидается `already settled`. Дважды платить контракт не должен.

## Чего эта репетиция НЕ проверяет

Совпадение результата при вызове в разные моменты - это проверено юнит-тестом,
в сети повторить нельзя, раунд рассчитывается один раз.

Расход газа на большом числе билетов. Здесь их пять, на мейннете бывает
несколько десятков. Если беспокоит - запиши в репетиции штук тридцать и
посмотри `gas_used` в ответе.
