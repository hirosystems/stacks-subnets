rm -rf ~/hc-space/*

export STACKS_FILTER="DEBG|trie_sql.rs|node.rs|storage.rs|marf.rs|cache.rs|p2p.rs|inv.rs|prune.rs|trie.rs|db.rs|neighbors.rs|processing.rs|burn.mod.rs|blocks.rs"

# cargo build --bin hyperchain-node --release && ../../target/release/hyperchain-node start --config conf/aaron-conf.toml 2>&1 | ~/tracer/bin/filter-tracer.sh

cargo build --bin hyperchain-node --release && \
	../../target/release/hyperchain-node start --config conf/greg-conf.toml 2>&1 | ~/tracer/bin/trace-l2.sh
