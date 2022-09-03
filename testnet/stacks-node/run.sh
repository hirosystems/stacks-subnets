# rm -rf ~/hc-space/*

export BLOCKSTACK_DEBUG=0

cargo build --bin hyperchain-node --release && \
	../../target/release/hyperchain-node start --config conf/l2-miner-profiling.toml 2>&1 | ~/tracer/bin/trace-l2.sh
