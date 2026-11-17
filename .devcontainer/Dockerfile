FROM mcr.microsoft.com/devcontainers/rust:2-1-trixie

# mosquitto: broker for the vcontrol-mqtt tests. gcc-arm-linux-gnueabihf: C compiler and linker for the Raspberry Pi build.
RUN apt-get update && export DEBIAN_FRONTEND=noninteractive \
    && apt-get -y install --no-install-recommends 7zip curl mosquitto openssl gcc-arm-linux-gnueabihf libc6-dev-armhf-cross

RUN rustup target add armv7-unknown-linux-musleabihf

# Debian ships no musl cross gcc; the glibc one works for `ring` and as the linker driver.
ENV CC_armv7_unknown_linux_musleabihf=arm-linux-gnueabihf-gcc \
    CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_LINKER=arm-linux-gnueabihf-gcc
