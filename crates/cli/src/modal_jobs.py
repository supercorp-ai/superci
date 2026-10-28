# A SuperCI control plane's jobs as Modal Functions, in one Modal workspace (`superci runners update modal
# --mode=function`): GitHub's runner for one job in a container of its own, started by the control plane (or its
# launcher) with `spawn`, sized per job. A Function is preemptible: a third of a sandbox's price, and Modal may stop it
# and start the same input again. The runner's just-in-time registration is used up by its first start, so a second
# start of the same input runs no runner: it tells the control plane, which runs the job again once its run has
# finished (on a sandbox, which is not preempted). One Function for GitHub's runner image and one for each image of
# the control plane's own (`superci runner_images`), named by its address (see `function_name`): an image changed and
# not deployed again is a Function not found, and its job says so. Deployed by the SuperCI dashboard with `modal deploy`.
import base64
import hashlib
import json
import subprocess
import time
import urllib.request

import modal

NAME = "__NAME__"
# The images of the control plane's own: [[reference, Modal Secret's name or None], …].
IMAGES = json.loads(base64.b64decode("__IMAGES__"))

app = modal.App(NAME)
# Each input's first start, by its call: a second start of the same one is Modal's restart after a preemption.
starts = modal.Dict.from_name(f"{NAME}-starts", create_if_missing=True)


def function_name(reference=None, secret=None):
    """The Function a job's image runs as: `run` for GitHub's runner image, else `run_` and the first 12 hex digits of
    the SHA-256 of its address and Secret (the same in the control plane's and the launcher's code)."""
    if not reference:
        return "run"
    return "run_" + hashlib.sha256(f"{reference}\n{secret or ''}".encode()).hexdigest()[:12]


def runner_command(jit, fail=None):
    """GitHub's runner for one job; a failing runner (`fail`: base64 of why) gets a job-started hook that says why and
    fails the job before any step (both are base64, so safe on the command line)."""
    if not fail:
        return ["/home/runner/run.sh", "--jitconfig", jit]
    return ["bash", "-c", f"printf '#!/bin/bash\\necho {fail} | base64 -d\\nexit 1\\n' > /tmp/superci-fail.sh && chmod 755 /tmp/superci-fail.sh"
            f" && ACTIONS_RUNNER_HOOK_JOB_STARTED=/tmp/superci-fail.sh exec /home/runner/run.sh --jitconfig {jit}"]


# GitHub's runner image, as a sandbox has it, with Python (a Function's container runs Modal's Python in it).
SETUP = ["USER root", "RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends xz-utils zstd zip unzip jq git wget build-essential pkg-config && rm -rf /var/lib/apt/lists/*"]
NO_RUNNER = "test -x /home/runner/run.sh || { echo 'This image has no GitHub runner at /home/runner/run.sh: make it FROM ghcr.io/actions/actions-runner' >&2; exit 1; }"


def job_image(reference=None, secret=None):
    if not reference:
        return modal.Image.from_registry("ghcr.io/actions/actions-runner:latest", add_python="3.12").dockerfile_commands(SETUP).env({"RUNNER_ALLOW_RUNASROOT": "1"})
    return (
        modal.Image.from_registry(reference, secret=modal.Secret.from_name(secret) if secret else None, add_python="3.12")
        .dockerfile_commands(["USER root", f"RUN {NO_RUNNER}"])
        .env({"RUNNER_ALLOW_RUNASROOT": "1"})
    )


def tell(notice):
    """Says to the control plane that this job's runner was lost (its link, made for this job alone): a few tries."""
    for wait in (0, 2, 5, 10):
        time.sleep(wait)
        try:
            urllib.request.urlopen(urllib.request.Request(notice, data=b"", method="POST"), timeout=20).read()
            return True
        except Exception as e:  # noqa: BLE001 (any failure is tried again)
            print(f"superci: telling the control plane failed: {e}")
    return False


def run_job(jit, fail=None, notice=None):
    """GitHub's runner for one job, until it ends; or, started again on the same input (Modal preempted it), the
    control plane told so and no runner."""
    call = modal.current_function_call_id() or ""
    if call and not starts.put(call, int(time.time()), skip_if_exists=True):
        print("superci: Modal started this job's Function again (preempted); its runner is not started again")
        if notice:
            tell(notice)
        return {"preempted": True}
    return {"exit": subprocess.run(runner_command(jit, fail), cwd="/home/runner").returncode}


# One container per job (none is used for a second), for up to a day (each call is given its job's own time).
for reference, secret in [[None, None]] + IMAGES:
    app.function(name=function_name(reference, secret), image=job_image(reference, secret), timeout=24 * 3600, single_use_containers=True)(run_job)
