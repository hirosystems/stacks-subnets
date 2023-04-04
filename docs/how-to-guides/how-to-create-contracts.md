---
title: Create Contracts
---

## Create Contracts

In this article, you'll learn how to create Stacks(L1) contract and subnet(L2) contracts.

If you already have an existing contract created using [Hiro Platform](https://platform.hiro.so/) or by following [Add a new contract](https://docs.hiro.so/clarinet/how-to-guides/how-to-add-contract), follow the [how to update existing contract to use with subnets](how-to-update-existing-contract-to-use-with-subnets.md).

This section will teach you how to launch and interact with this devnet subnet environment using a simple NFT example project.

## Create Stacks (L1) Contract

Our L1 NFT contract will implement the [SIP-009 NFT trait](https://github.com/stacksgov/sips/blob/main/sips/sip-009/sip-009-nft-standard.md#trait).

We will add this to our project as a requirement so that Clarinet will deploy it for us.

```sh
clarinet requirements add ST1NXBK3K5YYMD6FD41MVNP3JS1GABZ8TRVX023PT.nft-trait
```

We'll also use a new trait defined for the subnet, `mint-from-subnet-trait,` that allows the subnet to mint a new asset on the Stacks chain if it was originally minted on the subnet and then withdrawn. You can add a requirement for this contract by using the command:

```sh
clarinet requirements add ST1PQHQKV0RJXZFY1DGX8MNSNYVE3VGZJSRTPGZGM.subnet-traits
```

Now, we will use Clarinet to create our L1 contract:

```sh
clarinet contract new simple-nft-l1
```

This creates the file, _./contracts/simple-nft-l1.clar_, which will include the
following clarity code:

```clarity
(define-constant CONTRACT_OWNER tx-sender)
(define-constant CONTRACT_ADDRESS (as-contract tx-sender))

(define-constant ERR_NOT_AUTHORIZED (err u1001))

(impl-trait 'ST1NXBK3K5YYMD6FD41MVNP3JS1GABZ8TRVX023PT.nft-trait.nft-trait)
(impl-trait 'ST1PQHQKV0RJXZFY1DGX8MNSNYVE3VGZJSRTPGZGM.subnet-traits.mint-from-subnet-trait)

(define-data-var lastId uint u0)
(define-map CFG_BASE_URI bool (string-ascii 256))

(define-non-fungible-token nft-token uint)

(define-read-only (get-last-token-id)
  (ok (var-get lastId))
)

(define-read-only (get-owner (id uint))
  (ok (nft-get-owner? nft-token id))
)

(define-read-only (get-token-uri (id uint))
  (ok (map-get? CFG_BASE_URI true))
)

(define-public (transfer (id uint) (sender principal) (recipient principal))
  (begin
    (asserts! (is-eq tx-sender sender) ERR_NOT_AUTHORIZED)
    (nft-transfer? nft-token id sender recipient)
  )
)

;; test functions
(define-public (test-mint (recipient principal))
  (let
    ((newId (+ (var-get lastId) u1)))
    (var-set lastId newId)
    (nft-mint? nft-token newId recipient)
  )
)

(define-public (mint-from-subnet (id uint) (sender principal) (recipient principal))
    (begin
        ;; Check that the tx-sender is the provided sender
        (asserts! (is-eq tx-sender sender) ERR_NOT_AUTHORIZED)

        (nft-mint? nft-token id recipient)
    )
)

(define-public (gift-nft (recipient principal) (id uint))
  (begin
    (nft-mint? nft-token id recipient)
  )
)
```

Note that this contract implements the `mint-from-subnet-trait` and the SIP-009 `nft-trait.` When `mint-from-subnet-trait` is implemented, it allows an NFT to be minted on the subnet, then later withdrawn to the L1.

## Create Subnet (L2) Contract

Next, we will create the subnet contract at _./contracts/simple-nft-l2.clar_. As mentioned earlier, Clarinet does not support deploying subnet contracts yet, so we will manually create this file and add the following contents:

```clarity
(define-constant CONTRACT_OWNER tx-sender)
(define-constant CONTRACT_ADDRESS (as-contract tx-sender))

(define-constant ERR_NOT_AUTHORIZED (err u1001))

(impl-trait 'ST000000000000000000002AMW42H.subnet.nft-trait)

(define-data-var lastId uint u0)

(define-non-fungible-token nft-token uint)


;; NFT trait functions
(define-read-only (get-last-token-id)
  (ok (var-get lastId))
)

(define-read-only (get-owner (id uint))
  (ok (nft-get-owner? nft-token id))
)

(define-read-only (get-token-uri (id uint))
  (ok (some "unimplemented"))
)

(define-public (transfer (id uint) (sender principal) (recipient principal))
  (begin
    (asserts! (is-eq tx-sender sender) ERR_NOT_AUTHORIZED)
    (nft-transfer? nft-token id sender recipient)
  )
)

;; mint functions
(define-public (mint-next (recipient principal))
  (let
    ((newId (+ (var-get lastId) u1)))
    (var-set lastId newId)
    (nft-mint? nft-token newId recipient)
  )
)

(define-public (gift-nft (recipient principal) (id uint))
  (begin
    (nft-mint? nft-token id recipient)
  )
)

(define-read-only (get-token-owner (id uint))
  (nft-get-owner? nft-token id)
)

(impl-trait 'ST000000000000000000002AMW42H.subnet.subnet-asset)

;; Called for deposit from the burnchain to the subnet
(define-public (deposit-from-burnchain (id uint) (recipient principal))
  (begin
    (asserts! (is-eq tx-sender 'ST000000000000000000002AMW42H) ERR_NOT_AUTHORIZED)
    (nft-mint? nft-token id recipient)
  )
)

;; Called for withdrawal from the subnet to the burnchain
(define-public (burn-for-withdrawal (id uint) (owner principal))
  (begin
    (asserts! (is-eq tx-sender owner) ERR_NOT_AUTHORIZED)
    (nft-burn? nft-token id owner)
  )
)
```

This contract implements the `nft-trait` and the `subnet-asset` trait. The `nft-trait` is the same as the [`SIP-009`](https://github.com/stacksgov/sips/blob/a17d318321abf0754e8b2ce5706a9d25493d42ee/sips/sip-009/sip-009-nft-standard.md) trait on the Stacks network. The `subnet-asset` defines the functions required for deposit and withdrawal. The `deposit-from-burnchain` is invoked by the subnet node's consensus logic whenever a deposit is made in layer-1. The `burn-for-withdrawal` is invoked by the `nft-withdraw?` or `ft-withdraw?` functions of the subnet contract, that a user calls when they wish to withdraw their asset from the subnet back to the layer-1.

Now that your contracts are ready, you can start the devnet by following [how to start the Devnet guide.](how-to-start-devnet.md).
