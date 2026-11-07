# Settings must use point reads, including while no CI job is due. Listing a
# Modal Dict does not provide a lifetime guarantee for its entries.
import asyncio, sys, types
from unittest import mock

modal = mock.MagicMock()
fastapi = types.ModuleType('fastapi')
fastapi.FastAPI, fastapi.Request, fastapi.Response = mock.MagicMock, object, object
sys.modules['modal'], sys.modules['fastapi'] = modal, fastapi
same = lambda *_, **__: (lambda f: f)
modal.App.return_value.function, modal.concurrent, modal.asgi_app = same, same, same
ns = {'__name__': 'superci_plane'}
exec(compile(sys.stdin.read(), 'plane', 'exec'), ns)
records = {'GITHUB_APP': 'fake-app', 'ROUTING': 'fake-routing', 'DEVICE_KEY_test': 'fake-key'}
reads = []

async def keys():
    for key in [*records, 'removed-during-read']:
        yield key

async def items():
    for item in records.items():
        yield item

async def get(key):
    reads.append(key)
    return records.get(key)

settings = mock.MagicMock()
settings.keys.aio, settings.items.aio, settings.get.aio = keys, items, get
ns['settings'] = settings
plane = ns['Plane']()

async def check():
    assert await plane.secrets() == records
    assert set(records) <= set(reads), 'Reading settings must touch each entry, not only list the Dict'
    count = len(reads)
    assert await plane.secrets() == records
    assert len(reads) == count, 'The web request cache must still avoid extra reads'
    ns['state'].get.aio = mock.AsyncMock(return_value=None)
    reads.clear()
    await ns['sweep']()
    assert set(records) <= set(reads), 'The scheduled sweep must keep settings alive even without pending jobs'
    settings.put.aio.assert_not_called()

asyncio.run(check())
print('ok settings retention')
