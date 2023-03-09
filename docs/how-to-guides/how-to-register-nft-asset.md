---
title: How to Register NFT asset
---

## Register an NFT asset

To create an NFT asset, create the transaction so a published NFT asset can be registered. This transaction must be called by a miner of the hyperchains contract.

This transaction will be sent by `AUTH_HC_MINER_ADDR` using the following command:

```
node ./register_nft.js 0
```
In the response, look for the following transaction confirmation in the Clarinet console in an upcoming block on the layer 1.

🟩  invoked: ST1PQHQKV0RJXZFY1DGX8MNSNYVE3VGZJSRTPGZGM.hyperchain::register-new-nft-contract(ST2CY5V39NHDPWSXMW9QDT3HC3GD6Q6XX4CFRK9AG.simple-nft-l1, "hyperchain-deposit-nft-token") (ok true)