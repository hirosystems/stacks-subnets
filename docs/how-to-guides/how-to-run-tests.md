---
title: How to Run Tests
---

## NFT Testing

Tests can often be helpful in ensuring NFTs are properly managed and maintained (for example, minted, published, registered).

You can run tests by navigating to the `testnet/stacks-node/` directory and run the following command:

```
testnet/stacks-node$ cargo test
```

To ignore some tests, navigate to the `testnet/stacks-node` directory and use the following command:

```
testnet/stacks-node$ cargo test -- --ignored --num-threads=1
```