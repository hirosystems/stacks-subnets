# Subnets Core Contracts

This directory contains the contracts published to the Stacks L1 to implement a subnet.
* _subnet.clar_: interface between the subnet and the L1
* _multi-miner.clar_: implements a multi-miner for the subnet
* _helper/*_: used for testing

## Running Tests

To run the tests and generate a code coverage report:

```sh
clarinet test --coverage --import-map=./import_map.json --allow-net
```

To generate HTML from the code coverage report and view it:

```sh
mkdir coverage
cd coverage
genhtml ../coverage.lcov
# Open with your preferred browser
brave index.html
```
