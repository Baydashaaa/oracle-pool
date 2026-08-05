# oracle-pool

Prize pool and draw settlement for Oracle Draw on Terra Classic. One instance
per pool — daily and weekly are deployed separately.

## What it is for

Not verifiability: the existing off-chain draw already picks the winner from
the hash of the block at the round deadline, which anyone can recheck. This
contract exists to take **custody** away from a wallet controlled by a
mnemonic, and to make the payout permissionless.

## How a round works

1. `OpenRound` commits `sha256(secret)` — meant to be called while the previous
   round is still taking entries, so the commitment is older than every entry
   it governs.
2. Each mint calls `RecordEntry` from the NFT contract, in the same transaction
   as the payment, carrying 32 bytes of entropy from the minter's browser.
3. After `close_time`, anyone with the secret calls `ExecuteDraw` and takes
   `caller_bps` for their gas.

```
entropy = fold sha256(acc || minter || entry_entropy) over the round's entries
result  = sha256(secret || entropy || round_id)
index   = u128(result[0..16]) % total_entries
```

Tickets are the round's entries in id order, each repeated `entries` times.
`Proof { round_id }` returns everything needed to recheck this.

## Why minters supply entropy

Commit-reveal alone would not be enough. The operator knows the secret, and
unlike in a standalone beacon they cannot be barred from entering, because
minting *is* entering. They could compute the winner and keep minting until the
result pointed at them. Entropy from every minter makes the result
unpredictable to them: they cannot know what later minters will add.

## What the operator can still do

Delay. They cannot change an outcome — the commitment predates the entries and
the entropy is not theirs — but they hold the secret, so they choose when to
reveal. There is deliberately no reveal deadline, because the result is fixed
at `close_time` and a deadline would turn a CI outage into a lost round.
`RolloverRound` lets anyone move a round on after `stale_after_secs`, so funds
are never trapped, even if the master key is lost.

This is stated plainly rather than glossed: automation here comes from a
machine holding a key, not from the chain itself.

## Deliberate limits

- Distinct winners are picked by minter, not by current owner. One person
  minting from two wallets can take two places. Doing it properly would cost a
  query per candidate.
- Skipping and rollover consume nothing: the round's entries belong to the next
  one instead. Boundaries are derived at settlement, never stored at open, so
  there is nothing to repair afterwards.
- Settlement is strictly in round order.
- There is no `Sweep`. A contract holding a prize pool should not have a button
  that empties it.

## Build

```bash
cargo test
docker run --rm -v "$(pwd)":/code \
  --mount type=volume,source="$(basename "$(pwd)")_cache",target=/code/target \
  --mount type=volume,source=registry_cache,target=/usr/local/cargo/registry \
  cosmwasm/optimizer:0.16.0
```
