#!/usr/bin/env python3
"""Build the mainnet InstantiateMsg for both pool instances.

Reads the master seed from $MASTER — never from a file, never written to one.
Round secrets are derived, not stored:

    secret = HMAC-SHA256(MASTER, "<pool>:<round_id>")

so there is nothing to lose and nothing to leak except the master itself.
Only the hash of round 1's secret goes into the config, and a hash is public
by design.

Writes init-daily.json and init-weekly.json. Prints the seed hashes so they can
be checked against what the workflow computes later.
"""
import datetime as dt
import hashlib
import hmac
import json
import os
import subprocess
import sys

MASTER = os.environ.get("MASTER", "").strip()
if len(MASTER) != 64:
    sys.exit("set MASTER to the 64-char hex master seed first: read -s MASTER")

ADMIN = "terra1744ae9kyj8skflj0l5c3cwdwkcsayh9vw5cxtc"
NFT = "terra1hcsq79vmcqxr97sv720yw6scvyknssx62ufsa4rwlmv02gyft43s46uaqx"
TREASURY = "terra1549z8zd9hkggzlwf0rcuszhc9rs9fxqfy2kagt"
NODE = "https://terra-classic-rpc.publicnode.com:443"


def secret_for(pool: str, round_id: int) -> bytes:
    return hmac.new(bytes.fromhex(MASTER), f"{pool}:{round_id}".encode(), hashlib.sha256).digest()


def chain_now() -> dt.datetime:
    """Block time, not the local clock — WSL has already drifted 18 minutes once."""
    out = subprocess.run(
        ["terrad", "status", "--node", NODE, "-o", "json"], capture_output=True, text=True
    ).stdout
    info = json.loads(out)
    info = info.get("sync_info") or info.get("SyncInfo")
    t = info["latest_block_time"].split(".")[0].rstrip("Z")
    return dt.datetime.fromisoformat(t).replace(tzinfo=dt.timezone.utc)


def next_deadline(pool: str, now: dt.datetime) -> dt.datetime:
    """Daily: 20:00 UTC every day except Monday. Weekly: Monday 20:00 UTC."""
    d = now.replace(hour=20, minute=0, second=0, microsecond=0)
    if d <= now:
        d += dt.timedelta(days=1)
    while (d.isoweekday() == 1) if pool == "daily" else (d.isoweekday() != 1):
        d += dt.timedelta(days=1)
    return d


now = chain_now()
print("chain time:", now.isoformat())

for pool, payout, fname in (
    ("daily", [8000], "init-daily.json"),
    ("weekly", [4800, 2000, 1200], "init-weekly.json"),
):
    deadline = next_deadline(pool, now)
    seed_hash = hashlib.sha256(secret_for(pool, 1)).digest()

    cfg = {
        "admin": ADMIN,
        "nft_contract": NFT,
        "denom": "uluna",
        "treasury": TREASURY,
        "treasury_bps": 1000,
        "payout_bps": payout,
        "caller_bps": 10,
        "min_entries": 5,
        # Zero during the shadow run: the money still goes to the old wallets,
        # so the contract sees an empty pot and any threshold above zero would
        # skip every round and hide exactly what we are trying to compare.
        # Raise weekly's to 500000000000 when the funds are switched over.
        "min_pot": "0",
        "stale_after_secs": 1209600,  # 14 days
        "first_seed_hash": __import__("base64").b64encode(seed_hash).decode(),
        "first_close_time": str(int(deadline.timestamp()) * 1_000_000_000),
    }
    json.dump(cfg, open(fname, "w"), indent=2)
    print(f"\n{fname}")
    print("  close_time:", deadline.isoformat())
    print("  seed_hash: ", cfg["first_seed_hash"])
    print("  payout:    ", payout)

print("\nBoth rounds are committed before a single entry exists — which is the")
print("guarantee. Do not lose MASTER: it is the only way to reveal any round.")
