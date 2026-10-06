//! Docker for GitHub's runner in Cloudflare's containers. Cloudflare's machines cannot make Docker's bridge networks
//! (no IP forwarding; a container's interface cannot even be added), but the machine's own network works fully. So the
//! runner's `docker` is this wrapper (installed ahead of /usr/bin/docker): Docker starts the first time a job uses it;
//! a job's network is only a name, and its containers use the machine's network; services' ports are their own (what
//! `docker port` says, for `job.services.<name>.ports`); service names point at 127.0.0.1 in the machine and in the
//! job's container. Everything else goes to Docker unchanged. Two services on one port would collide.

/// The wrapper, installed at /usr/local/bin/docker in the runner's container.
pub const WRAPPER: &str = r#"#!/bin/bash
real=/usr/bin/docker
state=/tmp/superci-docker
mkdir -p "$state"
# Docker starts on first use (most jobs never need it).
if ! "$real" info >/dev/null 2>&1; then
  (
    flock 9
    if ! "$real" info >/dev/null 2>&1; then
      sudo sh -c 'dockerd --iptables=false --ip6tables=false --ip-forward=false > /tmp/dockerd.log 2>&1 &'
      for i in $(seq 1 150); do "$real" info >/dev/null 2>&1 && break; sleep 0.2; done
    fi
  ) 9>"$state/lock"
fi
# Service names in the job's containers (once all are known, before its first step).
hosts() {
  [ -s "$state/aliases" ] || return 0
  [ "$state/patched" -nt "$state/aliases" ] && return 0
  line="127.0.0.1 $(sort -u "$state/aliases" | tr '\n' ' ')"
  for c in $(cat "$state/jobs" 2>/dev/null); do "$real" exec -u 0 "$c" sh -c "echo '$line' >> /etc/hosts" >/dev/null 2>&1; done
  touch "$state/patched"
}
case "$1" in
  network)
    case "$2" in
      create) name="${@: -1}"; touch "$state/net-$name"; echo "$name"; exit 0 ;;
      rm|remove) for n in "${@:3}"; do rm -f "$state/net-$n"; echo "$n"; done; exit 0 ;;
      prune|connect|disconnect) exit 0 ;;
    esac ;;
  create)
    shift; args=(); ports=(); aliases=(); ours=0
    while [ $# -gt 0 ]; do
      case "$1" in
        --network|--net) if [ -e "$state/net-$2" ]; then args+=(--network host); ours=1; else args+=("$1" "$2"); fi; shift 2 ;;
        --network=*|--net=*) n="${1#*=}"; if [ -e "$state/net-$n" ]; then args+=(--network host); ours=1; else args+=("$1"); fi; shift ;;
        --network-alias|--net-alias) aliases+=("$2"); shift 2 ;;
        --network-alias=*|--net-alias=*) aliases+=("${1#*=}"); shift ;;
        -p|--publish) ports+=("$2"); shift 2 ;;
        --publish=*) ports+=("${1#*=}"); shift ;;
        *) args+=("$1"); shift ;;
      esac
    done
    [ "$ours" = 1 ] || { ports=(); aliases=(); }
    out=$("$real" create "${args[@]}"); status=$?
    echo "$out"
    id=$(echo "$out" | tail -1)
    if [ "$status" = 0 ] && [ "$ours" = 1 ] && [ -n "$id" ]; then
      printf '%s\n' "${ports[@]}" > "$state/ports-$id"
      if [ ${#aliases[@]} -gt 0 ]; then
        printf '%s\n' "${aliases[@]}" >> "$state/aliases"
        sudo sh -c "echo '127.0.0.1 ${aliases[*]}' >> /etc/hosts" 2>/dev/null
      else
        echo "$id" >> "$state/jobs"
      fi
    fi
    exit $status ;;
  run)
    shift; args=()
    while [ $# -gt 0 ]; do
      case "$1" in
        --network|--net) if [ -e "$state/net-$2" ]; then args+=(--network host); else args+=("$1" "$2"); fi; shift 2 ;;
        --network=*|--net=*) n="${1#*=}"; if [ -e "$state/net-$n" ]; then args+=(--network host); else args+=("$1"); fi; shift ;;
        --network-alias|--net-alias) shift 2 ;;
        --network-alias=*|--net-alias=*) shift ;;
        *) args+=("$1"); shift ;;
      esac
    done
    exec "$real" run "${args[@]}" ;;
  port)
    id=$("$real" inspect --format '{{.Id}}' "$2" 2>/dev/null)
    f="$state/ports-$id"; [ -e "$f" ] || f="$state/ports-$2"
    if [ -e "$f" ]; then
      while read -r p; do
        [ -n "$p" ] || continue
        c="${p##*:}"; proto=tcp
        case "$c" in */udp) proto=udp ;; esac
        c="${c%/*}"
        echo "$c/$proto -> 0.0.0.0:$c"
      done < "$f"
      exit 0
    fi ;;
  exec) hosts ;;
