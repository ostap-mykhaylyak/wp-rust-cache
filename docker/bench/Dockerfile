# One "server": nginx + PHP-FPM + MariaDB + Redis + Memcached + WordPress,
# so every backend runs on the same hardware, PHP, FPM pool and workload.
#   docker build -f docker/bench/Dockerfile -t wprc-bench .
ARG PHP_VERSION=8.4

FROM wprc-dev:${PHP_VERSION} AS build
COPY . /src
RUN cd /src && sh php-extension/build.sh > /so-path \
 && cp "$(cat /so-path)" /wp_rust_cache.so \
 && cargo build --release -p wp-rust-cache -p wprc-bench \
 && cp target/release/wp-rust-cache target/release/wprc-bench /usr/local/bin/

FROM php:${PHP_VERSION}-fpm
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      nginx mariadb-server redis-server memcached wrk curl unzip less procps \
      libmemcached-dev zlib1g-dev libssl-dev libzip-dev libicu-dev \
      $PHPIZE_DEPS \
 && docker-php-ext-install mysqli pdo_mysql opcache pcntl zip intl bcmath \
 && pecl install redis memcached memcache \
 && docker-php-ext-enable redis memcached memcache \
 && apt-get purge -y --auto-remove $PHPIZE_DEPS \
 && rm -rf /var/lib/apt/lists/* /tmp/pear

COPY --from=build /wp_rust_cache.so /tmp/
RUN cp /tmp/wp_rust_cache.so "$(php -r 'echo ini_get("extension_dir");')/" \
 && printf 'extension=wp_rust_cache.so\nwp_rust_cache.config=/etc/wp-rust-cache/config.toml\n' \
      > /usr/local/etc/php/conf.d/50-wp_rust_cache.ini
COPY --from=build /usr/local/bin/wp-rust-cache /usr/local/bin/wprc-bench /usr/local/bin/
RUN curl -fsSL -o /usr/local/bin/wp https://raw.githubusercontent.com/wp-cli/builds/gh-pages/phar/wp-cli.phar \
 && chmod +x /usr/local/bin/wp

COPY docker/bench/php.ini /usr/local/etc/php/conf.d/90-bench.ini
COPY docker/bench/www.conf /usr/local/etc/php-fpm.d/zz-bench.conf
COPY docker/bench/nginx.conf /etc/nginx/sites-enabled/default
COPY docker/bench/config.toml /etc/wp-rust-cache/config.toml
COPY docker/bench/*.sh docker/bench/*.php docker/bench/*.lua /bench/
COPY wordpress/object-cache.php /bench/wp-rust-cache-object-cache.php
COPY tests/php /bench/tests

# Keep both the site and its database across container rebuilds:
#   docker run -d --init --name wprc-srv --memory=5g --shm-size=1g \
#     -v wprc-www:/var/www/html -v wprc-db:/var/lib/mysql wprc-bench
ENTRYPOINT ["/bin/bash", "/bench/start.sh"]
CMD ["sleep", "infinity"]
