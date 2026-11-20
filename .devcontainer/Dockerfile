FROM mcr.microsoft.com/devcontainers/rust:2-1-trixie

# mosquitto: broker for the vcontrol-mqtt tests. gcc-arm-linux-gnueabihf: C compiler and linker for the Raspberry Pi build.
RUN apt-get update && export DEBIAN_FRONTEND=noninteractive \
    && apt-get -y install --no-install-recommends 7zip curl mosquitto openssl gcc-arm-linux-gnueabihf libc6-dev-armhf-cross

RUN rustup target add armv7-unknown-linux-musleabihf arm-unknown-linux-musleabihf

# rustfmt.toml uses nightly-only options: format with `cargo +nightly fmt`.
RUN rustup toolchain install nightly --profile minimal --component rustfmt

# Debian ships no musl cross gcc; the glibc one works for `ring` and as the linker driver.
ENV CC_armv7_unknown_linux_musleabihf=arm-linux-gnueabihf-gcc \
    CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_LINKER=arm-linux-gnueabihf-gcc

# ARMv6 (Raspberry Pi 1 and Zero). The Debian gcc defaults to ARMv7, so the C code is pinned to ARMv6.
ENV CC_arm_unknown_linux_musleabihf=arm-linux-gnueabihf-gcc \
    CFLAGS_arm_unknown_linux_musleabihf="-march=armv6 -mfpu=vfp -mfloat-abi=hard -marm" \
    CARGO_TARGET_ARM_UNKNOWN_LINUX_MUSLEABIHF_LINKER=arm-linux-gnueabihf-gcc

# Builds the .deb packages (`just dist`).
RUN cargo install cargo-deb --locked --version 3.8.0
