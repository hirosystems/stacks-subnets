---
title: How to set up Node.js scripts
---

## Setup Node.js scripts

In this article, you'll initialize a Node.js project,  

To submit transactions to Hiro's Stacks node and subnet node, we will use [Stacks.js](https://stacks.js.org) and some simple scripts. We will start by creating a new directory, _./scripts/_ for these scripts.

```sh
mkdir scripts
cd scripts
```

Then we will initialize a Node.js project and install the stacks.js dependencies:

```sh
npm init -y
npm install @stacks/network @stacks/transactions
```

In the generated `package.json` file, add the following into the `json` to enable modules:

```json
  "type": "module",
```

To simplify our scripts, we will define some environment variables that will be used to hold the signing keys for various subnet transactions.

```sh
export DEPLOYER_ADDR=ST1PQHQKV0RJXZFY1DGX8MNSNYVE3VGZJSRTPGZGM
export DEPLOYER_KEY=753b7cc01a1a2e86221266a154af739463fce51219d97e4f856cd7200c3bd2a601

export USER_ADDR=ST2NEB84ASENDXKYGJPQW86YXQCEFEX2ZQPG87ND
export USER_KEY=f9d7206a47f14d2870c163ebab4bf3e70d18f5d14ce1031f3902fbbc894fe4c701

export ALT_USER_ADDR=ST2REHHS5J3CERCRBEPMGH7921Q6PYKAADT7JP2VB
export ALT_USER_KEY=3eccc5dac8056590432db6a35d52b9896876a3d5cbdea53b72400bc9c2099fe801
export SUBNET_URL="http://localhost:30443"
```

### Publish Contract script

We will start with a script to publish a contract. To make it reusable, we will allow this script to handle some command line arguments:

1. Contract name
2. Path to contract
3. Network layer (1 = Stacks, 2 = Subnet)
4. The deployer's current account nonce

_publish.js_:

```js
import {
  AnchorMode,
  makeContractDeploy,
  broadcastTransaction,
} from "@stacks/transactions";
import { StacksTestnet, HIRO_MOCKNET_DEFAULT } from "@stacks/network";
import { readFileSync } from "fs";

async function main() {
  const contractName = process.argv[2];
  const contractFilename = process.argv[3];
  const networkLayer = parseInt(process.argv[4]);
  const nonce = parseInt(process.argv[5]);
  const senderKey = process.env.USER_KEY;
  const networkUrl =
    networkLayer == 2 ? process.env.SUBNET_URL : HIRO_MOCKNET_DEFAULT;

  const codeBody = readFileSync(contractFilename, { encoding: "utf-8" });

  const transaction = await makeContractDeploy({
    codeBody,
    contractName,
    senderKey,
    network: new StacksTestnet({ url: networkUrl }),
    anchorMode: AnchorMode.Any,
    fee: 10000,
    nonce,
  });

  const txid = await broadcastTransaction(
    transaction,
    new StacksTestnet({ url: networkUrl })
  );

  console.log(txid);
}

main();
```

### Register NFT script

We also need to register our NFT with our subnet, allowing it to be depositedinto the subnet. To do this, we'll write another script, but because we only need to do this once, we will hardcode our details into the script.

This script calls `register-new-nft-contract` on the L1 subnet contract, passing the L1 and L2 NFT contracts we will publish.

_register.js_:

```js
import {
  makeContractCall,
  AnchorMode,
  contractPrincipalCV,
  broadcastTransaction,
  getNonce,
} from "@stacks/transactions";
import { StacksTestnet, HIRO_MOCKNET_DEFAULT } from "@stacks/network";

async function main() {
  const network = new StacksTestnet({ url: HIRO_MOCKNET_DEFAULT });
  const senderKey = process.env.DEPLOYER_KEY;
  const deployerAddr = process.env.DEPLOYER_ADDR;
  const userAddr = process.env.USER_ADDR;
  const nonce = await getNonce(deployerAddr, network);

  const txOptions = {
    contractAddress: "ST1PQHQKV0RJXZFY1DGX8MNSNYVE3VGZJSRTPGZGM",
    contractName: "subnet-v1-1",
    functionName: "register-new-nft-contract",
    functionArgs: [
      contractPrincipalCV(deployerAddr, "simple-nft-l1"),
      contractPrincipalCV(userAddr, "simple-nft-l2"),
    ],
    senderKey,
    validateWithAbi: false,
    network,
    anchorMode: AnchorMode.Any,
    fee: 10000,
    nonce,
  };

  const transaction = await makeContractCall(txOptions);

  const txid = await broadcastTransaction(transaction, network);

  console.log(txid);
}

main();
```

### Mint NFT script

In order to move NFTs to and from the subnet, we will need to have some NFTs on our devnet. To do this, we need to mint, so we also write a script for submitting NFT mint transactions to the layer-1 network. This script takes just one argument: the user's current account nonce.

_mint.js_:

