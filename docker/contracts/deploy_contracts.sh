#!/bin/bash

# Deploys the contracts to the devnet, assumed to be running with an RPC endpoint at $DEVNET_RPC_URL.

# Spinwait until the devnet is ready for contracts to be deployed to it
while true; do
    # Check that the token bridge contracts have been deployed, this is the last step of the devnet initialization.
    response=$(curl -X POST -H "Content-Type: application/json" -d '{"jsonrpc":"2.0","method":"eth_getCode","params":["'$INIT_CHECK_ADDRESS'", "latest"],"id":1}' $DEVNET_RPC_URL 2> /dev/null)
    result=$(echo $response | jq -r '.result')

    # If the code is not empty, break out of the spinwait
    if [ "$result" != "0x" ] && [ -n "$result" ]; then
        break
    else
        sleep 1
    fi
done

# Exit on error
set -e

# Returns either "--no-verify" or an empty string
# depending on whether the $NO_VERIFY env var is set
no_verify() {
    if [[ -n $NO_VERIFY ]]; then
        echo "--no-verify"
    fi
    # Implicitly returns an empty string if $NO_VERIFY is unset
}

# Deploy verifier contract
cargo run \
    -p scripts -- \
    -p $DEVNET_PKEY \
    -r $DEVNET_RPC_URL \
    -d $DEPLOYMENTS_PATH \
    deploy-stylus \
    --contract verifier \

# Deploy Merkle contract
cargo run \
    -p scripts -- \
    -p $DEVNET_PKEY \
    -r $DEVNET_RPC_URL \
    -d $DEPLOYMENTS_PATH \
    deploy-stylus \
    --contract merkle \

# Deploy darkpool contract, setting the "--no-verify" flag
# conditionally depending on whether the corresponding env var is set
cargo run \
    -p scripts -- \
    -p $DEVNET_PKEY \
    -r $DEVNET_RPC_URL \
    -d $DEPLOYMENTS_PATH \
    deploy-stylus \
    --contract darkpool-test-contract \
    $(no_verify)

# Deploy the proxy contract
cargo run \
    -p scripts -- \
    -p $DEVNET_PKEY \
    -r $DEVNET_RPC_URL \
    -d $DEPLOYMENTS_PATH \
    deploy-proxy \
    -o $DEVNET_ACCOUNT_ADDRESS

# If the $UPLOAD_VKEYS env var is set, upload the verification keys
if [[ -n $UPLOAD_VKEYS ]]; then
    # Upload VALID WALLET CREATE verification key
    cargo run \
        -p scripts -- \
        -p $DEVNET_PKEY \
        -r $DEVNET_RPC_URL \
        -d $DEPLOYMENTS_PATH \
        upload-vkey \
        -c valid-wallet-create

    # Upload VALID WALLET UPDATE verification key
    cargo run \
        -p scripts -- \
        -p $DEVNET_PKEY \
        -r $DEVNET_RPC_URL \
        -d $DEPLOYMENTS_PATH \
        upload-vkey \
        -c valid-wallet-update

    # Upload VALID COMMITMENTS verification key
    cargo run \
        -p scripts -- \
        -p $DEVNET_PKEY \
        -r $DEVNET_RPC_URL \
        -d $DEPLOYMENTS_PATH \
        upload-vkey \
        -c valid-commitments

    # Upload VALID REBLIND verification key
    cargo run \
        -p scripts -- \
        -p $DEVNET_PKEY \
        -r $DEVNET_RPC_URL \
        -d $DEPLOYMENTS_PATH \
        upload-vkey \
        -c valid-reblind

    # Upload VALID MATCH SETTLE verification key
    cargo run \
        -p scripts -- \
        -p $DEVNET_PKEY \
        -r $DEVNET_RPC_URL \
        -d $DEPLOYMENTS_PATH \
        upload-vkey \
        -c valid-match-settle
fi

# Sleep forever to prevent the Docker Compose stack from aborting due to container exit
sleep infinity
