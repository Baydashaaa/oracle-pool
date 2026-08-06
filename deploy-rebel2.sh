#!/usr/bin/env bash
# Deploy a wasm to rebel-2 and instantiate it.
#   ./deploy-rebel2.sh <wasm> <init.json> <label> [expected_sha256]
#
# Always passes --admin, otherwise the contract can never be migrated — which
# is the entire point of this rehearsal.
set -euo pipefail

WASM=${1:?usage: deploy-rebel2.sh <wasm> <init.json> <label> [sha256]}
INIT=${2:?}
LABEL=${3:?}
EXPECTED=${4:-}

KEY=oracle-dev
GAS="--gas auto --gas-adjustment 1.6 --gas-prices 28.325uluna -y"
ADMIN=$(terrad keys show "$KEY" -a)
echo "==> deployer/admin: $ADMIN"

if [ -n "$EXPECTED" ]; then
  local_sha=$(sha256sum "$WASM" | cut -d' ' -f1)
  echo "==> local wasm: $local_sha"
  [ "$local_sha" = "$EXPECTED" ] || { echo "LOCAL WASM IS NOT THE EXPECTED BUILD" >&2; exit 1; }
fi

TMP=$(mktemp)
python3 - "$INIT" "$ADMIN" > "$TMP" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
s = json.dumps(d).replace("ADMIN_ADDRESS_HERE", sys.argv[2])
print(s)
PY

wait_tx () {
  for _ in $(seq 1 40); do
    if out=$(terrad q tx "$1" -o json 2>/dev/null); then echo "$out"; return 0; fi
    sleep 3
  done
  echo "timeout waiting for tx $1" >&2; return 1
}

echo "==> store"
h=$(terrad tx wasm store "$WASM" --from $KEY $GAS -o json \
  | python3 -c "import json,sys; print(json.load(sys.stdin)['txhash'])")
echo "    txhash: $h"

code_id=$(wait_tx "$h" | python3 -c "
import json,sys
d=json.load(sys.stdin)
if d.get('code',0)!=0: sys.exit('STORE FAILED: '+d.get('raw_log','')[:300])
for e in d.get('events',[]):
    if e['type']=='store_code':
        for a in e['attributes']:
            if a['key']=='code_id': print(a['value'])
")
echo "    code_id: $code_id"

echo "==> on-chain checksum"
terrad q wasm code-info "$code_id" -o json | python3 -c "
import json,sys,base64
d=json.load(sys.stdin)
h=d.get('checksum') or d.get('data_hash')
print('   ', base64.b64decode(h).hex() if h.endswith('=') else h.lower())
"

echo "==> instantiate"
h=$(terrad tx wasm instantiate "$code_id" "$(cat $TMP)" \
  --label "$LABEL" --admin "$ADMIN" --from $KEY $GAS -o json \
  | python3 -c "import json,sys; print(json.load(sys.stdin)['txhash'])")

addr=$(wait_tx "$h" | python3 -c "
import json,sys
d=json.load(sys.stdin)
if d.get('code',0)!=0: sys.exit('INSTANTIATE FAILED: '+d.get('raw_log','')[:300])
for e in d.get('events',[]):
    if e['type']=='instantiate':
        for a in e['attributes']:
            if a['key']=='_contract_address': print(a['value'])
")
echo "    contract: $addr"
echo "    code_id:  $code_id"
rm -f "$TMP"
