# A SuperCI control plane in your Modal workspace: a web endpoint GitHub sends jobs to, its state in a Modal Dict,
# its settings in a second Dict (written by the SuperCI dashboard), a schedule for the sweep, and one sandbox per
# job (GitHub's runner, registered just in time for that job). The control plane itself is SuperCI's Rust program
# (/opt/superci/plane, the same as on Cloudflare and AWS): this file hands it each request and does for it what only
# Modal's client can do. Deployed by the SuperCI dashboard with Modal's own `modal deploy`.
import asyncio
import base64
import json
import os
import subprocess
import time

import modal


def runner_command(jit, fail=None):
    """GitHub's runner for one job; a failing runner (`fail`: base64 of why) gets a job-started hook that says why and
    fails the job before any step (both are base64, so safe on the command line)."""
    if not fail:
        return ["/home/runner/run.sh", "--jitconfig", jit]
    return ["bash", "-c", f"printf '#!/bin/bash\\necho {fail} | base64 -d\\nexit 1\\n' > /tmp/superci-fail.sh && chmod 755 /tmp/superci-fail.sh"
            f" && ACTIONS_RUNNER_HOOK_JOB_STARTED=/tmp/superci-fail.sh exec /home/runner/run.sh --jitconfig {jit}"]

NAME = "__NAME__"
PLANE_ID = "__PLANE_ID__"
LABEL = "__LABEL__"

app = modal.App(NAME)
image = (
    modal.Image.debian_slim(python_version="3.12")
    .pip_install("fastapi[standard]==0.118.0")
    .add_local_file("/tmp/superci-plane", "/opt/superci/plane", copy=True)
    .run_commands("chmod +x /opt/superci/plane")
)
# GitHub's runner image; the runner refuses root unless told it may (sandboxes start as root).
runner_image = modal.Image.from_registry("ghcr.io/actions/actions-runner:latest").env({"RUNNER_ALLOW_RUNASROOT": "1"})
state = modal.Dict.from_name(f"{NAME}-state", create_if_missing=True)
settings = modal.Dict.from_name(f"{NAME}-settings", create_if_missing=True)


class Plane:
    """The control plane program: one per container, one message at a time (like a Durable Object)."""

    def __init__(self):
        self.proc = None
        self.lock = asyncio.Lock()
        self.cached = ({}, 0.0)

    def process(self):
        if self.proc is None or self.proc.poll() is not None:
            env = dict(os.environ, PLANE_ID=PLANE_ID, LABEL=LABEL)
            self.proc = subprocess.Popen(["/opt/superci/plane"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, env=env)
        return self.proc

    async def secrets(self, bearer="", fresh=False):
        values, at = self.cached
        # A dashboard key not seen yet (a new session set it a moment ago), or a move's step (it needs what the
        # dashboard wrote a moment ago): read again now, at most every second.
        unknown = fresh or (len(bearer) >= 16 and not any(k.startswith("DASHBOARD_KEY_") and v == bearer for k, v in values.items()))
        if time.time() - at > (1 if unknown else 10):
            values = {k: v async for k, v in settings.items.aio()}
            self.cached = (values, time.time())
        return values

    async def answer(self, q):
        op = q["op"]
        if op == "get":
            return {"value": await state.get.aio(q["key"])}
        if op == "put":
            await state.put.aio(q["key"], q["value"])
            return {}
        if op == "put_if_absent":
            return {"created": bool(await state.put.aio(q["key"], q["value"], skip_if_exists=True))}
        if op == "delete":
            try:
                await state.pop.aio(q["key"])
            except KeyError:
                pass
            return {}
        if op == "list":
            return {"items": [[k, v] async for k, v in state.items.aio() if isinstance(k, str) and k.startswith(q["prefix"])]}
        if op == "wake":
            due = await state.get.aio("__wake_at")
            if due is None or q["at_ms"] < due:
                await state.put.aio("__wake_at", q["at_ms"])
            return {}
        if op == "sandbox_start":
            try:
                sandbox = await modal.Sandbox.create.aio(
                    *runner_command(q["jit"], q.get("fail")),
                    app=app, image=runner_image, workdir="/home/runner",
                    # CPUs as GitHub counts them (vCPUs); Modal's are physical cores of two each. Held to what it asks
                    # for, so it is billed exactly that (Modal bills the higher of reserved and used).
                    cpu=(float(q["cpu"]) / 2,) * 2, memory=(int(q["ram_gb"]) * 1024,) * 2, timeout=int(q["max_minutes"]) * 60,
                    gpu=q.get("gpu"),  # one of Modal's ("T4", "L4", …), or none
                )
            except modal.exception.InvalidError as e:
                # What was asked cannot be had (no GPUs without a payment method): said as it is, not tried again.
                return {"error": f"Modal refused: {e}"}
            return {"id": sandbox.object_id}
        if op == "sandbox_stop":
            sandbox = await modal.Sandbox.from_id.aio(q["id"])
            await sandbox.terminate.aio()
            return {}
        return {"error": f"unknown question {op}"}

    async def run(self, message):
        async with self.lock:
            auth = message.get("headers", {}).get("authorization", "")
            fresh = message.get("url", "").split("?")[0].split("/", 3)[-1].startswith("move/")
            message["secrets"] = await self.secrets(auth[7:] if auth.startswith("Bearer ") else "", fresh)
            p = self.process()

            def write(obj):
                p.stdin.write((json.dumps(obj) + "\n").encode())
                p.stdin.flush()

            await asyncio.to_thread(write, message)
            while True:
                line = await asyncio.to_thread(p.stdout.readline)
                if not line:
                    self.proc = None
                    raise RuntimeError("the control plane stopped")
                m = json.loads(line)
                if m.get("type") != "ask":
                    return m
                try:
                    a = await self.answer(m)
                except Exception as e:  # what Modal said goes back to the control plane as its error
                    a = {"error": f"{type(e).__name__}: {e}"}
                await asyncio.to_thread(write, a)


plane = Plane()


@app.function(image=image, max_containers=1, scaledown_window=300, timeout=900)
@modal.concurrent(max_inputs=50)
@modal.asgi_app(label=NAME)
def web():
    from fastapi import FastAPI, Request
    from fastapi.responses import Response

    api = FastAPI(docs_url=None, redoc_url=None, openapi_url=None)

    @api.api_route("/{path:path}", methods=["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD"])
    async def handle(request: Request, path: str):
        body = await request.body()
        query = f"?{request.url.query}" if request.url.query else ""
        url = f"https://{request.headers.get('host', '')}{request.url.path}{query}"
        m = await plane.run({"kind": "request", "method": request.method, "url": url, "headers": dict(request.headers),
                             "body": base64.b64encode(body).decode()})
        headers = {k: v for k, v in m.get("headers", {}).items() if k.lower() not in ("content-length", "transfer-encoding")}
        return Response(content=base64.b64decode(m.get("body", "")), status_code=m.get("status", 500), headers=headers)

    return api


# Its container goes as soon as it is done (2 s): with Modal's default (a minute) a sweep every minute kept one up
# around the clock, a standing cost of about $5 a month.
@app.function(image=image, schedule=modal.Period(minutes=1), timeout=600, scaledown_window=2)
async def sweep():
    """Runs the control plane's sweep when one is due (it asks for the next one itself)."""
    due = await state.get.aio("__wake_at")
    if due is None or due > time.time() * 1000:
        return
    try:
        await state.pop.aio("__wake_at")
    except KeyError:
        pass
    m = await plane.run({"kind": "alarm"})
    if m.get("error"):
        print(f"superci: {m['error']}")
