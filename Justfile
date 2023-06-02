# Build subnets node
build *args:
    #!/usr/bin/env bash
    pushd testnet/stacks-node
    cargo build {{args}}
    popd

# Build release version subnets node
build-release: (build "--features" "monitoring_prom,slog_json" "--release")