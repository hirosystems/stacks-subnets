---
title: Miner resource recommendation
---

# Miner resource recommendation

As a miner, you can set up Subnet on the mainnet by following the resource recommendations in this document.

## Features

- Higher CPU speed results in higher throughput and lower latency
- Higher RAM will lead to faster transaction validation and faster blocks
- An additional layer on Stacks warrants more disk space to accommodate the additional Subnets’ blocks and microblocks 

## What problem does it solve?

The specifications below help you increase the transaction throughput by 4X times and reduce the block confirmation time from 10 minutes to 1 minute.

## Virtual Machine Specifications

We assume a [Debian](https://www.debian.org/) host with `x86_64` architecture for the following example. Note that the commands may also work on any Debian-derived distribution. You can run your machine on Google Compute Engine(GCE) and choose between the options below.

> [!NOTE]
> Bitcoin chainstate is approximately 529GB, and Stacks chainstate is approximately 84GB.

### Option 1

- Run on GCE n2-standard-4 (4 vCPUs, greater than or equal to 16GB memory) instances with 2048GB SSD 
- Annual cost is approximately $1100 per year
- Minimum CPU greater than or equal to 4 vCPUs
- Minimum Memory greater than or equal to 16GB Memory
- Minimum Storage is 2TB Disk to allow chainstate growth

### Option 2

- Run on GCE n2-standard-32(32 vCPUs, greater than or equal to 128GB memory) instances with 2048GB SSD
- Annual cost is approximately $10890 per year

## Hardware Specifications
 
The following are the hardware specifications for running a Subnet on the Mainnet.

### CPU

- Greathan or equal to 2.8 GHz
- Greathan or equal to 12 Core, 24 Threads

### RAM

- Preferred: greater than or equal to 128GB
- Minimum 48GB
- Motherboard greater than or equal to 256GB capacity

### Disk

- NVMe-based SSD storage: You can use local SSDs through SCSI interfaces. For higher performance in production settings, we recommend upgrading to NVMe interfaces
- High TBW (Total Bytes Written)
- greater than or equal to 3.4GB per second sequential read and sequential write performance
- greater than or equal to 1TB to allow for chainstate growth

### Networking

- Preferred 1GBit/s
- Minimum: 300Mbit/s symmetric, commercial
