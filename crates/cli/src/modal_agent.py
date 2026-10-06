# A SuperCI runner agent in your Modal workspace: one sandbox per job (GitHub's runner, registered just in time
# for that job), started when your control plane asks. It trusts that control plane the way AWS does: a token it signed
# (ES256, its public keys pinned here when the dashboard set this agent up), for this agent's URL, from its subject.
# Deployed by the SuperCI dashboard with Modal's own `modal deploy`; the sandboxes are Modal's official SDK.
import base64
import json
import time

import modal

TRUST = json.loads(base64.b64decode("__TRUST__"))  # {"issuer", "subject", "keys": {"keys": [...]}}

def runner_command(jit, fail=None):
    """GitHub's runner for one job; a failing runner (`fail`: base64 of why) gets a job-started hook that says why and
    fails the job before any step (both are base64, so safe on the command line)."""
    if not fail:
        return ["/home/runner/run.sh", "--jitconfig", jit]
    return ["bash", "-c", f"printf '#!/bin/bash\\necho {fail} | base64 -d\\nexit 1\\n' > /tmp/superci-fail.sh && chmod 755 /tmp/superci-fail.sh"
            f" && ACTIONS_RUNNER_HOOK_JOB_STARTED=/tmp/superci-fail.sh exec /home/runner/run.sh --jitconfig {jit}"]

NAME = "__NAME__"

app = modal.App(NAME)
web_image = modal.Image.debian_slim(python_version="3.12").pip_install("fastapi[standard]==0.118.0", "cryptography==46.0.1")
# GitHub's runner image; the runner refuses root unless told it may (sandboxes start as root).
runner_image = modal.Image.from_registry("ghcr.io/actions/actions-runner:latest").env({"RUNNER_ALLOW_RUNASROOT": "1"})


def b64url(s: str) -> bytes:
    return base64.urlsafe_b64decode(s + "=" * (-len(s) % 4))


def verify(authorization: str, audience: str) -> None:
    from cryptography.exceptions import InvalidSignature
    from cryptography.hazmat.primitives import hashes
    from cryptography.hazmat.primitives.asymmetric import ec
    from cryptography.hazmat.primitives.asymmetric.utils import encode_dss_signature

    if not authorization.startswith("Bearer "):
        raise PermissionError("no token")
    header, payload, signature = authorization[7:].split(".")
    raw = b64url(signature)
    der = encode_dss_signature(int.from_bytes(raw[:32], "big"), int.from_bytes(raw[32:], "big"))
    for jwk in TRUST["keys"]["keys"]:
        key = ec.EllipticCurvePublicNumbers(int.from_bytes(b64url(jwk["x"]), "big"), int.from_bytes(b64url(jwk["y"]), "big"), ec.SECP256R1()).public_key()
        try:
            key.verify(der, f"{header}.{payload}".encode(), ec.ECDSA(hashes.SHA256()))
        except InvalidSignature:
            continue
        claims = json.loads(b64url(payload))
        if claims.get("exp", 0) < time.time():
            raise PermissionError("expired")
        if claims.get("iss") != TRUST["issuer"] or claims.get("sub") != TRUST["subject"] or claims.get("aud") != audience:
            raise PermissionError("token for another agent or control plane")
        return
    raise PermissionError("bad token")


@app.function(image=web_image)
@modal.concurrent(max_inputs=50)
@modal.asgi_app(label=NAME)
def web():
    from fastapi import FastAPI, HTTPException, Request

    api = FastAPI()

    async def check(request: Request) -> dict:
        audience = f"https://{request.url.hostname}"
        try:
            verify(request.headers.get("authorization", ""), audience)
        except PermissionError as e:
            raise HTTPException(status_code=401, detail=f"not allowed: {e}")
        return await request.json()

    @api.get("/health")
    async def health():
        return {"agent": "modal"}

    @api.post("/launch")
    async def launch(request: Request):
        body = await check(request)
        if "jit" not in body:  # GitLab's jobs run in Docker, which Modal's sandboxes do not have
            raise HTTPException(status_code=400, detail="Modal runs no Docker for GitLab jobs")
        try:
            sandbox = await modal.Sandbox.create.aio(
                *runner_command(body["jit"], body.get("fail")),
                app=app, image=runner_image, workdir="/home/runner",
                # CPUs as GitHub counts them (vCPUs); Modal's are physical cores of two each. Held to what it asks for, so
                # it is billed exactly that (Modal bills the higher of reserved and used).
                cpu=(float(body.get("cpu") or 2) / 2,) * 2, memory=(int(body.get("ram_gb") or 8) * 1024,) * 2, timeout=int(body.get("max_minutes", 70)) * 60,
                gpu=body.get("gpu"),  # one of Modal's ("T4", "L4", …), or none
            )
        except modal.exception.InvalidError as e:
            # Modal refuses what was asked (a GPU without a payment method on the workspace): said as it says it.
            raise HTTPException(status_code=400, detail=f"Modal: {e}")
        return {"id": sandbox.object_id, "kind": "sandbox"}

    @api.post("/stop")
    async def stop(request: Request):
        body = await check(request)
        sandbox = await modal.Sandbox.from_id.aio(body["id"])
        await sandbox.terminate.aio()
        return {"stopped": True}

    return api
