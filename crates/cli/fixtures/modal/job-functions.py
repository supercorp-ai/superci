# Jobs as Modal Functions (`superci runners update modal --mode=function`): how modal_launcher.py and modal_plane.py
# start and stop one, and what modal_jobs.py's Function does, run once and started again on the same input (Modal's
# restart after a preemption). Run by `modal::tests` with the file's source on stdin and its kind ("launcher", "plane"
# or "jobs") as the argument, against stand-ins for Modal and FastAPI: nothing reaches Modal or the network.
import asyncio, hashlib, sys, types
from unittest import mock

modal = mock.MagicMock()
class NotFoundError(Exception): pass
class RemoteError(Exception): pass
class InvalidError(Exception): pass
modal.exception.NotFoundError, modal.exception.RemoteError, modal.exception.InvalidError = NotFoundError, RemoteError, InvalidError
class HTTPException(Exception):
    def __init__(self, status_code, detail): self.status_code, self.detail = status_code, detail
routes = {}
class FastAPI:
    def _route(self, path, **_):
        def keep(f): routes[path] = f; return f
        return keep
    get = post = api_route = _route
fastapi = types.ModuleType("fastapi")
fastapi.FastAPI, fastapi.HTTPException, fastapi.Request, fastapi.Response = FastAPI, HTTPException, object, object
sys.modules["modal"], sys.modules["fastapi"] = modal, fastapi
same = lambda *_, **__: (lambda f: f)
modal.concurrent, modal.asgi_app = same, same
# (The jobs' app records each Function it is given; the others' decorators give back what they decorate.)
functions = []
def function(**options):
    def keep(f): functions.append((options, f)); return f
    return keep
modal.App.return_value.function = function
kind, source = sys.argv[1], sys.stdin.read()
ns = {"__name__": "superci_" + kind}
exec(compile(source, kind, "exec"), ns)

# One Function per image, by its address and Secret: the same name in all three.
name = ns["function_name"]
assert name() == "run" and name(None, "s") == "run"
assert name("ghcr.io/acme/ci:1", "ghcr-read") == "run_" + hashlib.sha256(b"ghcr.io/acme/ci:1\nghcr-read").hexdigest()[:12]
assert name("ghcr.io/acme/ci:1") != name("ghcr.io/acme/ci:2") != name("ghcr.io/acme/ci:2", "s")

if kind == "jobs":
    # GitHub's runner image and each image of the control plane's own, each in a container of its own, with Python.
    assert [o["name"] for o, _ in functions] == ["run", name("ghcr.io/acme/ci:1", "ghcr-read")], functions
    assert all(o["single_use_containers"] and o["timeout"] == 86400 and f is ns["run_job"] for o, f in functions), functions
    assert all(c.kwargs.get("add_python") == "3.12" for c in modal.Image.from_registry.call_args_list), modal.Image.from_registry.call_args_list
    modal.Secret.from_name.assert_called_once_with("ghcr-read")
    # Its first start runs GitHub's runner; a second start of the same input runs none and tells the control plane.
    kept = {}
    def put(key, value, skip_if_exists=False):
        if skip_if_exists and key in kept: return False
        kept[key] = value; return True
    ns["starts"] = mock.Mock(put=mock.Mock(side_effect=put))
    modal.current_function_call_id.return_value = "fc-1"
    ns["subprocess"] = mock.Mock(); ns["subprocess"].run.return_value.returncode = 0
    told = []
    ns["urllib"] = mock.Mock(); ns["urllib"].request.urlopen.side_effect = lambda r, timeout: told.append(r) or mock.Mock()
    ns["time"] = mock.Mock(time=lambda: 1_790_000_000, sleep=lambda s: None)
    notice = "https://plane.example/interrupted?runner=superci-abc-7&t=0123456789abcdef"
    assert ns["run_job"]("J", None, notice) == {"exit": 0}
    ns["subprocess"].run.assert_called_once_with(["/home/runner/run.sh", "--jitconfig", "J"], cwd="/home/runner")
    assert told == []
    assert ns["run_job"]("J", None, notice) == {"preempted": True}
    assert ns["subprocess"].run.call_count == 1, "no runner started again"
    ns["urllib"].request.Request.assert_called_once_with(notice, data=b"", method="POST")
    assert len(told) == 1
    # Another input runs its own runner (a failing one, with its hook); a control plane not answering is asked again.
    modal.current_function_call_id.return_value = "fc-2"
    assert ns["run_job"]("K", "d2h5", None) == {"exit": 0}
    assert ns["subprocess"].run.call_args.args[0][:2] == ["bash", "-c"] and "ACTIONS_RUNNER_HOOK_JOB_STARTED" in ns["subprocess"].run.call_args.args[0][2]
    ns["urllib"].request.urlopen.side_effect = [OSError("down"), OSError("down"), mock.Mock()]
    assert ns["tell"](notice) is True and ns["urllib"].request.urlopen.call_count == 4
    ns["urllib"].request.urlopen.side_effect = OSError("down")
    assert ns["tell"](notice) is False
    print("ok", kind)
    sys.exit(0)

