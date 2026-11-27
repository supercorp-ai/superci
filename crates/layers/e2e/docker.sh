#!/bin/bash
# Docker with superci-layers as containerd's snapshotter and content store, against Docker as it comes: the same
# work done each way (pull, run, build, commit, save, load, push, pull back, prune), and what can be compared
# (image ids, what `docker save` writes) the same; once with no store, once with the images' layers served from a
# store (their EROFS files made here as SuperCI's converter makes them, in a local bucket; their links asked for when
# used; one link broken, that layer pulled as usual; one image's layers not in the store, offered and uploaded after
# pulled). Then superci-layers and containerd started again mid-way, and everything still there.
# In a privileged container (see Dockerfile), with superci-layers at /superci-layers.
set -Eeuo pipefail
trap 'echo "FAILED at line $LINENO: $BASH_COMMAND" >&2; for f in /var/log/superci-layers.log /var/log/containerd.log /var/log/dockerd.log; do echo "== $f"; tail -30 "$f"; done' ERR
T() { echo "== $(date +%T) $*"; }
CONF=/etc/containerd/config.toml
# The images: Docker's official ones from AWS's public mirror (Docker Hub limits anonymous pulls), or the mirror
# IMAGES names (CI uses Google's: ECR Public limits what one address pulls in a month, and GitHub's runners share theirs).
L=${IMAGES:-public.ecr.aws/docker/library}
ALPINE=$L/alpine:3.22 BUSYBOX=$L/busybox:1.37 POSTGRES=$L/postgres:17-alpine REGISTRY=$L/registry:2
# An image pulled, asked again a few times when the mirror does not give it (a public mirror at times lists a layer it
# cannot yet give): the last try's failure is the test's.
pull() {
  for wait in 5 10 20 40; do docker pull -q "$1" > /dev/null && return; echo "the pull of $1 failed: again in $wait s" >&2; sleep "$wait"; done
  docker pull -q "$1" > /dev/null
}
cp "$CONF" /root/config.stock.toml
mkdir -p /etc/docker /root/out

layers() { /superci-layers --root=/var/lib/superci-layers --socket=/run/superci-layers.sock --index=/root/index.json >> /var/log/superci-layers.log 2>&1 & }
start() { # $1: stock | superci
  if [ "$1" = superci ]; then
    layers
    for i in $(seq 100); do [ -S /run/superci-layers.sock ] && break; sleep 0.1; done
  fi
  containerd >> /var/log/containerd.log 2>&1 &
  for i in $(seq 100); do [ -S /run/containerd/containerd.sock ] && break; sleep 0.1; done
  dockerd --containerd=/run/containerd/containerd.sock >> /var/log/dockerd.log 2>&1 &
  for i in $(seq 200); do docker info >/dev/null 2>&1 && break; sleep 0.1; done
  docker info >/dev/null
}
stop() {
  pkill dockerd || true; for i in $(seq 100); do pgrep -x dockerd >/dev/null || break; sleep 0.1; done
  pkill -x containerd || true; for i in $(seq 100); do pgrep -x containerd >/dev/null || break; sleep 0.1; done
  pkill -x superci-layers || true; for i in $(seq 100); do pgrep -x superci-layers >/dev/null || break; sleep 0.1; done
}
fresh() {
  stop
  for m in $(awk '$2 ~ "^/var/lib/(docker|containerd|superci-layers)" {print $2}' /proc/mounts | sort -r); do umount -l "$m" || true; done
  rm -rf /var/lib/docker /var/lib/containerd /var/lib/superci-layers /run/containerd /run/superci-layers.sock
}

