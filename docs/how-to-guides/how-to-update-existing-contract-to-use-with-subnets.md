---
title: Update contracts to use with Subnets
---

## Update contracts to use with Subnets

This article helps you update your existing contract configuration to use with Subnets.

If you are a new user to create contracts, you can refer to the [create contracts](how-to-create-contracts.md) article and create L1 and L2 contracts.

:::note

Contracts can be updated to use with subnets **before** they are deployed to environments.

:::

To access your existing contracts, login to the [Hiro Platform](platform.hiro.so) and [import your existing project](https://docs.hiro.so/platform/create-project#import-project-from-github).

You'll need the following public function with the signature `mint-from-subnet` to use with subnets. 

```
 (mint-from-subnet
      (
        uint       ;; asset-id (NFT) or amount (FT)
        principal  ;; sender
        principal  ;; recipient
      )
      (response bool uint)
    )
```

Refer to the example for `simple-nft.clar` file [here](https://github.com/hirosystems/stacks-subnets/blob/072cb1020731ad893773872ccdebb9163eca1a9c/core-contracts/contracts/helper/simple-nft.clar#L42-L49) to add a public function with signature.

```
(define-public (mint-from-subnet (id uint) (sender principal) (recipient principal))
    (begin
        ;; Check that the tx-sender is the provided sender
        (asserts! (is-eq tx-sender sender) ERR_NOT_AUTHORIZED)

        (nft-mint? nft-token id recipient)
    )
)
```

Now that you have your L1 contract ready, create an L2 contract by following the [Create the subnet (L2) contract](how-to-create-contracts.md#creating-the-subnet-l2-contract) section in the How-to create contracts.

