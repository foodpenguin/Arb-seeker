#!/usr/bin/env bash
# Deploys AeroMigrationArb on Base with the HOT keystore and allowlists the pool factories.
# Run it YOURSELF in your own terminal (it signs mainnet transactions).
#
#   ./scripts/deploy_aero_arb.sh <HOT address> <COLD address>
#   ARB=0x... ./scripts/deploy_aero_arb.sh <HOT> <COLD>   # resume / allowlist newly added factories
#
# Addresses come from bot/config.local.toml [aero] (morpho, pool_manager, migration, old_token,
# new_token, legacy_factory, new_factories), never from this script. pool_manager unset = deployed
# with V4 flash accounting disabled (funding = "morpho" only).
#
# Owner stays HOT on purpose during the merger window: new Aero factories can then be allowlisted
# within a minute (re-run with ARB=). The contract holds nothing at rest and every profit goes to the
# immutable PROFIT_RECEIVER (= COLD), so a HOT owner can at most allowlist targets or pause.
# Hand it to COLD afterwards: cast send <ARB> 'transferOwnership(address)' <COLD> (HOT), then
# ./scripts/accept_ownership.sh <ARB> <COLD>.
set -euo pipefail

HOT="${1:?usage: $0 <HOT address> <COLD address>}"
COLD="${2:?usage: $0 <HOT address> <COLD address>}"
ACCOUNT="${ACCOUNT:-bot-hot}"
# KEYSTORE=<file> uses that keystore file instead of the named account (tests on an anvil fork).
KEYSTORE="${KEYSTORE:-}"
RPC="${RPC:-https://mainnet.base.org}"
ARB="${ARB:-}"
NO_PM=0x0000000000000000000000000000000000000000

export PATH="$PATH:$HOME/.foundry/bin"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CFG="${CFG:-$REPO/bot/config.local.toml}"
say() { printf '\n==> %s\n' "$*"; }
die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }
lower() { tr '[:upper:]' '[:lower:]' <<<"$1"; }
same() { [[ "$(lower "$1")" == "$(lower "$2")" ]]; }
sent_ok() { grep -qE '^status +1' <<<"$1"; }
rd() { cast call "$1" "$2" "${@:3}" --rpc-url "$RPC" 2>/dev/null | cut -d' ' -f1 || true; }
wait_eq() {
  local want="$1" got=""; shift
  for _ in $(seq 1 15); do
    got=$(rd "$@"); same "$got" "$want" && return 0
    sleep 2
  done
  printf 'last read of %s: %s\n' "$2" "${got:-<error>}" >&2
  return 1
}
send() {
  local desc="$1" r; shift
  r=$(cast send "$@" --nonce "$NONCE" --rpc-url "$RPC" "${SIGNER[@]}" --password-file "$PWF") || die "$desc: cast send failed"
  sent_ok "$r" || die "$desc reverted: $(grep -E 'transactionHash|status' <<<"$r")"
  echo "tx $(awk '/^transactionHash/{print $2}' <<<"$r")"
  NONCE=$((NONCE + 1))
}
# [aero] key = "0x..." (first address on the line) / key = ["0x..", ...] (all addresses)
aero_val() { awk -v k="$1" '/^\[/{s=($0=="[aero]")} s && $1==k {print; exit}' "$CFG" | grep -oE '0x[0-9a-fA-F]{40}' | head -1; }
aero_list() { awk -v k="$1" '/^\[/{s=($0=="[aero]")} s && $1==k {print; exit}' "$CFG" | grep -oE '0x[0-9a-fA-F]{40}'; }

