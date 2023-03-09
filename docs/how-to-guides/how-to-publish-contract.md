---
title: How to Publish Contract
---

## Publish the NFT contract to the Stacks and Subnets

Once the Stacks node and the hyperchain node boots up (use the indicators in the top right panel to determine this), you can begin interacting with the chains. Initially, you will want to publish NFT contracts on both L1 and L2.

### Publish layer 1 contract

When depositing your L1 NFT on the hyperchain, your asset will be minted by the L2 NFT contract. 

The publish script takes in four arguments:

- the name of the contract to be published
- the filename for the contract source code
- the layer on which to broadcast the transaction (1 or 2)
- the nonce of the transaction

First, publish the layer 1 contracts. You can enter this command (and the following transaction commands) in the same terminal window as you entered the environment variables. Make sure you are in the `scripts` directory. 

These transactions are called by the principal `USER_ADDR`.

Here is an example of how to publish a layer 1 contract.

```
node ./publish_tx.js trait-standards ../contracts/trait-standards.clar 1 0 
node ./publish_tx.js simple-nft-l1 ../contracts/simple-nft.clar 1 1
```

Verify the contracts were published by using the Clarinet console. For layer 1 contracts, you should see the following lines in the "transactions" region in a recent block.

🟩  deployed: ST2NEB84ASENDXKYGJPQW86YXQCEFEX2ZQPG87ND.trait-standards (ok true)              

🟩  deployed: ST2NEB84ASENDXKYGJPQW86YXQCEFEX2ZQPG87ND.simple-nft-l1 (ok true)

### Publish layer 2 contract

Next, you should publish the layer 2 contracts. Note that it may take a minute for the hyperchain node to start accepting transactions, so these commands may fail if you send them too early (but you can always re-try when the node is ready).

These transactions are called by the principal `USER_ADDR`.

Here is an example of how to publish a layer 2 contract.

```
node ./publish_tx.js trait-standards ../contracts-l2/trait-standards.clar 2 0 
node ./publish_tx.js simple-nft-l2 ../contracts-l2/simple-nft-l2.clar 2 1 
```

To verify the layer 2 transactions were processed, `grep` the hyperchains log for the transaction IDs of *each* hyperchain transaction. The transaction ID is logged to the console after the call to `publish_tx` - make sure this is the ID you `grep` for.

For example,

```
docker logs hyperchain-node.nft-use-case.devnet 2>&1 | grep "17901e5ad0587d414d5bb7b1c24c3d17bb1533f5025d154719ba1a2a0f570246"
```

Look for a log line similar to the following in the results:

```
Jul 19 12:34:41.683519 INFO Tx successfully processed. (ThreadId(9), src/chainstate/stacks/miner.rs:235), event_name: transaction_result, tx_id: 17901e5ad0587d414d5bb7b1c24c3d17bb1533f5025d154719ba1a2a0f570246, event_type: success, payload: SmartContract
```

To ensure the contracts were successfully parsed and published, `grep` for the name of the contract and ensure there are no error lines returned (not atypical for no lines to be returned at this step).

For example,

```
docker logs hyperchain-node.nft-use-case.devnet 2>&1 | grep "simple-nft-l2"
```