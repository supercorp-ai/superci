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
import warnings

# The image builder version every image here is built with (the runner's too), whatever the workspace's own is: the one
# SuperCI's Modal client was tested with (`IMAGE_BUILDER` in modal.rs). A client refuses a newer one than it knows, and
# a new workspace has Modal's newest. Read by Modal's client, so before it is imported.
os.environ["MODAL_IMAGE_BUILDER_VERSION"] = "__IMAGE_BUILDER__"

import modal


# What Modal says beside its answers (a client release it will stop taking): its client issues each as a ServerWarning.
# Heard here for the control plane's status (its `cloud:says`), so the dashboard says it before Modal stops taking it.
heard = []
_shown = warnings.showwarning


def hear(message, category, *rest, **kw):
    if issubclass(category, modal.exception.ServerWarning) and str(message) not in heard:
        heard.append(str(message))
    _shown(message, category, *rest, **kw)


warnings.showwarning = hear


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
# GitHub's runner image; the runner refuses root unless told it may (sandboxes start as root). With what GitHub's own
# runners have that it lacks and common actions use: xz and zstd (setup-zig's and others' archives, the cache's), zip,
# jq, git, and a C compiler (a Rust or Node build links with it).
runner_image = (
    modal.Image.from_registry("ghcr.io/actions/actions-runner:latest")
    .dockerfile_commands(["USER root", "RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends xz-utils zstd zip unzip jq git wget build-essential pkg-config && rm -rf /var/lib/apt/lists/*"])
    .env({"RUNNER_ALLOW_RUNASROOT": "1"})
)
state = modal.Dict.from_name(f"{NAME}-state", create_if_missing=True)
# Each kind of record (the key up to its first colon: job, live, rline, …) in a Dict of its own, so listing one kind
# never reads every record. Before, all were in `state`: until they are moved (`migrate`, at each sweep), what is not
# found in its kind's Dict is looked for there too.
kinds = {}
layout = {"kinds": False}


def kind_of(key):
    kind = key.split(":", 1)[0] if ":" in key else "_"
    safe = "".join(c if c.isalnum() or c in "-_" else "t" for c in kind)[:24] or "_"
    if safe not in kinds:
        kinds[safe] = modal.Dict.from_name(f"{NAME}-k-{safe}", create_if_missing=True)
    return kinds[safe]


async def moved():
    if not layout["kinds"]:
        layout["kinds"] = (await state.get.aio("__layout")) == "kinds"
    return layout["kinds"]
settings = modal.Dict.from_name(f"{NAME}-settings", create_if_missing=True)


# How many messages a container answers side by side: each by a control plane program of its own, as AWS's Lambda
# answers its requests (each store operation atomic in itself; the upkeep already runs in a container of its own).
SIDE_BY_SIDE = 8


