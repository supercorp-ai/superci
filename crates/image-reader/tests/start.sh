#!/bin/bash
# What starts a runner inside a published image (core docker.rs), for real: run by ../try.sh --start inside a
# privileged Linux container that has a user runner, with at /t a packed small root image (rootout/) and the script
# (start.sh). The image says ImageOS=inside-image in its /etc/environment; this machine is set to say outside.
PIN=$(ls /t/rootout | head -1)
mkdir -p /srv/out && cp -r /t/rootout/$PIN /srv/out/$PIN && cp -r /t/rootout/$PIN /srv/out/0123456789abcdef0123
mkdir -p /srv/out/unpinned && cp -r /t/rootout/$PIN/. /srv/out/unpinned/ && printf 'x' >> /srv/out/unpinned/reader-x86_64
(cd /srv && python3 -m http.server 8000 --bind 127.0.0.1 >/dev/null 2>&1 &)
sleep 1
echo 'ImageOS=outside' > /etc/environment
cat > /home/runner/run.sh <<'RUN'
#!/bin/bash
echo "RAN as $(id -un) ImageOS=$ImageOS FOO=[$FOO] bad=[${BAD:-}] cwd=$PWD args=$* docker=$(cat /usr/local/bin/docker 2>/dev/null)"
if [ -n "$ACTIONS_RUNNER_HOOK_JOB_STARTED" ]; then bash "$ACTIONS_RUNNER_HOOK_JOB_STARTED"; echo "hook exit $?"; fi
RUN
chmod +x /home/runner/run.sh; chown -R runner /home/runner
run() { sudo -u runner env SUPERCI_IMAGE="$1" SUPERCI_JIT=JIT SUPERCI_DOCKER=wrapper bash /t/start.sh 2>&1; }
echo "== 1. a pinned address"; run http://127.0.0.1:8000/out/$PIN
echo "   mounted: $(findmnt -rn | grep -c /mnt/image/)  read-ahead: $(cat /sys/block/loop*/queue/read_ahead_kb 2>/dev/null | sort -n | tail -1)"
umount -R -l /mnt/image/merged 2>/dev/null; umount -l /mnt/image/lower 2>/dev/null; pkill image-reader; umount -l /mnt/image/file 2>/dev/null; sleep 0.5; rm -rf /mnt/image/*
echo "== 2. an address naming another index"; run http://127.0.0.1:8000/out/0123456789abcdef0123
echo "   left mounted: $(findmnt -rn | grep -c /mnt/image/)  reader running: $(pgrep -c image-reader)"
echo "== 3. a reader that is not the index's"; run http://127.0.0.1:8000/out/unpinned
echo "   left mounted: $(findmnt -rn | grep -c /mnt/image/)  reader running: $(pgrep -c image-reader)"
echo "== 4. nothing at the address"; run http://127.0.0.1:8000/out/nothing-here | cut -c1-400
echo "== 5. no image set"; sudo -u runner env SUPERCI_JIT=JIT SUPERCI_DOCKER=wrapper bash /t/start.sh 2>&1