```js
import {
  makeContractCall,
  AnchorMode,
  standardPrincipalCV,
  uintCV,
  broadcastTransaction,
} from "@stacks/transactions";
import { StacksTestnet, HIRO_MOCKNET_DEFAULT } from "@stacks/network";

async function main() {
  const network = new StacksTestnet({ url: HIRO_MOCKNET_DEFAULT });
  const senderKey = process.env.USER_KEY;
  const deployerAddr = process.env.DEPLOYER_ADDR;
  const addr = process.env.USER_ADDR;
  const nonce = parseInt(process.argv[2]);

  const txOptions = {
    contractAddress: deployerAddr,
    contractName: "simple-nft-l1",
    functionName: "gift-nft",
    functionArgs: [standardPrincipalCV(addr), uintCV(5)],
    senderKey,
    validateWithAbi: false,
    network,
    anchorMode: AnchorMode.Any,
    fee: 10000,
    nonce,
  };

  const transaction = await makeContractCall(txOptions);

  const txid = await broadcastTransaction(transaction, network);

  console.log(txid);
}

main();
```

### Deposit NFT script

We also want to be able to deposit an asset into the subnet. To do this, we will write another script to call the `deposit-nft-asset` function on the layer-1 subnet contract. Like the NFT minting script, this script takes just one argument: the user's current account nonce.

_deposit.js_

```js
import {
  makeContractCall,
  AnchorMode,
  standardPrincipalCV,
  uintCV,
  contractPrincipalCV,
  PostConditionMode,
  broadcastTransaction,
} from "@stacks/transactions";
import { StacksTestnet, HIRO_MOCKNET_DEFAULT } from "@stacks/network";

async function main() {
  const network = new StacksTestnet({ url: HIRO_MOCKNET_DEFAULT });
  const senderKey = process.env.USER_KEY;
  const addr = process.env.USER_ADDR;
  const deployerAddr = process.env.DEPLOYER_ADDR;
  const nonce = parseInt(process.argv[2]);

  const txOptions = {
    contractAddress: "ST1PQHQKV0RJXZFY1DGX8MNSNYVE3VGZJSRTPGZGM",
    contractName: "subnet-v1-1",
    functionName: "deposit-nft-asset",
    functionArgs: [
      contractPrincipalCV(deployerAddr, "simple-nft-l1"), // contract ID of nft contract on L1
      uintCV(5), // ID
      standardPrincipalCV(addr), // sender
    ],
    senderKey,
    validateWithAbi: false,
    network,
    anchorMode: AnchorMode.Any,
    fee: 10000,
    postConditionMode: PostConditionMode.Allow,
    nonce,
  };

  const transaction = await makeContractCall(txOptions);

  const txid = await broadcastTransaction(transaction, network);

  console.log(txid);
}

main();
```

### Transfer NFT script

To demonstrate some subnet transactions, we will want to transfer an NFT from one user to another. We will write another script to invoke the NFT's `transfer` function in the subnet. Again, this script takes just one argument: the user's current account nonce.

_transfer.js_

```js
import {
  makeContractCall,
  AnchorMode,
  standardPrincipalCV,
  uintCV,
  PostConditionMode,
  broadcastTransaction,
} from "@stacks/transactions";
import { StacksTestnet } from "@stacks/network";

async function main() {
  const network = new StacksTestnet({ url: process.env.SUBNET_URL });
  const senderKey = process.env.USER_KEY;
  const addr = process.env.USER_ADDR;
  const alt_addr = process.env.ALT_USER_ADDR;
  const nonce = parseInt(process.argv[2]);

  const txOptions = {
    contractAddress: addr,
    contractName: "simple-nft-l2",
    functionName: "transfer",
    functionArgs: [
      uintCV(5), // ID
      standardPrincipalCV(addr), // sender
      standardPrincipalCV(alt_addr), // recipient
    ],
    senderKey,
    validateWithAbi: false,
    network,
    anchorMode: AnchorMode.Any,
    fee: 10000,
    nonce,
    postConditionMode: PostConditionMode.Allow,
  };

  const transaction = await makeContractCall(txOptions);

  const txid = await broadcastTransaction(transaction, network);

  console.log(txid);
}

main();
```

### L2 withdraw script

In order to withdraw an asset from a subnet, users must first submit a withdraw transaction on that subnet. To support this, we will write a script that invokes the `nft-withdraw?` method on the layer-2 subnet contract. This script takes just a single argument: the user's current account nonce.

_withdraw-l2.js_

```js
import {
  makeContractCall,
  AnchorMode,
  standardPrincipalCV,
  contractPrincipalCV,
  uintCV,
  broadcastTransaction,
  PostConditionMode,
} from "@stacks/transactions";
import { StacksTestnet } from "@stacks/network";

async function main() {
  const network = new StacksTestnet({ url: process.env.SUBNET_URL });
  const senderKey = process.env.ALT_USER_KEY;
  const contractAddr = process.env.USER_ADDR;
  const addr = process.env.ALT_USER_ADDR;
  const nonce = parseInt(process.argv[2]);

  const txOptions = {
    contractAddress: "ST000000000000000000002AMW42H",
    contractName: "subnet",
    functionName: "nft-withdraw?",
    functionArgs: [
      contractPrincipalCV(contractAddr, "simple-nft-l2"),
      uintCV(5), // ID
      standardPrincipalCV(addr), // recipient
    ],
    senderKey,
    validateWithAbi: false,
    network,
    anchorMode: AnchorMode.Any,
    fee: 10000,
    nonce,
    postConditionMode: PostConditionMode.Allow,
  };

  const transaction = await makeContractCall(txOptions);

  const txid = await broadcastTransaction(transaction, network);

  console.log(txid);
}

main();
```

