docker_tag := "latest"
docker_registry := "localhost:5000"
docker_image := docker_registry + "/subnet-node:" + docker_tag

# Print help message
help:
    @just --list --unsorted
    @echo ""
    @echo "Available variables and default values:"
    @just --evaluate

# Build subnets node
build *args:
    #!/usr/bin/env bash
    pushd testnet/stacks-node
    cargo build {{args}}
    popd

# Build release version subnets node
build-release: (build "--features" "monitoring_prom,slog_json" "--release")

# Build docker image
docker-build:
    DOCKER_BUILDKIT=1 docker build -t {{docker_image}} .
    
# Build and push docker image
docker-push: docker-build
    docker push {{docker_image}}