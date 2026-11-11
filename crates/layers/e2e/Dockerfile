# Docker on Ubuntu 26.04 as SuperCI's machines have it (the versions as build arguments), to run docker.sh in.
FROM ubuntu:26.04
ARG DOCKER=5:29.4.2-1~ubuntu.26.04~resolute
ARG CONTAINERD=2.3.6-1~ubuntu.26.04~resolute
RUN apt-get update -qq && apt-get install -y -qq ca-certificates curl iptables jq erofs-utils python3-minimal >/dev/null \
 && install -m 0755 -d /etc/apt/keyrings && curl -fsSL https://download.docker.com/linux/ubuntu/gpg -o /etc/apt/keyrings/docker.asc \
 && echo "deb [signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/ubuntu resolute stable" > /etc/apt/sources.list.d/docker.list \
 && apt-get update -qq && apt-get install -y -qq --allow-downgrades "docker-ce=$DOCKER" "docker-ce-cli=$DOCKER" "containerd.io=$CONTAINERD" docker-buildx-plugin \
 && rm -rf /var/lib/apt/lists/*
COPY docker.sh bucket.py /
ENTRYPOINT ["bash", "/docker.sh"]
