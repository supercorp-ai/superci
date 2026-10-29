# How modal_plane.py reads the control plane's records: from each kind's Dict, else (until they are moved) from the
# old one; several at once side by side, each being a call to Modal's API. Run by `modal::tests` with the file's
# source on stdin, against stand-ins for Modal and FastAPI: nothing reaches Modal.
import asyncio, sys, types
from unittest import mock

modal = mock.MagicMock()
fastapi = types.ModuleType("fastapi")
fastapi.FastAPI, fastapi.Request, fastapi.Response = mock.MagicMock, object, object
sys.modules["modal"], sys.modules["fastapi"] = modal, fastapi
same = lambda *_, **__: (lambda f: f)
modal.App.return_value.function, modal.concurrent, modal.asgi_app = same, same, same
ns = {"__name__": "superci_plane"}
exec(compile(sys.stdin.read(), "plane", "exec"), ns)

# Each kind's Dict and the old one, as Modal answers them: a while for each call, the calls in flight counted.
kept = {"job:1": "one", "job:3": "three", "day:9": "nine"}
old = {"job:2": "two", "__layout": None}
flight = {"now": 0, "most": 0}

def dict_of(records):
    async def get(key):
        flight["now"] += 1
        flight["most"] = max(flight["most"], flight["now"])
        await asyncio.sleep(0.05)
        flight["now"] -= 1
        return records.get(key)
    d = mock.MagicMock()
    d.get.aio = get
    return d

ns["kinds"].clear()
modal.Dict.from_name = lambda *_, **__: dict_of(kept)
ns["state"] = dict_of(old)
plane = ns["plane"]

async def asked(q):
    began = asyncio.get_running_loop().time()
    said = await plane.answer(q)
    return said, asyncio.get_running_loop().time() - began

# One record, from its kind's Dict, else from the old one; none where neither has it.
assert asyncio.run(asked({"op": "get", "key": "job:1"}))[0] == {"value": "one"}
assert asyncio.run(asked({"op": "get", "key": "job:2"}))[0] == {"value": "two"}
assert asyncio.run(asked({"op": "get", "key": "job:9"}))[0] == {"value": None}

# Several, in the order asked, read side by side: forty take about as long as one.
keys = ["job:3", "job:2", "job:9", "day:9"] + [f"job:{i}" for i in range(100, 136)]
said, took = asyncio.run(asked({"op": "get_many", "keys": keys}))
assert said["values"][:4] == ["three", "two", None, "nine"] and said["values"][4:] == [None] * 36, said
assert flight["most"] >= len(keys) and took < 1.0, (flight, took)
assert asyncio.run(asked({"op": "get_many", "keys": []}))[0] == {"values": []}

# Once the records are moved, the old Dict is not asked.
ns["layout"]["kinds"] = True
ns["state"] = mock.MagicMock()
assert asyncio.run(asked({"op": "get_many", "keys": ["job:2", "job:3"]}))[0] == {"values": [None, "three"]}
ns["state"].get.aio.assert_not_called()
print("ok plane store")
