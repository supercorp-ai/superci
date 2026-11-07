
# A Modal control plane left alone for eight days (no job, no request, nobody at the dashboard): only Modal's schedule
# runs its sweep, every minute, with nothing due. Everything it keeps must still be there after, though Modal drops a
# Dict entry after seven days with no point read or write. Without a keep-alive this lost the settings Dict (sign-ins,
# device keys), the settings record (the order, accounts, runners), the signing identity, a moved control plane's
# standby mark and every record with months left before its end.
import asyncio

# The stand-in keeps Modal's rule: an untouched entry goes after seven days; a touched one stays.
probe = from_name("probe")
asyncio.run(probe.put.aio("untouched", "x"))
asyncio.run(probe.put.aio("touched", "y"))
clock.now += 6 * DAY
asyncio.run(probe.get.aio("touched"))
clock.now += DAY + 1
assert probe.alive() == {"touched": "y"}, probe.alive()
del dicts["probe"]

plane = ns["plane"]
ns["layout"]["kinds"] = True
settings = from_name(f"{NAME}-settings")
for k, v in {"GITHUB_APP": "app", "GITHUB_SECRET": "hook", "DEVICE_KEY_laptop": "k" * 32, "ROUTING": "{}"}.items():
    asyncio.run(settings.put.aio(k, v))
far = (clock.now + 80 * DAY) * 1000
kept = {
    "settings": '{"version":3}',                 # the order, launchers, accounts and runners
    "identity": '{"signing":"key"}',             # the OpenID key AWS and launchers trust
    "standby": "1",                              # a control plane moved away: not in use
    "day:2026-11-07": '{"jobs":4}',              # a day's totals, kept for months
    "spend:2026-11:aws": "12.5",                 # this month's spend, which limits count
    "cloud:says": "[]",
}
for k, v in kept.items():
    asyncio.run(plane.answer({"op": "put", "key": k, "value": v, "until": far if ":" in k else None}))

minutes = 8 * 24 * 60
for _ in range(minutes):
    clock.now += 60
    asyncio.run(ns["sweep"]())

def alive():
    out = {}
    for d in dicts.values():
        out.update({(d.name, k): v for k, v in d.alive().items()})
    return out

left = alive()
lost = [k for k, v in kept.items() if ns["kind_of"](k).alive().get(k) != v]
gone = [k for k in ("GITHUB_APP", "GITHUB_SECRET", "DEVICE_KEY_laptop", "ROUTING") if (settings.name, k) not in left]
assert not lost and not gone, f"lost after eight idle days: records {lost}, the settings Dict's {gone}"
for k, v in kept.items():
    assert asyncio.run(plane.answer({"op": "get", "key": k})) == {"value": v}, k
assert asyncio.run(plane.secrets(fresh=True))["DEVICE_KEY_laptop"] == "k" * 32

# What it cost: each entry read once a day, not once a minute; beside that, the sweep's two small reads a minute (whether
# a sweep is due, and when the entries were last read).
reads = sum(1 for d in dicts.values() for op, _ in d.calls if op == "get")
entries = len(left)
assert reads <= entries * 9 + 2 * minutes + 50, f"{reads} reads for {entries} entries in eight days"
assert sum(1 for d in dicts.values() for op, _ in d.calls if op == "put") <= len(kept) * 3 + 10, "kept alive by reading, never by writing back"
print("ok idle week")
