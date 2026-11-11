# A SuperCI machine as `superci-layers setup` finds one: Ubuntu 26.04 with systemd as its first process, and Docker
# and containerd from Docker's own packages (the versions as build arguments), started by systemd. Run privileged.
FROM ubuntu:26.04
ARG DOCKER=5:29.4.2-1~ubuntu.26.04~resolute
ARG CONTAINERD=2.3.6-1~ubuntu.26.04~resolute
RUN apt-get update -qq && apt-get install -y -qq systemd systemd-sysv ca-certificates curl iptables jq erofs-utils e2fsprogs util-linux python3-minimal >/dev/null \
 && install -m 0755 -d /etc/apt/keyrings && curl -fsSL https://download.docker.com/linux/ubuntu/gpg -o /etc/apt/keyrings/docker.asc \
 && echo "deb [signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/ubuntu resolute stable" > /etc/apt/sources.list.d/docker.list \
 && apt-get update -qq && apt-get install -y -qq --allow-downgrades "docker-ce=$DOCKER" "docker-ce-cli=$DOCKER" "containerd.io=$CONTAINERD" >/dev/null \
 && rm -rf /var/lib/apt/lists/* \
 && systemctl enable docker containerd >/dev/null 2>&1 \
 && for u in systemd-resolved systemd-timesyncd getty@tty1 console-getty; do systemctl mask $u >/dev/null 2>&1 || true; done
STOPSIGNAL SIGRTMIN+3
ENTRYPOINT ["/sbin/init"]
