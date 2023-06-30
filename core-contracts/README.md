# Subnets Core Contracts

This directory contains the contracts published to the Stacks L1 to implement a subnet.
* _subnet.clar_: interface between the subnet and the L1
* _multi-miner.clar_: implements a multi-miner for the subnet
* _helper/*_: used for testing

## Running Tests

To run the tests and generate a code coverage report, run this from the repository root:

```sh
clarinet test --coverage --manifest-path=./core-contracts/Clarinet.toml --import-map=./core-contracts/import_map.json --allow-net --allow-read
```

Or if you have `just` installed:

```sh
just clarinet-test # Run tests with locally installed Clarinet
just clarinet-test-docker # Run tests with latest development build from DockerHub
```

To generate HTML from the code coverage report and view it:

```sh
mkdir coverage
cd coverage
genhtml ../coverage.lcov
# Open with your preferred browser
brave index.html
```