class Plane:
    """The control plane programs of a container, each answering one message at a time."""

    def __init__(self):
        self.free = []
        self.slots = asyncio.Semaphore(SIDE_BY_SIDE)
        self.cached = ({}, 0.0)
        # A swap is a read then a write (a Dict has no write over what was read): one at a time, so two programs
        # answering side by side never both take what one read (a job claimed twice, a write of settings lost).
        self.swapping = asyncio.Lock()

    def process(self):
        """A program not answering anything now, or a new one."""
        while self.free:
            p = self.free.pop()
            if p.poll() is None:
                return p
        env = dict(os.environ, PLANE_ID=PLANE_ID, LABEL=LABEL)
        return subprocess.Popen(["/opt/superci/plane"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, env=env)

    async def secrets(self, bearer="", fresh=False):
        values, at = self.cached
        # A dashboard key not seen yet (a new session set it a moment ago), or a move's step (it needs what the
        # dashboard wrote a moment ago): read again now, at most every second.
        unknown = fresh or (len(bearer) >= 16 and not any(k.startswith("DEVICE_KEY_") and v == bearer for k, v in values.items()))
        if time.time() - at > (1 if unknown else 10):
            values = {k: v async for k, v in settings.items.aio()}
            self.cached = (values, time.time())
        return values

    async def get(self, key):
        value = await kind_of(key).get.aio(key)
        if value is None and not await moved():
            value = await state.get.aio(key)
        return value

    async def drop(self, key):
        """A record taken out, from its kind's Dict and (until the records are moved) the old one."""
        for d in ([kind_of(key)] + ([] if await moved() else [state])):
            try:
                await d.pop.aio(key)
            except KeyError:
                pass

    async def end(self, key, until):
        """A record's end, kept beside it as the control plane's `io::Ending` keeps one (the same records): its end
        (`~e:<key>`) and a mark that sorts by it (`~x:<until>:<key>`) for `expire`; written without one, it has none
        (an older mark then removes nothing). Written side by side: each is a call to Modal's API."""
        if until is None:
            await self.drop(f"~e:{key}")
        else:
            mark = f"~x:{int(until):013d}:{key}"
            await asyncio.gather(kind_of(f"~e:{key}").put.aio(f"~e:{key}", str(until)), kind_of(mark).put.aio(mark, ""))

    async def answer(self, q):
        op = q["op"]
        if op == "get":
            return {"value": await self.get(q["key"])}
        if op == "get_many":
            # Side by side: each is a call to Modal's API (the status a page shows asks for some 150 records).
            return {"values": list(await asyncio.gather(*(self.get(k) for k in q["keys"])))}
        if op == "put":
            await asyncio.gather(kind_of(q["key"]).put.aio(q["key"], q["value"]), self.end(q["key"], q.get("until")))
            return {}
        if op == "put_if_absent":
            if not await moved() and (await state.get.aio(q["key"])) is not None:
                return {"created": False}
            created = bool(await kind_of(q["key"]).put.aio(q["key"], q["value"], skip_if_exists=True))
            if created:
                await self.end(q["key"], None)
            return {"created": created}
        if op == "swap":
            # Read, then written, one swap at a time in this container (see `swapping`).
            async with self.swapping:
                if await self.get(q["key"]) != q.get("was"):
                    return {"swapped": False}
                await asyncio.gather(kind_of(q["key"]).put.aio(q["key"], q["value"]), self.end(q["key"], q.get("until")))
                return {"swapped": True}
        if op == "delete":
            await asyncio.gather(self.drop(q["key"]), self.drop(f"~e:{q['key']}"))
            return {}
        if op == "expire":
            # What has passed its end, by its marks: taken out while the mark is still its end (written again since,
            # it keeps its newer one); each mark that is due goes.
            due = []
            for mark, _ in (await self.answer({"op": "list", "prefix": "~x:"}))["items"]:
                until, _, key = mark[3:].partition(":")
                if until.isdigit() and key and int(until) <= q["now_ms"]:
                    due.append((mark, int(until), key))

            async def gone(mark, until, key):
                ended = await self.get(f"~e:{key}") == str(until)
                if ended:
                    await asyncio.gather(self.drop(key), self.drop(f"~e:{key}"))
                await self.drop(mark)
                return ended
            return {"gone": sum(await asyncio.gather(*(gone(*d) for d in due)))}
        if op == "list":
            items = {k: v async for k, v in kind_of(q["prefix"]).items.aio() if isinstance(k, str) and k.startswith(q["prefix"])}
            if not await moved():
                async for k, v in state.items.aio():
                    if isinstance(k, str) and k.startswith(q["prefix"]) and k not in items:
                        items[k] = v
            return {"items": sorted(([k, v] for k, v in items.items()), key=lambda kv: kv[0])}
        if op == "migrate":
            # Moves up to `budget` records from the old Dict into their kinds' (not over one written there since).
            if await moved():
                return {"done": True}
            budget, batch = q.get("budget", 500), []
            async for k in state.keys.aio():
                if isinstance(k, str) and not k.startswith("__"):
                    batch.append(k)
                    if len(batch) > budget:
                        break
            for k in batch[:budget]:
                value = await state.get.aio(k)
                if value is not None:
                    await kind_of(k).put.aio(k, value, skip_if_exists=True)
                try:
                    await state.pop.aio(k)
                except KeyError:
                    pass
            if len(batch) > budget or (batch and budget == 0):
                return {"done": False}
            await state.put.aio("__layout", "kinds")
            layout["kinds"] = True
            return {"done": True}
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
        async with self.slots:
            p = self.process()
            try:
                m = await self.exchange(p, message)
            except BaseException:
                # (Left mid-message: not handed another.)
                p.kill()
                raise
            self.free.append(p)
            return m

    async def exchange(self, p, message):
        auth = message.get("headers", {}).get("authorization", "")
        fresh = message.get("url", "").split("?")[0].split("/", 3)[-1].startswith("move/")
        message["secrets"] = await self.secrets(auth[7:] if auth.startswith("Bearer ") else "", fresh)

        def write(obj):
            p.stdin.write((json.dumps(obj) + "\n").encode())
            p.stdin.flush()

        await asyncio.to_thread(write, message)
        while True:
            line = await asyncio.to_thread(p.stdout.readline)
            if not line:
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
told = set()


async def after(url, what):
    """Has a program do what the control plane left for after an answer (see `later` in the program)."""
    m = await plane.run({"kind": "later", "url": url, "what": what})
    if m.get("error"):
        print(f"superci: {m['error']}")
    await tell()


async def tell():
    """Keeps what Modal said that the status does not have yet, as the control plane keeps a launcher's (`cloud:says`:
    once each, the newest ten)."""
    new = [s for s in heard if s not in told]
    if not new:
        return
    kept = json.loads(await kind_of("cloud:says").get.aio("cloud:says") or "[]")
    for s in new:
        if {"cloud": "modal", "said": s} not in kept:
            kept.append({"cloud": "modal", "said": s})
    await kind_of("cloud:says").put.aio("cloud:says", json.dumps(kept[-10:]))
    told.update(new)


@app.function(image=image, max_containers=1, scaledown_window=300, timeout=900)
@modal.concurrent(max_inputs=50)
@modal.asgi_app(label=NAME)
def web():
    from fastapi import FastAPI, Request
    from fastapi.responses import Response
    from starlette.background import BackgroundTask

    api = FastAPI(docs_url=None, redoc_url=None, openapi_url=None)

    @api.api_route("/{path:path}", methods=["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD"])
    async def handle(request: Request, path: str):
        body = await request.body()
        query = f"?{request.url.query}" if request.url.query else ""
        url = f"https://{request.headers.get('host', '')}{request.url.path}{query}"
        m = await plane.run({"kind": "request", "method": request.method, "url": url, "headers": dict(request.headers),
                             "body": base64.b64encode(body).decode()})
        await tell()
        headers = {k: v for k, v in m.get("headers", {}).items() if k.lower() not in ("content-length", "transfer-encoding")}
        # An answer that says to wait (a runner asking for a message when there is none) waits here, the program let
        # go: the requests behind it are answered meanwhile.
        hold = next((v for k, v in headers.items() if k.lower() == "x-superci-hold"), None)
        if hold and hold.isdigit():
            await asyncio.sleep(int(hold) / 1000)
        # What the control plane left for after its answer (a queued job's sandbox): GitHub has its answer first (it
        # waits ten seconds), then a program is given it.
        left = BackgroundTask(after, url, m["later"]) if m.get("later") else None
        return Response(content=base64.b64decode(m.get("body", "")), status_code=m.get("status", 500), headers=headers, background=left)

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
    await tell()
    if m.get("error"):
        print(f"superci: {m['error']}")