# The same work each way; what it gives, written to /root/out/$1.
work() {
  local o=/root/out/$1
  T "$1: pull and run"
  pull $ALPINE
  pull $BUSYBOX
  docker run --rm $ALPINE cat /etc/alpine-release > "$o.run"
  docker image inspect -f '{{.Id}}' $ALPINE $BUSYBOX > "$o.ids"
  T "$1: build (two stages, a cache mount, a copy from another image)"
  mkdir -p /root/ctx && cd /root/ctx
  printf 'hello\n' > hello.txt
  cat > Dockerfile <<'DF'
ARG ALPINE BUSYBOX
FROM $ALPINE AS build
RUN --mount=type=cache,target=/var/cache/apk apk add --no-cache jq > /dev/null && jq --version > /jq-version
RUN mkdir -p /out && echo built > /out/built && ln -s built /out/link && rm -f /etc/motd
FROM $BUSYBOX
COPY --from=build /out /out
COPY --from=build /jq-version /jq-version
COPY hello.txt /hello.txt
RUN echo "$(cat /hello.txt) $(cat /out/link)" > /result && rm /hello.txt
DF
  docker build -q --build-arg ALPINE=$ALPINE --build-arg BUSYBOX=$BUSYBOX -t e2e/built:1 . > /dev/null
  docker run --rm e2e/built:1 sh -c 'cat /result /jq-version; ls /hello.txt 2>&1 || true' > "$o.built"
  cd /root
  T "$1: commit, diff, cp, export"
  docker run --name c1 $ALPINE sh -c 'echo changed > /etc/changed; rm /etc/hostname 2>/dev/null; mkdir -p /new/dir' 
  docker diff c1 | sort > "$o.diff"
  docker commit c1 e2e/committed:1 > /dev/null
  docker cp c1:/etc/changed "$o.cp"
  docker export c1 | tar -t | sort > "$o.export"
  docker rm c1 > /dev/null
  docker run --rm e2e/committed:1 cat /etc/changed > "$o.committed"
  T "$1: save, remove, load"
  docker save $ALPINE -o /root/alpine-$1.tar
  sha256sum < /root/alpine-$1.tar > "$o.save"
  # (A build's layers carry the times its files were made: what a save holds is compared, not its digests.)
  docker save e2e/built:1 | tar -t | sed -E 's/[0-9a-f]{64}/DIGEST/' | sort | uniq -c > "$o.save-built"
  docker rmi -f e2e/committed:1 > /dev/null
  docker save e2e/built:1 -o /root/built.tar && docker rmi e2e/built:1 > /dev/null && docker load -q -i /root/built.tar > /dev/null
  docker run --rm e2e/built:1 cat /result >> "$o.built"
  T "$1: push to a registry and pull it back"
  docker run -d --name reg -p 127.0.0.1:5000:5000 $REGISTRY > /dev/null
  for i in $(seq 50); do curl -q -sf http://127.0.0.1:5000/v2/ > /dev/null && break; sleep 0.2; done
  docker tag e2e/built:1 127.0.0.1:5000/built:1
  docker push -q 127.0.0.1:5000/built:1 > /dev/null
  docker rmi 127.0.0.1:5000/built:1 e2e/built:1 > /dev/null
  docker pull -q 127.0.0.1:5000/built:1 > /dev/null
  docker run --rm 127.0.0.1:5000/built:1 cat /result >> "$o.built"
  docker rm -f reg > /dev/null
  T "$1: volumes, disk usage, prune"
  docker volume create v1 > /dev/null
  docker run --rm -v v1:/v $ALPINE sh -c 'echo kept > /v/f'
  docker run --rm -v v1:/v $BUSYBOX cat /v/f > "$o.volume"
  docker system df --format '{{.Type}}' > "$o.df"
  docker image prune -a -f > /dev/null
  docker images -q | wc -l > "$o.left"
}

# (PHASES="store" for the store's run alone, while working on it; all three by default.)
PHASES=${PHASES:-stock superci store}
want() { [[ " $PHASES " == *" $1 "* ]]; }

if want stock; then
T "stock: Docker as it comes"
fresh; start stock
[ "$(docker info -f '{{.Driver}}')" = overlayfs ] || { echo "stock Docker's driver is $(docker info -f '{{.Driver}}')" >&2; exit 1; }
work stock
fi

T "the store: each layer of the images used made an EROFS file, as SuperCI's converter makes it, in a bucket"
fresh; start stock
mkdir -p /root/bucket /root/oci
for img in $ALPINE $BUSYBOX $POSTGRES; do pull "$img"; done
docker save $ALPINE $BUSYBOX $POSTGRES | tar -x -C /root/oci
python3 - <<'PY' > /root/layers.txt
import json, os
seen = set()
def walk(d, image):
    p = f"/root/oci/blobs/sha256/{d.split(':')[1]}"
    if not os.path.exists(p): return  # (another platform's: not saved)
    b = json.load(open(p))
    for m in b.get("manifests", []): walk(m["digest"], image)
    for l in b.get("layers", []):
        if l["digest"] not in seen and l["mediaType"].endswith("gzip"):
            seen.add(l["digest"]); print(l["digest"], l["size"], image)
