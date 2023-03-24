---
# The default id is the same as the one defined below. so not needed
title: Getting Started
---

# Getting Started

Once you understand the [overview](overview.md) of Subnets, you can use this guide to get started with subnets. 

There are two ways to test your applications on a subnet. Running a local subnet or interacting with a subnet on a testnet environment.

- Run a local subnet
  - Creating a new project with Clarinet
  - Creating Contracts
  - Start Devnet
  - Setup Node.js scripts
  - Interact with subnet
  - Enable event observer interface
- Use Hiro's subnet on testnet
  - Publish the NFT contract on the subnet
  - Mint a new NFT in the stacks network
  - Deposit this NFT into the subnet
  - Transfer the NFT from one user to another in the subnet
  - Withdraw the NFT from the subnet

> **_NOTE:_**
>
> A subnet was previously referred to as a hyperchain. While the process of
> updating the content is ongoing; there may still be some references to a
> hyperchain instead of a subnet.

## Create a new project with Clarinet

This guide walks you through the first step of running a local subnet which is creating a new project with Clarinet.

Clarinet provides a tool to set up a complete local development environment, referred to as "devnet," which uses Docker to spin up a Bitcoin node, a Stacks node, a Stacks API node, a Stacks Explorer, and now, a subnet node and subnet API node. This allows developers to test locally on a system that matches the production environment.

Make sure you have [`clarinet`](https://github.com/hirosystems/clarinet/releases/tag/v1.5.3) installed and the **clarinet version is at 1.5.3 or above**. If you do not already have Clarinet installed, you can refer to the instructions [here](https://docs.hiro.so/smart-contracts/clarinet#installing-clarinet) for installation procedures.

To create a new project, run:

```sh
clarinet new subnet-nft-example
cd subnet-nft-example
```

This command creates a new directory, 'subnet-nft-example', with a clarinet project already initialized and then switches into that directory.

You can now create L1 Stacks contract and L2 subnets contracts by following the [create contracts](how-to-guides/how-to-create-contracts.md) article.
