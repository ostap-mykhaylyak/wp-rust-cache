# FrankenPHP (PHP ZTS, one process, many threads) with the extension built
# for it: the threaded SAPI a Caddy-based stack uses.
#   docker build -f docker/frankenphp/Dockerfile -t wprc-frankenphp .
FROM dunglas/frankenphp:php8.5

RUN apt-get update -qq \
 && apt-get install -y -qq --no-install-recommends autoconf gcc g++ make pkg-config curl ca-certificates procps >/dev/null \
 && rm -rf /var/lib/apt/lists/*
ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo PATH=/usr/local/cargo/bin:$PATH
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal >/dev/null

COPY . /src
RUN cd /src && sh php-extension/build.sh > /so \
 && cp "$(cat /so)" "$(php -r 'echo ini_get("extension_dir");')/" \
 && printf 'extension=wp_rust_cache.so\nwp_rust_cache.config=/src/tests/php/test.toml\n' \
      > /usr/local/etc/php/conf.d/50-wp_rust_cache.ini \
 && php -r 'echo PHP_VERSION, PHP_ZTS ? " ZTS" : " NTS", "\n";' \
 && php -m | grep wp_rust_cache

COPY docker/frankenphp/app /app
COPY docker/frankenphp/Caddyfile /etc/frankenphp/Caddyfile
COPY docker/frankenphp/test.sh /test.sh
