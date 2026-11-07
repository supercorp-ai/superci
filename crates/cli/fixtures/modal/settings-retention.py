
# Settings must use point reads, including while no CI job is due. Listing a Modal Dict does not provide a lifetime
# guarantee for its entries. (Taras Kornichuk's test from #22, on the shared stand-in: harness.py.)
import asyncio

settings = from_name(f"{NAME}-settings")
records = {"GITHUB_APP": "fake-app", "ROUTING": "fake-routing", "DEVICE_KEY_test": "fake-key"}
for k, v in records.items():
    asyncio.run(settings.put.aio(k, v))
plane = ns["Plane"]()

def reads():
    return [k for op, k in settings.calls if op == "get"]

async def check():
    settings.calls.clear()
    assert await plane.secrets() == records
    assert set(records) <= set(reads()), "Reading settings must touch each entry, not only list the Dict"
    count = len(reads())
    assert await plane.secrets() == records
    assert len(reads()) == count, "The web request cache must still avoid extra reads"
    # A setting taken out while they are read stays out: settings are read, never written back.
    clock.now += 11
    await settings.pop.aio("ROUTING")
    assert "ROUTING" not in await plane.secrets()
    settings.calls.clear()
    await ns["sweep"]()
    assert set(records) - {"ROUTING"} <= set(reads()), "The scheduled sweep must keep settings alive even without pending jobs"
    assert not [k for op, k in settings.calls if op == "put"]
    # Once a day, not at every sweep.
    settings.calls.clear()
    clock.now += 60
    await ns["sweep"]()
    assert not reads(), "Settings are read once a day, not every minute"

asyncio.run(check())
print("ok settings retention")