for m in json.load(open("/root/oci/index.json"))["manifests"]: walk(m["digest"], m.get("annotations", {}).get("io.containerd.image.name", ""))
PY
# (Busybox's layers are not in the store: pulled as usual, then offered and uploaded after the job.)
grep " $BUSYBOX\$" /root/layers.txt | cut -d' ' -f1 > /root/not-stored.txt
[ -s /root/not-stored.txt ]
grep -v " $BUSYBOX\$" /root/layers.txt > /root/stored.txt
echo '{"layers": {' > /root/index.json
n=0; total=0
while read -r d size _; do
  h=${d#sha256:}
  mkfs.erofs --quiet --tar=f --ungzip --aufs -zlz4hc -C131072 -Enoinline_data -T0 --mkfs-time -U 39bb3f52-5f38-4d6c-a25a-5ea4cc1d8b11 "/root/bucket/$h.erofs" "/root/oci/blobs/sha256/$h"
  cp "/root/oci/blobs/sha256/$h" "/root/bucket/$h.blob"
  es=$(stat -c %s "/root/bucket/$h.erofs"); ed=$(sha256sum "/root/bucket/$h.erofs" | cut -c1-64)
  url="http://127.0.0.1:8000/$h.erofs"
  [ $n = 2 ] && url="http://127.0.0.1:8000/broken/$h.erofs" && broken=$d
  [ $n -gt 0 ] && echo , >> /root/index.json
  printf '"%s": {"size": %s, "erofs": {"url": "%s", "size": %s, "digest": "sha256:%s"}, "blob": {"url": "http://127.0.0.1:8000/%s.blob"}}' "$d" "$size" "$url" "$es" "$ed" "$h" >> /root/index.json
  n=$((n+1)); total=$((total+es))
done < /root/stored.txt
echo '}}' >> /root/index.json
jq -e '.layers | length' /root/index.json > /dev/null
echo "  $n layers, $((total/1024)) KB of EROFS files; one link broken: $broken; not in the store: $(wc -l < /root/not-stored.txt)"
# The machine's index has no links: they are asked for when a layer is used (the bucket's /links has them).
mv /root/index.json /root/bucket/index.links.json
jq '.layers |= map_values(.erofs.url = "" | .blob.url = "") | .refresh = {"url": "http://127.0.0.1:8000/links", "token": ""}' /root/bucket/index.links.json > /root/index.store.json
python3 /bucket.py /root/bucket &

sed 's|^disabled_plugins = \[\(.*\)\]|disabled_plugins = [\1, "io.containerd.content.v1.content"]|; s|\["", |[|' /root/config.stock.toml > "$CONF"
grep -q 'io.containerd.content.v1.content' "$CONF" || echo 'disabled_plugins = ["io.containerd.content.v1.content"]' >> "$CONF"
printf '\n[proxy_plugins]\n  [proxy_plugins.superci]\n    type = "snapshot"\n    address = "/run/superci-layers.sock"\n  [proxy_plugins.superci-content]\n    type = "content"\n    address = "/run/superci-layers.sock"\n' >> "$CONF"
echo '{"storage-driver": "superci", "features": {"containerd-snapshotter": true}}' > /etc/docker/daemon.json

if want superci; then
T "superci: containerd's content store off, superci-layers for snapshots and content, no store"
fresh
start superci
[ "$(docker info -f '{{.Driver}}')" = superci ] || { echo "Docker's driver is $(docker info -f '{{.Driver}}')" >&2; exit 1; }
work superci

[ -n "$(ls /var/lib/superci-layers/overlay/snapshots)" ] || { echo "no snapshot was made in superci-layers" >&2; exit 1; }
[ -n "$(ls /var/lib/containerd/io.containerd.content.v1.content/blobs/sha256 2>/dev/null)" ] || { echo "no blob was written through superci-layers" >&2; exit 1; }
! grep -q "from the store" /var/log/superci-layers.log
fi

T "store: the same with the images' layers served from the store"
fresh
mv /root/index.store.json /root/index.json
: > /var/log/superci-layers.log
start superci
work store
pull $POSTGRES
docker run --rm $POSTGRES postgres --version > /root/out/store.postgres
served=$(grep -c "from the store" /var/log/superci-layers.log || true)
usual=$(grep -c "pulled as usual" /var/log/superci-layers.log || true)
echo "  served from the store: $served; pulled as usual: $usual; blobs read from the store: $(grep -c 'read from the store' /var/log/superci-layers.log || true)"
grep "pulled as usual" /var/log/superci-layers.log | grep -q "$broken" || { echo "the broken layer was not pulled as usual" >&2; exit 1; }
[ "$served" -ge 3 ] && [ "$usual" -ge 1 ]
# Each layer's file mounted read-only, with no devices and no set-user-ID programs taken from it.
mounts=$(awk '$3 == "erofs" {print $4}' /proc/mounts)
[ -n "$mounts" ] || { echo "no layer's file mounted" >&2; exit 1; }
echo "$mounts" | while read -r opts; do case ",$opts," in *,ro,*) ;; *) echo "a layer mounted $opts" >&2; exit 1 ;; esac; case ",$opts," in *,nosuid,*) ;; *) echo "a layer mounted $opts" >&2; exit 1 ;; esac; case ",$opts," in *,nodev,*) ;; *) echo "a layer mounted $opts" >&2; exit 1 ;; esac; done
# Nothing offered while the job runs. After it (as a machine does before it goes off): Busybox's layers, which the
# store lacks, offered with where they came from and uploaded whole; what came from the store said.
[ ! -e /root/bucket/proposed.txt ] || { echo "offered during the job" >&2; exit 1; }
# (The store switched in, as its setup says on a machine: what came from it is counted then only.)
echo "on: started by this test" > /root/layers.state
/superci-layers keep --link=http://127.0.0.1:8000 --index=/root/index.json --most=60 --state=/root/layers.state >> /var/log/superci-layers.log 2>&1
jq -e '.served >= 3 and .pulled >= 1 and .uploaded >= 1' /root/bucket/kept.json > /dev/null || { echo "kept: $(cat /root/bucket/kept.json)" >&2; exit 1; }
while read -r d; do
  grep -q "^$d .*$L/busybox" /root/bucket/proposed.txt || { echo "$d was not offered with its source" >&2; cat /root/bucket/proposed.txt >&2; exit 1; }
  grep -q "^$d " /root/bucket/uploaded.txt || { echo "$d was not uploaded" >&2; exit 1; }
  [ "sha256:$(sha256sum "/root/bucket/staging/${d#sha256:}" | cut -c1-64)" = "$d" ]
done < /root/not-stored.txt
! cut -d' ' -f1 /root/stored.txt | grep -q -F -f - /root/bucket/proposed.txt || { echo "a layer the store has was offered" >&2; exit 1; }
echo "  offered and uploaded after the job: $(sort -u /root/bucket/uploaded.txt | wc -l) layer(s); kept: $(cat /root/bucket/kept.json); links asked for: yes"

want stock && want superci || { T "passed (the store's run)"; exit 0; }
T "the same each way"
bad=0
for f in run ids built diff cp export committed save save-built volume df left; do
  for way in superci store; do
    if cmp -s "/root/out/stock.$f" "/root/out/$way.$f"; then echo "  same: $f ($way)"; else echo "  DIFFERENT: $f ($way)"; diff "/root/out/stock.$f" "/root/out/$way.$f" | head -10 || true; bad=1; fi
  done
done
grep -q "PostgreSQL" /root/out/store.postgres || bad=1
# (Saves that differ: what in them.)
for way in superci store; do
  cmp -s /root/alpine-stock.tar "/root/alpine-$way.tar" && continue
  echo "  alpine saved with $way differs from stock:"
  diff <(tar -tvf /root/alpine-stock.tar) <(tar -tvf "/root/alpine-$way.tar") | head -20 || true
  rm -rf /root/s1 /root/s2 && mkdir -p /root/s1 /root/s2 && tar -xf /root/alpine-stock.tar -C /root/s1 && tar -xf "/root/alpine-$way.tar" -C /root/s2
  diff -r /root/s1 /root/s2 | head -20 || true
  for f in index.json manifest.json; do diff <(jq -S . /root/s1/$f) <(jq -S . /root/s2/$f) || true; done
done
[ $bad = 0 ]

T "superci-layers started again while Docker runs; then everything started again"
pull $ALPINE
docker run -d --name live $ALPINE sleep 600 > /dev/null
pkill -x superci-layers; sleep 0.5
layers
for i in $(seq 100); do [ -S /run/superci-layers.sock ] && break; sleep 0.1; done; sleep 0.5
docker run --rm $ALPINE echo "a container after superci-layers came back" 
docker exec live echo "the running container kept going"
docker rm -f live > /dev/null
stop; start superci
docker run --rm $ALPINE echo "after containerd, Docker and superci-layers all started again"
docker system prune -a -f > /dev/null
docker images -q | wc -l | grep -qx 0
T "passed"
