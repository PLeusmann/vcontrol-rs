# Everything runs in the vcontrol-dev container (.devcontainer); the host needs only docker and just.

container := "vcontrol-dev"
exec := "docker exec " + container
features := "--features mqtt"
# Raspberry Pi 2 and later (armv7l), Pi 1 and Zero (armv6l).
pi_targets := "armv7-unknown-linux-musleabihf arm-unknown-linux-musleabihf"

default:
    @just --list

# Build the dev image; (re)create the container when it is missing or runs an older image.
container:
    #!/bin/sh
    set -eu
    docker build -q -t {{container}} .devcontainer >/dev/null
    image=$(docker image inspect -f '{{{{.Id}}' {{container}})
    if [ "$(docker inspect -f '{{{{.Image}}' {{container}} 2>/dev/null)" != "$image" ]; then
        docker rm -f {{container}} >/dev/null 2>&1 || true
        docker run -d --name {{container}} -u root -v "$PWD":/work -w /work \
            -v vcontrol-target:/target -v vcontrol-cargo-registry:/usr/local/cargo/registry \
            -e CARGO_TARGET_DIR=/target {{container}} sleep infinity >/dev/null
    fi
    docker start {{container}} >/dev/null

build: container
    {{exec}} cargo build {{features}}

test: container
    {{exec}} cargo test {{features}}

clippy: container
    {{exec}} cargo clippy {{features}} --all-targets

# Build the .deb for each target into target/dist/<target>/.
dist *targets=pi_targets: container
    for t in {{targets}}; do {{exec}} cargo deb --target "$t" --output "target/dist/$t/" || exit 1; done
