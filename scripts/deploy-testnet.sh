#!/usr/bin/env bash
# Deploys the vault to NEAR testnet with near-cli-rs. Not run by anyone yet:
# needs a funded testnet account you control (OWNER) and a keeper account.
# Usage: OWNER=you.testnet KEEPER=keeper.you.testnet ./scripts/deploy-testnet.sh
set -euo pipefail
: "${OWNER:?set OWNER}"; : "${KEEPER:?set KEEPER}"
VAULT="${VAULT:-trimtab.$OWNER}"
USDC="${USDC:-usdc.fakes.testnet}"          # any NEP-141 on testnet with 6 decimals
SPOT="${SPOT:-eth.fakes.testnet}"           # any NEP-141 with 18 decimals
MPC="v1.signer-prod.testnet"

(cd crates/vault-contract && cargo near build non-reproducible-wasm)
WASM=target/near/vault_contract/vault_contract.wasm
[ -f "$WASM" ] || WASM=crates/vault-contract/target/near/vault_contract.wasm

near account create-account fund-myself "$VAULT" '5 NEAR' autogenerate-new-keypair save-to-keychain \
  sign-as "$OWNER" network-config testnet sign-with-keychain send
near contract deploy "$VAULT" use-file "$WASM" with-init-call new \
  json-args "{\"owner\":\"$OWNER\",\"usdc\":\"$USDC\",\"spot_token\":\"$SPOT\",\"mpc\":\"$MPC\",\"keepers\":[\"$KEEPER\"],\"config\":null}" \
  prepaid-gas '100 Tgas' attached-deposit '0 NEAR' network-config testnet sign-with-keychain send
echo "Deployed $VAULT. Next: derive its EVM address and set perp_evm_address:"
echo "  cargo run -p keeper -- derive --vault $VAULT --root-key \$(near contract call-function as-read-only $MPC public_key json-args {} network-config testnet now)"
