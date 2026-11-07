# What the Modal fixtures share: modal_plane.py loaded (its source on stdin) against stand-ins for Modal's client and
# FastAPI, and Dicts that keep Modal's documented rules. Nothing reaches Modal. A fixture is this file followed by its
# own lines (see `modal::tests::fixture`).
#
# Modal's Dict, as its documentation says (modal.com/docs/reference/modal.Dict, read 2026-11-07): "An individual Dict
# entry will expire after 7 days of inactivity (no reads or writes)". A point read or write (`get`, `put`, `pop`,
# `contains`, `update`) is activity. Whether listing (`keys`, `items`, `values`, `len`) is, the documentation does not
# say: here it is not, the stricter reading, so code that counts on a listing to keep an entry fails here first.
import asyncio, sys, types
from unittest import mock

DAY = 24 * 3600
IDLE = 7 * DAY


class Clock:
    """The time every stand-in and the program read (`time.time`), moved on by a fixture."""
    def __init__(self):
        self.now = 1_800_000_000.0

    def time(self):
        return self.now


clock = Clock()


class Aio:
    """A Modal client method: called with `.aio(...)`."""
    def __init__(self, f):
        self.aio = f


class Dict:
    """A named Modal Dict: its entries, each with when it was last read or written, gone after seven idle days."""
    def __init__(self, name):
        self.name, self.d, self.touched, self.calls = name, {}, {}, []
        self.get, self.put, self.pop, self.contains = Aio(self._get), Aio(self._put), Aio(self._pop), Aio(self._contains)
        self.update, self.keys, self.items, self.values, self.len = Aio(self._update), Aio(self._keys), Aio(self._items), Aio(self._values), Aio(self._len)

    def _expire(self):
        for k in [k for k, t in self.touched.items() if clock.now - t >= IDLE]:
            self.d.pop(k, None)
            self.touched.pop(k, None)

    def _touch(self, op, k):
        self._expire()
        self.calls.append((op, k))
        if k in self.d:
            self.touched[k] = clock.now

    async def _get(self, k, default=None):
        self._touch("get", k)
        return self.d.get(k, default)

    async def _contains(self, k):
        self._touch("contains", k)
        return k in self.d

    async def _put(self, k, v, skip_if_exists=False):
        self._expire()
        self.calls.append(("put", k))
        if skip_if_exists and k in self.d:
            return False
        self.d[k], self.touched[k] = v, clock.now
        return True

    async def _update(self, other=None, **kw):
        for k, v in {**(other or {}), **kw}.items():
            await self._put(k, v)

    async def _pop(self, k):
        self._touch("pop", k)
        if k not in self.d:
            raise KeyError(k)
        self.touched.pop(k, None)
        return self.d.pop(k)

    async def _keys(self):
        self._expire()
        self.calls.append(("keys", None))
        for k in list(self.d):
            yield k

    async def _items(self):
        self._expire()
        self.calls.append(("items", None))
        for k, v in list(self.d.items()):
            yield k, v

    async def _values(self):
        async for _, v in self._items():
            yield v

    async def _len(self):
        self._expire()
        return len(self.d)

    def alive(self):
        """What is left now (no activity)."""
        self._expire()
        return dict(self.d)


dicts = {}


def from_name(name, create_if_missing=False, **_):
    if name not in dicts:
        dicts[name] = Dict(name)
    return dicts[name]


async def listed(**_):
    return list(dicts.values())


modal = mock.MagicMock()
modal.Dict.from_name = from_name
modal.Dict.objects.list = Aio(listed)
fastapi = types.ModuleType("fastapi")
fastapi.FastAPI, fastapi.Request, fastapi.Response = mock.MagicMock, object, object
sys.modules["modal"], sys.modules["fastapi"] = modal, fastapi
same = lambda *_, **__: (lambda f: f)
modal.App.return_value.function, modal.concurrent, modal.asgi_app = same, same, same
source = sys.stdin.read()
ns = {"__name__": "superci_plane"}
exec(compile(source, "plane", "exec"), ns)
ns["time"].time = clock.time
NAME = ns["NAME"]
