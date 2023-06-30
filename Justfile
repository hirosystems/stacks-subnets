docker_tag := "latest"
docker_registry := "localhost:5000"
docker_image := docker_registry + "/subnet-node:" + docker_tag

# Print help message
help:
    @just --list --unsorted
    @echo ""
    @echo "Available variables and default values:"
    @just --evaluate

# Generate Just tab completions for Bash shell
bash-completions:
    #!/usr/bin/env bash
    set -euo pipefail
    dir="$HOME/.local/share/bash-completion/completions"
    mkdir -p "$dir"
    just --completions bash > "$dir/just"

# Build subnets node
build *args: (process-template "mocknet")
    #!/usr/bin/env bash
    set -euo pipefail
    pushd testnet/stacks-node
    cargo build {{args}}

# Wrapper around `cargo test`
test *args: (process-template "mocknet")
    cargo test {{args}}

# Build release version subnets node
build-release: (build "--features" "monitoring_prom,slog_json" "--release")

# Build docker image
docker-build:
    DOCKER_BUILDKIT=1 docker build -t {{docker_image}} .
    
# Build and push docker image
docker-push: docker-build
    docker push {{docker_image}}

# Process template Clarity (and other files) into final forms
process-template env: 
    #!/usr/bin/env bash
    set -euo pipefail
    pushd core-contracts/contracts
    ./process_template.sh templates output/{{env}} config/common.yaml config/{{env}}.yaml

# Process template Clarity (and other files) into final forms for all environments
process-templates: (process-template "mocknet") (process-template "devnet") (process-template "testnet") (process-template "mainnet")

# Run `clarinet test` on our contracts
clarinet-test: (process-template "mocknet")
    clarinet test --coverage --manifest-path=./core-contracts/Clarinet.toml --import-map=./core-contracts/import_map.json --allow-net --allow-read

# Run `clarinet test` using Clarinet from DockerHub
clarinet-test-docker: (process-template "mocknet")
    docker run --workdir /src --rm -v "$PWD:/src" hirosystems/clarinet:develop \
        test --coverage --manifest-path=core-contracts/Clarinet.toml --import-map=core-contracts/import_map.json --allow-net --allow-read

# Run `clarinet check` on our contracts
clarinet-check: (process-template "mocknet")
    #!/usr/bin/env bash
    set -euo pipefail
    pushd core-contracts
    clarinet check

# Run `clarinet check` using Clarinet from DockerHub
clarinet-check-docker: (process-template "mocknet")
    #!/usr/bin/env bash
    set -euo pipefail
    pushd core-contracts
    docker run --workdir /src --rm -v "$PWD:/src" hirosystems/clarinet:develop \
        check