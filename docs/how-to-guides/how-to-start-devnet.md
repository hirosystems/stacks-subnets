---
title: How to start Devnet
---

In this guide, you will manually deploy contracts to subnets as the Clarinet support to deploy the contracts is in the roadmap.

### Start devnet

The settings for the devnet are found in _./settings/Devnet.toml_. In order to launch a subnet in the devnet, we need to tell Clarinet to enable a subnet node and a corresponding API node.

Add, or uncomment, the following line under `[devnet.toml]`:

```toml
enable_subnet_node = true
```

Also, in that file, we can see a few of the default settings that `clarinet` will be using for our subnet. `subnet_contract_id` specifies the L1 contract that the subnet will be interacting with. 

```toml
subnet_contract_id = "ST13F481SBR0R7Z6NMMH8YV2FJJYXA5JPA0AD3HP9.subnet-v1-1"
```
This will be automatically downloaded from the network and deployed by `clarinet`, but you can take a look at it [here](https://explorer.hiro.so/txid/0x928db807c802078153009524e8f7f062ba45371e72a763ce60ed04a70aaefddc?chain=testnet).


`subnet_node_image_url` and `subnet_api_image_url` specify the docker images that will be used for the subnet node and the subnet API node, respectively.

```toml
subnet_node_image_url = "hirosystems/stacks-subnets:0.4.0"
subnet_api_image_url = "hirosystems/stacks-blockchain-api:7.1.0-beta.2"
```

For a custom subnet implementation, you can modify the above values.

Once the configuration is complete, run the following command to start the devnet environment:

```sh
clarinet integrate
```

This will launch docker containers for a bitcoin node, a Stacks node, the Stacks API service, a subnet node, the subnet API service, and an explorer service. While running, `clarinet integrate` opens a terminal UI that shows various data points about the state of the network.

All of the nodes and services are running and ready when we see:

![Clarinet integrate services](images/subnet-devnet.png)

Once this state is reached, we should see successful calls to `commit-block` in the transactions console. This is the subnet miner committing blocks to the L1. Leave this running and perform the next steps in another terminal.