MORPHO=$(aero_val morpho); MORPHO="${MORPHO:-0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb}"
PM=$(aero_val pool_manager); PM="${PM:-$NO_PM}"
MIG=$(aero_val migration); OLD=$(aero_val old_token); NEW=$(aero_val new_token); LEGF=$(aero_val legacy_factory)
mapfile -t NEWF < <(aero_list new_factories)
mapfile -t LCL < <(aero_list legacy_cl_factories)
[[ -n "$MIG" && -n "$OLD" && -n "$NEW" && -n "$LEGF" && ${#NEWF[@]} -gt 0 ]] || die "[aero] in $CFG needs migration, old_token, new_token, legacy_factory, new_factories"
HOT=$(cast to-check-sum-address "$HOT") || die "bad HOT"
COLD=$(cast to-check-sum-address "$COLD") || die "bad COLD"
same "$HOT" "$COLD" && die "HOT and COLD must differ"

say "chain + config"
[[ "$(cast chain-id --rpc-url "$RPC")" == "8453" ]] || die "RPC is not Base (8453)"
for a in "$MORPHO" "$MIG" "$OLD" "$NEW" "$LEGF" "${NEWF[@]}" "${LCL[@]}"; do
  [[ "$(cast codesize "$a" --rpc-url "$RPC")" -gt 0 ]] || die "no contract at $a"
done
same "$(rd "$MIG" 'V3_TOKEN()(address)')" "$NEW" || die "migration V3_TOKEN != new_token"
same "$(rd "$MIG" 'V2_TOKEN()(address)')" "$OLD" || die "migration V2_TOKEN != old_token"
echo "migration $MIG: old $OLD -> new $NEW (verified on-chain)"
if [[ "$PM" != "$NO_PM" ]]; then
  [[ "$(cast codesize "$PM" --rpc-url "$RPC")" -gt 0 ]] || die "no contract at pool_manager $PM"
  [[ -n "$(rd "$PM" 'protocolFeeController()(address)')" ]] || die "pool_manager $PM does not answer protocolFeeController(): not a V4 PoolManager?"
  echo "V4 PoolManager $PM (verified on-chain)"
else
  echo "pool_manager unset: V4 flash accounting disabled in this deployment"
fi
echo "factories: legacy $LEGF, new ${NEWF[*]}; legacy CL ${LCL[*]:-none}"

if [[ -n "$ARB" ]]; then
  ARB=$(cast to-check-sum-address "$ARB")
  wait_eq "$HOT" "$ARB" 'executor()(address)' || die "executor != HOT"
  wait_eq "$COLD" "$ARB" 'PROFIT_RECEIVER()(address)' || die "PROFIT_RECEIVER != COLD"
  wait_eq "$MIG" "$ARB" 'MIGRATION()(address)' || die "MIGRATION mismatch"
  # Only a note: resuming is for allowlisting, which a deployment from before V4 funding (no
  # POOL_MANAGER()) still needs. The bot's funding check refuses v4/auto on such a contract.
  GOT_PM=$(rd "$ARB" 'POOL_MANAGER()(address)')
  if [[ -z "$GOT_PM" ]]; then
    echo "note: $ARB has no POOL_MANAGER() (deployed before V4 funding): it runs [aero] funding = \"morpho\" only"
  elif ! same "$GOT_PM" "$PM"; then
    echo "note: $ARB POOL_MANAGER is $GOT_PM, config pool_manager is $PM: funding = \"v4\"/\"auto\" will be refused"
  fi
  same "$(rd "$ARB" 'owner()(address)')" "$HOT" || die "owner is not HOT (allowlist from the owner wallet instead)"
fi

if [[ -n "$KEYSTORE" ]]; then SIGNER=(--keystore "$KEYSTORE"); else SIGNER=(--account "$ACCOUNT"); fi
say "keystore $ACCOUNT"
PWF=$(umask 077 && mktemp /dev/shm/aerodeploy.XXXXXX)
trap 'rm -f "$PWF"' EXIT
trap 'exit 130' INT TERM
read -rsp "keystore password for $ACCOUNT: " PW; echo
printf "%s" "$PW" > "$PWF"; unset PW
same "$(cast wallet address "${SIGNER[@]}" --password-file "$PWF")" "$HOT" || die "keystore $ACCOUNT is not HOT $HOT"
BAL=$(cast balance "$HOT" --rpc-url "$RPC")
echo "HOT balance: $(cast to-unit "$BAL" ether) ETH"
[[ "$BAL" -ge 20000000000000 ]] || die "HOT needs at least 0.00002 ETH"
NONCE=$(cast nonce "$HOT" --block pending --rpc-url "$RPC")

TODO=()
[[ -z "$ARB" ]] && TODO+=("deploy AeroMigrationArb(morpho=$MORPHO, poolManager=$PM, owner=HOT, executor=HOT, profitReceiver=$COLD)")
TARGETS=()
for f in "$LEGF" "${NEWF[@]}" "${LCL[@]}"; do
  if [[ -z "$ARB" || "$(rd "$ARB" 'allowedTarget(address)(bool)' "$f")" != "true" ]]; then TARGETS+=("$f"); TODO+=("setAllowedTarget($f, true)"); fi
done
[[ ${#TODO[@]} -gt 0 ]] || { echo "nothing to do: deployed and every factory allowlisted"; exit 0; }
printf '\nAbout to send %d MAINNET transaction(s) from HOT %s:\n' "${#TODO[@]}" "$HOT"
printf '  - %s\n' "${TODO[@]}"
read -rp "type 'deploy' to continue: " OK
[[ "$OK" == "deploy" ]] || die "aborted"

if [[ -z "$ARB" ]]; then
  say "build + deploy"
  (cd "$REPO/contracts" && forge build >/dev/null 2>&1) || die "forge build failed"
  EXPECT=$(cast compute-address "$HOT" --nonce "$NONCE" | awk '{print $NF}')
  echo "deploying to $EXPECT (if anything below fails, resume with: ARB=$EXPECT $0 $HOT $COLD)"
  OUT=$(cd "$REPO/contracts" && forge create src/AeroMigrationArb.sol:AeroMigrationArb --rpc-url "$RPC" "${SIGNER[@]}" --password-file "$PWF" --broadcast \
    --nonce "$NONCE" --constructor-args "$MORPHO" "$PM" "$MIG" "$OLD" "$NEW" "$HOT" "$HOT" "$COLD") || die "forge create failed; check $EXPECT, then resume with ARB=$EXPECT"
  ARB=$(awk '/Deployed to:/{print $3}' <<<"$OUT")
  same "$ARB" "$EXPECT" || die "deployed to '$ARB', expected $EXPECT"
  NONCE=$((NONCE + 1))
  wait_eq "$HOT" "$ARB" 'executor()(address)' || die "executor != HOT"
  wait_eq "$COLD" "$ARB" 'PROFIT_RECEIVER()(address)' || die "PROFIT_RECEIVER != COLD"
  wait_eq "$PM" "$ARB" 'POOL_MANAGER()(address)' || die "POOL_MANAGER mismatch"
  echo "verified: owner=HOT executor=HOT receiver=COLD poolManager=$PM"
fi

for f in "${TARGETS[@]}"; do
  say "setAllowedTarget($f)"
  send setAllowedTarget "$ARB" 'setAllowedTarget(address,bool)' "$f" true
  wait_eq true "$ARB" 'allowedTarget(address)(bool)' "$f" || die "$f not allowlisted"
done

rm -f "$PWF"
cat <<EOF

==> DONE. AeroMigrationArb: $ARB
Put it in bot/config.local.toml:   [aero] contract = "$ARB"
Then (dry run first, then live), from bot/:
  ./target/release/arb-bot -c config.local.toml
  ./target/release/arb-bot -c config.local.toml --live
EOF