esac
exec "$real" "$@"
"#;

/// The runner inside GitHub's full image, which a Cloudflare container's disk cannot hold: the image stays where it
/// is published (`SUPERCI_IMAGE`, a public address; see the `image-reader` crate) and shows up here as one file, mounted
/// read-only with a writable layer on the container's own disk; only what a job reads is fetched. GitHub's runner
/// (this container's own, at /opt/actions-runner inside) then starts in it as `runner`, with the image's environment
/// (/etc/environment, read as names and values, never run), as on GitHub's machines. Docker keeps its data on the
/// container's disk.
///
/// An address that ends in its index's checksum (as `image-reader pack` names what it makes) is checked here: the
/// index against the address, the reader program against the index (and the reader checks every piece).
///
/// If the image cannot be had, the job does not run in the container's small image instead (it would fail there for
/// want of tools, without saying why): what was set up is taken down, and the job fails at once saying so (`SUPERCI_FAIL`,
/// as for a job nothing can run).
const FULL_IMAGE: &str = r#"full_image() {
  local img=/mnt/image root=/mnt/image/merged pin="${SUPERCI_IMAGE##*/}" why="" want dev
  case "$pin" in *[!0-9a-f]*) pin="" ;; esac
  [ ${#pin} -ge 16 ] || pin=""
  up() {
    sudo mkdir -p $img/file $img/lower $img/cache $img/up $img/work $root $img/docker || { why="no room for it on this machine"; return 1; }
    curl -fsSL --retry 2 -m 15 -o /tmp/image-index.json "$SUPERCI_IMAGE/index.json" || { why="its index could not be fetched"; return 1; }
    if [ -n "$pin" ]; then case "$(sha256sum /tmp/image-index.json | cut -d' ' -f1)" in "$pin"*) ;; *) why="its index is not the one the address names"; return 1 ;; esac; fi
    curl -fsSL --retry 2 -m 30 -o /tmp/image-reader "$SUPERCI_IMAGE/reader-x86_64" && chmod +x /tmp/image-reader || { why="its reader program could not be fetched"; return 1; }
    want=$(tr -d ' \n' < /tmp/image-index.json | grep -o '"x86_64":"[0-9a-f]\{64\}"' | cut -d'"' -f4)
    if [ -n "$pin" ] || [ -n "$want" ]; then [ "$(sha256sum /tmp/image-reader | cut -d' ' -f1)" = "$want" ] || { why="its reader program is not the one its index names"; return 1; }; fi
    sudo sh -c "nohup /tmp/image-reader '$SUPERCI_IMAGE' $img/file $img/cache > /tmp/image-reader.log 2>&1 & echo \$! > /tmp/image-reader.pid"
    for i in $(seq 1 600); do ls $img/file/* >/dev/null 2>&1 && break; sudo kill -0 "$(cat /tmp/image-reader.pid 2>/dev/null)" 2>/dev/null || break; sleep 0.05; done
    ls $img/file/* >/dev/null 2>&1 || { why="it did not come up ($(tail -1 /tmp/image-reader.log 2>/dev/null | tr -d '\r\n%' | cut -c1-160))"; return 1; }
    sudo mount -t squashfs -o loop,ro $img/file/* $img/lower || { why="it could not be mounted"; return 1; }
    dev=$(findmnt -n -o SOURCE $img/lower 2>/dev/null); [ -n "$dev" ] && echo 4096 | sudo tee /sys/block/${dev##*/}/queue/read_ahead_kb >/dev/null 2>&1
    sudo mount -t overlay overlay -o lowerdir=$img/lower,upperdir=$img/up,workdir=$img/work $root || { why="its writable layer could not be made"; return 1; }
    sudo mount -t proc proc $root/proc && sudo mount --rbind /sys $root/sys && sudo mount --rbind /dev $root/dev || { why="it could not be set up"; return 1; }
    sudo mkdir -p $root/var/lib/docker $root/opt/actions-runner
    sudo mount --bind $img/docker $root/var/lib/docker && sudo mount --bind ${SUPERCI_RUNNER_DIR:-/home/runner} $root/opt/actions-runner || { why="it could not be set up"; return 1; }
    sudo rm -f $root/etc/resolv.conf; sudo cp /etc/resolv.conf $root/etc/resolv.conf
    echo "127.0.0.1 $(hostname)" | sudo tee -a $root/etc/hosts >/dev/null
    printf '%s' "$SUPERCI_DOCKER" | sudo tee $root/usr/local/bin/docker >/dev/null && sudo chmod 755 $root/usr/local/bin/docker
    # The runner's files here are this machine's runner's: the image's must be the same one.
    [ "$(sudo chroot $root /usr/bin/id -u runner 2>/dev/null)" = "$(id -u)" ] || { why="its user runner is not this machine's (id $(id -u))"; return 1; }
  }
  if up; then
    exec sudo chroot $root /usr/bin/sudo -u runner -H bash -c 'while IFS= read -r l; do k=${l%%=*}; v=${l#*=}; v=${v#\"}; v=${v%\"}; case "$k" in ""|[0-9]*|*[!A-Za-z0-9_]*) ;; *) [ "$k" != "$l" ] && export "$k=$v" ;; esac; done < /etc/environment; cd /opt/actions-runner && exec ./run.sh --jitconfig "$0"' "$SUPERCI_JIT"
  fi
  sudo umount -R -l $root >/dev/null 2>&1; sudo umount -l $img/lower >/dev/null 2>&1
  sudo kill "$(cat /tmp/image-reader.pid 2>/dev/null)" >/dev/null 2>&1; sudo umount -l $img/file >/dev/null 2>&1
  echo "superci: GitHub's full image could not be loaded from $SUPERCI_IMAGE: $why" >&2
  SUPERCI_FAIL=$(printf '::error title=SuperCI could not run this job::The full image could not be loaded from %s: %s. Try the job again; if it keeps happening, check the address in the SuperCI dashboard (Runners, Cloudflare).' "$SUPERCI_IMAGE" "$why" | base64 | tr -d '\n')
}
[ -n "$SUPERCI_IMAGE" ] && [ -z "$SUPERCI_FAIL" ] && full_image
"#;

/// What starts GitHub's runner in a Cloudflare container: inside GitHub's full image when one is set (`SUPERCI_IMAGE`; see
/// `FULL_IMAGE`; a job whose image cannot be had fails saying so), else as the container's own image has it: the wrapper first (from SUPERCI_DOCKER), then the runner with
/// its just-in-time configuration (from SUPERCI_JIT, kept off the command line).
pub fn start() -> String { format!("{FULL_IMAGE}{PLAIN}") }

const PLAIN: &str = r#"if [ -n "$SUPERCI_FAIL" ]; then printf '#!/bin/bash\necho %s | base64 -d\nexit 1\n' "$SUPERCI_FAIL" > /tmp/superci-fail.sh && chmod 755 /tmp/superci-fail.sh && export ACTIONS_RUNNER_HOOK_JOB_STARTED=/tmp/superci-fail.sh; fi
printf '%s' "$SUPERCI_DOCKER" | sudo tee /usr/local/bin/docker >/dev/null && sudo chmod 755 /usr/local/bin/docker
unset SUPERCI_DOCKER
exec /home/runner/run.sh --jitconfig "$SUPERCI_JIT""#;
