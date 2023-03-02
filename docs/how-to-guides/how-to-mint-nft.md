---
title: How to Mint NFT
---

## Mint an NFT

To mint an NFT, create a transaction to mint an NFT on the L1 chain by using the command shown below.

```
node ./mint_nft.js 2 
```

Once the transaction is processed, the principal `USER_ADDR` will own an NFT.

## Verify the transaction was successful

To verify the transaction was successful in the next few blocks in the Stacks explorer, look for a line in the response similar to the example below.

🟩  invoked: ST2NEB84ASENDXKYGJPQW86YXQCEFEX2ZQPG87ND.simple-nft-l1::gift-nft(ST2NEB84ASENDXKYGJPQW86YXQCEFEX2ZQPG87ND, u5) (ok true)