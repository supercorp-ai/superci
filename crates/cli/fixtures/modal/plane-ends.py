# How modal_plane.py keeps a record's end beside it, as the control plane's io::Ending does (the same records: its end
# `~e:<key>`, a mark `~x:<until>:<key>`), each write with its end one question, the Dict's calls made side by side; and
# takes out what has passed its end. Run by `modal::tests` with the file's source on stdin, against stand-ins for
# Modal's Dicts (each call taking a while) and FastAPI: nothing reaches Modal.
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


class Dict:
    """A Modal Dict as its client answers: each call a while."""
    def __init__(self):
        self.d = {}
        self.get, self.put, self.pop = mock.MagicMock(), mock.MagicMock(), mock.MagicMock()
        self.get.aio, self.put.aio, self.pop.aio, self.items.aio = self._get, self._put, self._pop, self._items

    async def _get(self, k):
        await asyncio.sleep(0.05)
        return self.d.get(k)

    async def _put(self, k, v, skip_if_exists=False):
        await asyncio.sleep(0.05)
        if skip_if_exists and k in self.d:
            return False
        self.d[k] = v
        return True

    async def _pop(self, k):
        await asyncio.sleep(0.05)
        return self.d.pop(k)

    async def _items(self):
        for kv in list(self.d.items()):
            yield kv

    items = mock.MagicMock()


dicts = {}
modal.Dict.from_name = lambda name, **_: dicts.setdefault(name, Dict())
ns["kinds"].clear()
ns["layout"]["kinds"] = True
plane = ns["plane"]
every = lambda: {k: v for d in dicts.values() for k, v in d.d.items()}


def asked(q):
    async def timed():
        began = asyncio.get_running_loop().time()
        said = await plane.answer(q)
        return said, asyncio.get_running_loop().time() - began
    return asyncio.run(timed())

# A record with an end: the record, its end and its mark, written side by side.
said, took = asked({"op": "put", "key": "job:1", "value": "one", "until": 5000})
assert said == {} and took < 0.09, took
assert every() == {"job:1": "one", "~e:job:1": "5000", "~x:0000000005000:job:1": ""}, every()
# Written again without one: none (its mark is left, and removes nothing).
asked({"op": "put", "key": "job:1", "value": "uno"})
assert "~e:job:1" not in every() and every()["job:1"] == "uno", every()
# A swap over what was read: refused when it is not what is there; written with its end when it is.
assert asked({"op": "swap", "key": "job:1", "was": "one", "value": "x"})[0] == {"swapped": False}
assert asked({"op": "swap", "key": "job:1", "was": "uno", "value": "dos", "until": 7000})[0] == {"swapped": True}
assert every()["job:1"] == "dos" and every()["~e:job:1"] == "7000", every()
assert asked({"op": "swap", "key": "job:2", "was": None, "value": "new"})[0] == {"swapped": True}
# Made only when absent: then with no end left from before.
asked({"op": "put", "key": "~e:job:3", "value": "1"})
assert asked({"op": "put_if_absent", "key": "job:3", "value": "three"})[0] == {"created": True}
assert "~e:job:3" not in every() and asked({"op": "put_if_absent", "key": "job:3", "value": "again"})[0] == {"created": False}
# Taken out with its end.
asked({"op": "put", "key": "job:4", "value": "four", "until": 6000})
asked({"op": "delete", "key": "job:4"})
assert "job:4" not in every() and "~e:job:4" not in every(), every()
# What has passed its end (by now 7000): job:1 (its end is 7000), not job:2 (none); a mark of an older end (5000)
# goes alone, one not due stays, one that is not a mark is left as it is.
asked({"op": "put", "key": "job:5", "value": "five", "until": 9000})
asked({"op": "put", "key": "~x:soon:job:2", "value": ""})
said, took = asked({"op": "expire", "now_ms": 7000})
assert said == {"gone": 1}, said
assert "job:1" not in every() and "~e:job:1" not in every() and every()["job:2"] == "new" and every()["job:5"] == "five", every()
assert not any(k.startswith("~x:0000000005000") or k.startswith("~x:0000000007000") for k in every()) and "~x:0000000009000:job:5" in every() and "~x:soon:job:2" in every(), every()
print("ok plane ends")