assert ns["JOBS"] == "superci-jobs-abc", ns["JOBS"]

async def started(fails=None, **asked):
    """A job's Function started as /launch (the launcher) or sandbox_start (a Modal control plane) asks: what Modal was
    asked, and the answer, as the control plane gets it."""
    modal.reset_mock()
    call = mock.Mock(object_id="fc-1")
    modal.Function.from_name.return_value.with_options.return_value.spawn.aio = mock.AsyncMock(side_effect=fails, return_value=call)
    q = {"jit": "J", "cpu": 8, "ram_gb": 16, "max_minutes": 60, "function": True, "notice": "https://plane.example/interrupted?runner=r&t=t", **asked}
    if kind == "launcher":
        ns["verify"] = lambda *_: None
        ns["web"]()
        request = mock.Mock(headers={"authorization": "Bearer t"}, url=mock.Mock(hostname="l.example"), json=mock.AsyncMock(return_value=q))
        try:
            said = await routes["/launch"](request)
        except HTTPException as e:
            said = {"error": e.detail, "status": e.status_code}
    else:
        said = await ns["plane"].answer({"op": "sandbox_start", **q})
    return said

said = asyncio.run(started(image="ghcr.io/acme/ci:1", image_secret="ghcr-read", gpu="T4"))
assert said == ({"id": "fc-1", "kind": "function", "image": "ghcr.io/acme/ci:1"} if kind == "launcher" else {"id": "fc-1"}), said
modal.Function.from_name.assert_called_once_with("superci-jobs-abc", name("ghcr.io/acme/ci:1", "ghcr-read"))
modal.Function.from_name.return_value.with_options.assert_called_once_with(cpu=(4.0, 4.0), memory=(16384, 16384), timeout=3600, gpu="T4")
modal.Function.from_name.return_value.with_options.return_value.spawn.aio.assert_called_once_with("J", None, "https://plane.example/interrupted?runner=r&t=t")
modal.Sandbox.create.aio.assert_not_called()
# A Function not deployed: the job is told how to deploy it.
said = asyncio.run(started(NotFoundError("Function not found")))
assert said["error"].startswith("Modal refused: no Function run in the app superci-jobs-abc") and "`superci runners create modal`" in said["error"] and said.get("status", 400) == 400, said

# Stopped by its id: a Function call is cancelled with its container; a sandbox is ended as before.
async def stop(id):
    modal.reset_mock()
    modal.FunctionCall.from_id.return_value.cancel.aio = mock.AsyncMock()
    modal.Sandbox.from_id.aio = mock.AsyncMock()
    if kind == "launcher":
        request = mock.Mock(headers={"authorization": "Bearer t"}, url=mock.Mock(hostname="l.example"), json=mock.AsyncMock(return_value={"id": id}))
        return await routes["/stop"](request)
    return await ns["plane"].answer({"op": "sandbox_stop", "id": id})
asyncio.run(stop("fc-1"))
modal.FunctionCall.from_id.assert_called_once_with("fc-1")
modal.FunctionCall.from_id.return_value.cancel.aio.assert_called_once_with(terminate_containers=True)
modal.Sandbox.from_id.aio.assert_not_called()
asyncio.run(stop("sb-1"))
modal.Sandbox.from_id.aio.assert_called_once_with("sb-1")
modal.FunctionCall.from_id.assert_not_called()
print("ok", kind)
