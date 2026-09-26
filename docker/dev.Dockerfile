# Build and test environment: one PHP version + the Rust toolchain.
#   docker build -f docker/dev.Dockerfile --build-arg PHP_VERSION=8.4 -t wprc-dev:8.4 docker
#   ZTS: --build-arg PHP_VARIANT=zts -t wprc-dev:8.4-zts
ARG PHP_VERSION=8.4
ARG PHP_VARIANT=cli
FROM php:${PHP_VERSION}-${PHP_VARIANT}

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      autoconf dpkg-dev file g++ gcc libc-dev make pkg-config re2c \
      curl ca-certificates procps \
 && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal \
      --component clippy,rustfmt \
 && rustc --version

RUN docker-php-ext-install pcntl

WORKDIR /src
