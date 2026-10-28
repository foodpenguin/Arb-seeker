#!/usr/bin/env bash
# Cold wallet accepts ownership of a bot contract with two-step ownership (e.g. AeroMigrationArb),
# step 2 after the owner called transferOwnership(COLD) (see deploy_aero_arb.sh).
# Run it YOURSELF; cast asks for the cold private key with a hidden prompt.
# The key is never stored: not on disk, not in a keystore, not in shell history or argv.
#
#   ./scripts/accept_ownership.sh <CONTRACT address> <COLD address>
set -euo pipefail

LIQ="${1:?usage: $0 <CONTRACT address> <COLD address>}"
COLD="${2:?usage: $0 <LIQ address> <COLD address>}"
RPC="${RPC:-https://mainnet.base.org}"
export PATH="$PATH:$HOME/.foundry/bin"
die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }
same() { [[ "$(tr '[:upper:]' '[:lower:]' <<<"$1")" == "$(tr '[:upper:]' '[:lower:]' <<<"$2")" ]]; }
rd() { cast call "$LIQ" "$1" --rpc-url "$RPC" 2>/dev/null || true; }

[[ "$(cast chain-id --rpc-url "$RPC")" == "8453" ]] || die "RPC is not Base (8453)"
same "$(rd 'owner()(address)')" "$COLD" && { echo "owner is already $COLD: nothing to do."; exit 0; }
same "$(rd 'pendingOwner()(address)')" "$COLD" || die "pendingOwner is not $COLD (call transferOwnership(COLD) from the current owner first)"
echo "COLD balance: $(cast balance "$COLD" --ether --rpc-url "$RPC") ETH"

echo "Paste the private key of $COLD at the prompt (hidden). A wrong key fails at gas estimation and costs nothing."
R=$(cast send "$LIQ" 'acceptOwnership()' --rpc-url "$RPC" --interactive) || die "cast send failed"
grep -qE '^status +1' <<<"$R" || die "acceptOwnership reverted: $(grep -E 'transactionHash|status' <<<"$R")"
echo "tx $(awk '/^transactionHash/{print $2}' <<<"$R")"

# Public RPCs are load-balanced; a node may lag a block behind.
for _ in $(seq 1 15); do
  same "$(rd 'owner()(address)')" "$COLD" && { echo "verified: owner = $COLD"; exit 0; }
  sleep 2
done
die "tx succeeded but owner() does not show $COLD yet; re-check with: cast call $LIQ 'owner()(address)' --rpc-url $RPC"
