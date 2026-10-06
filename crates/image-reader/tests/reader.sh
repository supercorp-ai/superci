#!/bin/bash
# The reader, for real: run by ../try.sh inside a privileged Linux container, with at /t a packed small image (out/),
# the checksums of its files (expected.txt) and the reader program (reader).
set -u
PIN=$(ls /t/out | head -1)
cd /srv && cp -r /t/out /srv/out
# A copy with one piece changed (piece 20: in the big file), and one with another index.
cp -r /srv/out/$PIN /srv/bad && mkdir -p /srv/badroot && mv /srv/bad /srv/badroot/$PIN && printf 'XXXX' | dd of=/srv/badroot/$PIN/p/000020 bs=1 seek=100 conv=notrunc 2>/dev/null
mkdir -p /srv/otherroot && cp -r /srv/out/$PIN /srv/otherroot/$PIN && sed -i 's/"piece":1048576/"piece":1048576 /' /srv/otherroot/$PIN/index.json
mkdir -p /srv/hotroot && cp -r /srv/out/$PIN /srv/hotroot/$PIN && echo '[5,6,7]' > /srv/hotroot/$PIN/hot.json
(cd /srv && python3 -m http.server 8000 --bind 127.0.0.1 >/dev/null 2>&1 &)
sleep 1
pass=0; fail=0
ok() { if [ "$2" = "$3" ]; then echo "ok   $1"; pass=$((pass+1)); else echo "FAIL $1: got '$2', expected '$3'"; fail=$((fail+1)); fi; }
start() { # root dir, cache name, extra args
  mkdir -p /mnt/$2/file /mnt/$2/lower /mnt/$2/cache
  /t/reader "http://127.0.0.1:8000/$1/$PIN" /mnt/$2/file /mnt/$2/cache "${@:3}" > /mnt/$2/log 2>&1 &
  for i in $(seq 1 100); do ls /mnt/$2/file/* >/dev/null 2>&1 && break; sleep 0.05; done
  mount -t squashfs -o loop,ro /mnt/$2/file/* /mnt/$2/lower 2>/mnt/$2/mounterr
}
sums() { (cd /mnt/$1/lower && find . -type f | sort | xargs sha256sum 2>&1); }
stat_of() { sleep 1.2; python3 -c "import json,sys; print(json.load(open('/mnt/$1/cache/stats.json')).get('$2'))"; }

# 1. As published: every file reads as it was packed; each piece checked; the hot list fetched.
start out a
ok "files read as packed" "$(sums a | sha256sum)" "$(cat /t/expected.txt | sha256sum)"
ok "each piece checked" "$(grep -c 'each checked' /mnt/a/log)" 1
ok "hot list used" "$(grep -c 'hot: 3 pieces' /mnt/a/log)" 1
ok "nothing failed" "$(stat_of a failed)" 0

# 2. A changed piece: only what lies in it fails to read; the rest reads; nothing wrong is ever handed out.
start badroot b
bad=$(sums b)
ok "a file in the changed piece is refused" "$(echo "$bad" | grep -c 'Input/output error')" 1
ok "other files still read" "$(echo "$bad" | grep -v 'error' | grep -c -F -f <(grep -v big.bin /t/expected.txt | cut -d' ' -f1))" 9
ok "the changed piece is said" "$(grep -c 'piece 20: not what the index says' /mnt/b/log | sed 's/[1-9][0-9]*/some/')" some
ok "counted as failed" "$( [ "$(stat_of b failed)" -ge 1 ] && echo yes)" yes

# 3. Another index than the address names: the reader stops.
mkdir -p /mnt/c/file /mnt/c/cache
/t/reader "http://127.0.0.1:8000/otherroot/$PIN" /mnt/c/file /mnt/c/cache > /mnt/c/log 2>&1; code=$?
ok "an index that is not the pinned one stops it" "$code $(grep -c 'not the one the address names' /mnt/c/log)" "1 1"

# 4. A hot list the index does not name: not used; the image still works.
start hotroot d
ok "another hot list is not used" "$(grep -c 'hot.json is not the one the index names' /mnt/d/log)" 1
ok "files still read" "$(sums d | sha256sum)" "$(cat /t/expected.txt | sha256sum)"

# 5. A cache that may hold little: pieces are dropped and fetched again; everything still reads right, twice.
start out e --max-cache-mb 6
ok "reads with a small cache" "$(sums e | sha256sum)" "$(cat /t/expected.txt | sha256sum)"
echo 3 > /proc/sys/vm/drop_caches
ok "reads again after the kernel forgot" "$(sums e | sha256sum)" "$(cat /t/expected.txt | sha256sum)"
ok "pieces were dropped" "$( [ "$(stat_of e dropped)" -ge 20 ] && echo yes)" yes
ok "it holds no more than it may" "$( [ "$(stat_of e kept_bytes)" -le $((6*1048576)) ] && echo yes)" yes
ok "its file takes that little disk" "$( [ "$(du -m /mnt/e/cache/pieces | cut -f1)" -le 8 ] && echo yes)" yes

# 6. A disk with no room to spare: nothing is kept beyond what a read holds, and reads still work.
start out f --keep-free-mb 99999999
ok "reads with no room to spare" "$(sums f | sha256sum)" "$(cat /t/expected.txt | sha256sum)"
ok "almost nothing kept" "$( [ "$(stat_of f kept_bytes)" -le $((4*1048576)) ] && echo yes)" yes

# 7. Not https, and not this machine: refused.
/t/reader "http://example.com/x" /mnt/c/file /mnt/c/cache > /mnt/c/log2 2>&1
ok "plain http elsewhere is refused" "$(grep -c 'must start with https://' /mnt/c/log2)" 1
echo "passed $pass, failed $fail"
[ $fail = 0 ] || { for d in a b c d e f; do echo "--- $d"; tail -5 /mnt/$d/log 2>/dev/null; cat /mnt/$d/mounterr 2>/dev/null; done; }
