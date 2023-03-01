# Subnets Core Contracts

Essential Clarity contracts for using Stacks Subnets

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