### L1 withdraw script

The second step of a withdrawal is to call the `withdraw-nft-asset` method on the layer-1 subnet contract. This method requires information from the subnet to verify that the withdrawal is valid. We will write a script that queries our subnet node's RPC interface for this information and then issues the layer-1 withdrawal transaction.

This scripts has two input arguments: the (subnet) block height of the layer-2 withdrawal transaction, and the user's current account nonce.

_withdraw-l1.js_

```js
import {
  makeContractCall,
  deserializeCV,
  AnchorMode,
  standardPrincipalCV,
  uintCV,
  someCV,
  PostConditionMode,
  contractPrincipalCV,
  broadcastTransaction,
} from "@stacks/transactions";
import { StacksTestnet, HIRO_MOCKNET_DEFAULT } from "@stacks/network";

async function main() {
  const network = new StacksTestnet({ url: HIRO_MOCKNET_DEFAULT });
  const subnetUrl = process.env.SUBNET_URL;
  const senderKey = process.env.ALT_USER_KEY;
  const addr = process.env.ALT_USER_ADDR;
  const l1ContractAddr = process.env.DEPLOYER_ADDR;
  const l2ContractAddr = process.env.USER_ADDR;
  const withdrawalBlockHeight = process.argv[2];
  const nonce = parseInt(process.argv[3]);
  const withdrawalId = 0;

  let json_merkle_entry = await fetch(
    `${subnetUrl}/v2/withdrawal/nft/${withdrawalBlockHeight}/${addr}/${withdrawalId}/${l2ContractAddr}/simple-nft-l2/5`
  ).then((x) => x.json());
  let cv_merkle_entry = {
    withdrawal_leaf_hash: deserializeCV(json_merkle_entry.withdrawal_leaf_hash),
    withdrawal_root: deserializeCV(json_merkle_entry.withdrawal_root),
    sibling_hashes: deserializeCV(json_merkle_entry.sibling_hashes),
  };

  const txOptions = {
    senderKey,
    network,
    anchorMode: AnchorMode.Any,
    contractAddress: "ST1PQHQKV0RJXZFY1DGX8MNSNYVE3VGZJSRTPGZGM",
    contractName: "subnet-v1-1",
    functionName: "withdraw-nft-asset",
    functionArgs: [
      contractPrincipalCV(l1ContractAddr, "simple-nft-l1"), // nft-contract
      uintCV(5), // ID
      standardPrincipalCV(addr), // recipient
      uintCV(withdrawalId), // withdrawal ID
      uintCV(withdrawalBlockHeight), // withdrawal block height
      someCV(contractPrincipalCV(l1ContractAddr, "simple-nft-l1")), // nft-mint-contract
      cv_merkle_entry.withdrawal_root, // withdrawal root
      cv_merkle_entry.withdrawal_leaf_hash, // withdrawal leaf hash
      cv_merkle_entry.sibling_hashes,
    ], // sibling hashes
    fee: 10000,
    postConditionMode: PostConditionMode.Allow,
    nonce,
  };

  const transaction = await makeContractCall(txOptions);

  const txid = await broadcastTransaction(transaction, network);

  console.log(txid);
}

main();
```

### Verify script

Lastly, we need a simple way to query for the current owner of an NFT, so we will write a script that invokes the read-only `get-owner` function via either the subnet or stacks node's RPC interface. This script takes just one argument indicating whether it should query the subnet (`2`) or the stacks node (`1`).

_verify.js_

```js
import {
  uintCV,
  callReadOnlyFunction,
  cvToString,
  cvToHex,
  hexToCV,
} from "@stacks/transactions";
import { StacksTestnet, HIRO_MOCKNET_DEFAULT } from "@stacks/network";

async function main() {
  const networkLayer = parseInt(process.argv[2]);
  const senderAddress = process.env.ALT_USER_ADDR;
  const contractAddress =
    networkLayer == 2 ? process.env.USER_ADDR : process.env.DEPLOYER_ADDR;
  const networkUrl =
    networkLayer == 2 ? process.env.SUBNET_URL : HIRO_MOCKNET_DEFAULT;
  const network = new StacksTestnet({ url: networkUrl });
  const contractName = networkLayer == 2 ? "simple-nft-l2" : "simple-nft-l1";

  const txOptions = {
    contractAddress,
    contractName,
    functionName: "get-owner",
    functionArgs: [uintCV(5)],
    network,
    senderAddress,
  };

  const result = await callReadOnlyFunction(txOptions);

  console.log(cvToString(result.value));
}

main();
```
