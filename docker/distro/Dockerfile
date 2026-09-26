# A real distribution with systemd and the distribution's own PHP-FPM, to
# test the Debian package the way a server administrator uses it.
#   docker build -f docker/distro/Dockerfile --build-arg BASE=debian:trixie -t wprc-distro:trixie docker/distro
#   docker run -d --name wprc-trixie --privileged --cgroupns=host \
#     -v /sys/fs/cgroup:/sys/fs/cgroup:rw --tmpfs /run --tmpfs /run/lock \
#     --shm-size=1g wprc-distro:trixie
ARG BASE=debian:trixie
FROM ${BASE}

ENV container=docker DEBIAN_FRONTEND=noninteractive
RUN apt-get update -qq \
 && apt-get install -y -qq systemd systemd-sysv nginx mariadb-server curl ca-certificates \
      php-fpm php-cli php-mysql php-xml php-mbstring php-curl php-zip php-intl less procps >/dev/null \
 && curl -fsSL -o /usr/local/bin/wp https://raw.githubusercontent.com/wp-cli/builds/gh-pages/phar/wp-cli.phar \
 && chmod +x /usr/local/bin/wp \
 && rm -rf /var/lib/apt/lists/*

COPY distro-test.sh /root/distro-test.sh
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
