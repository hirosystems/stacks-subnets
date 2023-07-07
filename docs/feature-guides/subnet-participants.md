---
title: Subnet Participants
---

## Subnet Participants

This document helps you understand the participant criteria to run a Subnet. The participants are classified into two categories.

- The **primary participants** need to agree on trust parameters and incentives to launch a Subnet.
- The **secondary participants** are required to illustrate different parties involved when using/interacting with a Subnet on Stacks.

> [!NOTE]
> Hiro does not intend to participate in running a Subnet on Stacks.

## Primary Participants

### Miners

The following are the required entities to run a Subnet.

- The initial version of Subnets uses a Byzantine Fault Tolerance (BFT) consensus mechanism, and the miners are a Federated pool of fully trusted miners
- Miners have arbitrary control over STX deposited on the Subnets. A minimum of three miners will be required for the Subnet’s BFT consensus to materialize and support a 2n/3 majority
- Miners are motivated by Subnet transaction fees. The Supporting applications and the use case will set the Subnet fees
- Miners will require specific hardware/ software to validate and process Subnet transactions. Miners may include the supporting application, use case, and facilitator or none of these.

**Key Activities**

- Miners are responsible for approving and signing transactions and submitting them to the Stacks chain for finality
- Miners must agree to Subnet parameters and incentives. (Example: transaction fees, duration, performance requirements)

### Supporting Application (Example: NFT Marketplace)

A supporting application supports Subnets and owns the user experience. For an NFT use case, the supporting application might be an NFT marketplace. For a DeFi use case, the supporting application might be an exchange, and it may be independent of miners or elect to participate as a miner.

**Key Activities**

- Own user experience for people who want to use a Subnet as an application, specifically support connecting a Stacks Wallet to Subnet and thereby minting and trading on a Subnet
- Must agree to Subnet parameters and incentives (For example transaction fees, duration, and performance requirements)

### Supporting UseCase (Example: NFT Collection Mint)

- A supporting use case refers to the event that drives people to use Subnet, for example, a mint for a popular NFT collection.
- The supporting use case includes any other parties involved with the use case. In the example above, it could be the artist for a mint collection. The supporting application may be independent of miners.

**Key Activities**

- Must agree to Subnet parameters and incentives (For example transaction fees, duration, and performance requirements)

## Secondary participants

These participants use a Subnet and do not need to agree to the parameters and incentives of a Subnet.

### Wallets

Users interact with applications using wallets, so Subnet support is crucial.

**Key Activities**

- Own user experience for people who want to use a Subnet via a wallet
- Deposit of STX from Stacks main chain to a Subnet
- Connect to Subnets for mints and transfers
- Support withdrawals of STX and NFTs from Subnet to Stacks main chain
- Display Subnet address balances

### End Users

- Users interact with Subnets to experience faster throughput and lower latency for specific use cases (dEFI trades or purchasing or trading NFTs).

- Users interact primarily with wallets and supporting applications to complete the specific use case (NFT mint)
