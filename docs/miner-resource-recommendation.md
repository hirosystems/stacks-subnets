---
title: Miner Resource Recommendation
---

# Miner Resource Recommendation

These are our suggested miner resource requirements to spin up a Subnet Miner.

## Features

- Higher CPU speed results in higher throughput and lower latency
- Higher RAM will lead to faster transaction validation and faster blocks
- An additional layer on Stacks warrants more disk space to accommodate the additional subnets’ blocks and microblocks

## What problem does it solve?

The specifications below help you increase the transaction throughput by 4X times and reduce the block confirmation time from 10 minutes to 1 minute.

## Virtual Machine Specifications

We assume a [Debian](https://www.debian.org/) host with `x86_64` architecture for the following example. Note that the commands may also work on any Debian-derived distribution. You can run your machine on Google Compute Engine(GCE) and choose the specifications below.

:::note

_Bitcoin chainstate is approximately 529 GB, and Stacks chainstate is approximately 84 GB._

:::

- Run on GCE n2-standard-4 (4 vCPUs, greater than or equal to 16 GB memory) instances with 2048 GB SSD
- Annual cost is approximately $1100 per year
- Minimum CPU greater than or equal to 4 vCPUs
- Minimum memory greater than or equal to 16 GB Memory
- Minimum storage is 2 TB Disk to allow chainstate growth

## Hardware Specifications

The following are the hardware specifications for running a subnet on the mainnet.

### CPU

- Processing speed greater than or equal to 2.8 GHz
- CPU greater than or equal to 4 vCPUs

### RAM

The minimum size of the RAM is 16 GB.

### Disk

- NVMe-based Solid State Drive(SSD) storage: You can use local SSDs through Small Computer System Interface(SCSI). For higher performance in production settings, we recommend upgrading to NVMe interfaces:
- high TBW (Total Bytes Written)
- greater than or equal to 3.4 GB per second sequential read and sequential write performance
- greater than or equal to 1 TB to allow for chainstate growth

### Networking

- Preferred 1 GBit/s
- Minimum: 300 MBit/s symmetric, commercial
