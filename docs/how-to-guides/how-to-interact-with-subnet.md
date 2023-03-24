---
title: How to interact with Subnet
---

### Interact with Subnet

We will now use the scripts created in the [Set up nodejs scripts](how-to-setup-nodejs-scripts.md) to demonstrate a subnet's functionality. In this article, you can do the following things:

1. Publish our NFT contract on the subnet
2. Mint a new NFT in the stacks network
3. Deposit this NFT into the subnet
4. Transfer the NFT from one user to another in the subnet
5. Withdraw the NFT from the subnet

First, we will publish the L2 NFT contract to the subnet:

```sh
node ./publish.js simple-nft-l2 ../contracts/simple-nft-l2.clar 2 0
```

Clarinet's interface doesn't show the transactions on the subnet, but we can see the transaction in our local explorer instance. In a web browser, visit http://localhost:8000. By default, it will open the explorer for the devnet L1. To switch to the subnet, click on "Network" in the top right, then "Add a network." In the popup, choose a name, e.g., "Devnet Subnet," then for the URL, use "http://localhost:13999". You will know this contract deployment succeeded when you see the contract deploy transaction for "simple-nft-l2" in the list of confirmed transactions.

![contract deploy confirmed](../images/confirmed.png)

Now that the NFT contracts are deployed to both the L1 and the L2, we will register the NFT with the subnet.

```sh
node ./register.js
```

This is an L1 transaction, so you can watch for it in the Clarinet interface or the Devnet network on explorer.

Now, we need an asset to work with, so we will mint an NFT on the L1:

```js
node ./mint.js 0
```

We can see this transaction either on the Clarinet interface or in the Devnet network on explorer.

Once the mint has been processed, we can deposit it into the subnet:

```js
node ./deposit.js 1
```

We can see this transaction on the Clarinet interface or in the Devnet network on explorer.

We can verify that the NFT is now owned by the subnet contract
(`ST1PQHQKV0RJXZFY1DGX8MNSNYVE3VGZJSRTPGZGM.subnet-v1-1`) on the L1 using:

```js
node ./verify.js 1
```

Similarly, we can verify that the NFT is owned by the expected address
(`ST2NEB84ASENDXKYGJPQW86YXQCEFEX2ZQPG87ND`) on the L2:

```js
node ./verify.js 2
```

Now that the NFT is inside the subnet, we can transfer it from one address to another:

```js
node ./transfer.js 1
```

We can see this transaction in the "Devnet Subnet" network in our explorer.

If we call the `verify.js` script again, we should now see that the NFT is owned by `ST2REHHS5J3CERCRBEPMGH7921Q6PYKAADT7JP2VB`.

Now, we will initiate a withdrawal from the subnet, by calling the `nft-withdraw?` function on the L2 subnet contract.

```js
node ./withdraw-l2.js 0
```

We can confirm that this transaction is successful in the L2 explorer. In the explorer, note the block height that this withdrawal transaction is included in. Fill in this block height for `$height` in the next step.

For the second part of the withdraw, we call `withdraw-nft-asset` on the L1 subnet contract:

```sh
node ./withdraw-l1.js $height 0
```

This is an L1 transaction, so it can be confirmed in the L1 explorer or in the Clarinet terminal UI.

If everything went well, now the NFT should be owned by the correct user on the L1 (`ST2REHHS5J3CERCRBEPMGH7921Q6PYKAADT7JP2VB`):

```sh
node ./verify.js 1
```

In the subnet, this asset should not be owned by anyone (`none`):

```sh
node ./verify.js 2
```
