#!/usr/bin/env python3
"""Says where SuperCI's Modal client has drifted from Modal's own (the Modal drift workflow runs it every week).

    scripts/modal-drift.py [--proto FILE] [--go-version FILE] [--pypi-version VERSION]
                           [--client-image FILE] [--newest-image FILE]

SuperCI calls Modal's API with messages declared by hand (crates/cli/src/modal.rs, `mod pb`), says it is Modal's Go
SDK of a release (`LIBMODAL_VERSION`), and deploys with Modal's Python client of a release (`MODAL_CLIENT`), telling it
which image builder version to build with (`IMAGE_BUILDER`). Each is held to Modal's sources: its API
(modal_proto/api.proto in modal-labs/modal-client), its Go SDK's version (go/version.go there), its Python client's
newest release (PyPI), and the image builder versions a client release knows (py/modal/_image.py, at the pinned
release and at the newest). Read from there unless given. Exits 1, saying what moved, when:

- a message, field or method SuperCI uses is not in the API as declared (its number, its type), or an enum value it
  sends means something else;
- the Go SDK or the Python client has a newer minor release than the one SuperCI says or installs (Modal stops
  taking old releases, and says so first: see `heard` in modal.rs);
- the image builder version is not one the pinned client knows, or Modal's newest client knows a newer one (new
  workspaces get it; SuperCI moves to it on purpose, with a client that knows it);
- a release of the client is written anywhere but `MODAL_CLIENT`.
"""
import json, re, sys, urllib.request

MODAL_RS = "crates/cli/src/modal.rs"
PROTO_URL = "https://raw.githubusercontent.com/modal-labs/modal-client/main/modal_proto/api.proto"
GO_VERSION_URL = "https://raw.githubusercontent.com/modal-labs/modal-client/main/go/version.go"
PYPI_URL = "https://pypi.org/pypi/modal/json"
IMAGE_URL = "https://raw.githubusercontent.com/modal-labs/modal-client/py/v{}/py/modal/_image.py"


def builders(image_py):
    """The image builder versions a client release knows (its `ImageBuilderVersion`), without PREVIEW."""
    known = re.search(r"ImageBuilderVersion = Literal\[([^\]]*)\]", image_py)
    return sorted(v for v in re.findall(r'"([^"]+)"', known.group(1)) if v != "PREVIEW") if known else []

# The enum values SuperCI sends or reads as numbers, and what each means to it.
ENUMS = [
    ("ClientType", 9, "CLIENT_TYPE_LIBMODAL_GO"),            # x-modal-client-type
    ("ObjectCreationType", 1, "OBJECT_CREATION_TYPE_CREATE_IF_MISSING"),  # AppGetOrCreate, DictGetOrCreate
    ("AppState", 3, "APP_STATE_DEPLOYED"),                   # a control plane found by AppList
    ("FileDescriptor", 1, "FILE_DESCRIPTOR_STDOUT"),         # SandboxGetLogs
    ("FileDescriptor", 2, "FILE_DESCRIPTOR_STDERR"),
    ("GenericResult.GenericStatus", 1, "GENERIC_STATUS_SUCCESS"),
]
# Messages SuperCI declares that are not Modal's own, or are known by another name there.
NOT_MODALS = {"Empty", "Timestamp"}

# A prost type and the proto types it reads (an enum is an int32 on the wire).
SCALARS = {"string": {"string"}, "bytes": {"bytes"}, "bool": {"bool"}, "float": {"float"}, "double": {"double"},
           "int32": {"int32"}, "uint32": {"uint32"}, "int64": {"int64"}, "uint64": {"uint64"}}


def fetch(url):
    with urllib.request.urlopen(url, timeout=60) as r:
        return r.read().decode()


def ours(source):
    """SuperCI's declared messages: name -> {number: (prost type, repeated, field name)}; and the methods it calls."""
    pb = source[source.index("mod pb {"):source.index("\n}\n", source.index("mod pb {"))]
    messages = {}
    for m in re.finditer(r"pub struct (\w+) \{(.*?)\}\s*(?=#\[derive|$)", pb, re.S):
        fields = {}
        for f in re.finditer(r'#\[prost\(([^)]*?)tag = "(\d+)"\)\]\s*pub (\w+):', m.group(2)):
            spec = f.group(1)
            kind = "map" if spec.startswith("map") else re.match(r"\s*(\w+)", spec).group(1)
            fields[int(f.group(2))] = (kind, "repeated" in spec, f.group(3))
        messages[m.group(1)] = fields
    calls = set(re.findall(r'(?:unary|stream)(?:::<[^>]*>)?\("([A-Z]\w+)"', source))
    streams = set(re.findall(r'stream(?:::<[^>]*>)?\("([A-Z]\w+)"', source))
    return messages, calls, streams


def theirs(proto):
    """Modal's API: message (and nested message) -> {number: (type, label, name)}; enums; the service's methods."""
    messages, enums, rpcs, stack = {}, {}, {}, []
    # (Read by statements, not lines: `message Empty {}` opens and closes on one.)
    text = re.sub(r"/\*.*?\*/", "", "\n".join(l.split("//")[0] for l in proto.splitlines()), flags=re.S)
    tokens = [" ".join(t.split()) for t in re.split(r"([{};])", text)]
    tokens = [t for t in tokens if t]
    for i, line in enumerate(tokens):
        if line in ("{", ";"):
            continue
        if line == "}":
            stack.pop()
            continue
        if i + 1 < len(tokens) and tokens[i + 1] == "{":
            opened = re.match(r"(message|enum|oneof|service)\s+(\w+)", line)
            # (Any other block, an rpc's or a field's options, is no scope of names.)
            stack.append((opened.group(1), opened.group(2)) if opened else ("options", ""))
        names = [n for k, n in stack if k in ("message", "enum")]
        kind = next((k for k, _ in reversed(stack) if k != "oneof"), None)
        if line.startswith("rpc "):
            r = re.match(r"rpc\s+(\w+)\s*\(\s*(stream\s+)?[\w.]+\s*\)\s*returns\s*\(\s*(stream\s+)?[\w.]+\s*\)", line)
            if r:
                rpcs[r.group(1)] = bool(r.group(3))
        elif kind == "message":
            f = re.match(r"(repeated |optional )?(map\s*<[^>]+>|[\w.]+)\s+(\w+)\s*=\s*(\d+)", line)
            if f:
                messages.setdefault(".".join(names), {})[int(f.group(4))] = (f.group(2), (f.group(1) or "").strip(), f.group(3))
        elif kind == "enum":
            v = re.match(r"(\w+)\s*=\s*(\d+)", line)
            if v:
                enums.setdefault(".".join(names), {})[int(v.group(2))] = v.group(1)
    for name, fields in list(messages.items()):
        # (A nested message is found by its own name too: SuperCI declares AppListItem, Modal nests it.)
        messages.setdefault(name.split(".")[-1], fields)
    return messages, enums, rpcs


def compatible(prost, proto_type, enums):
    if prost == "map":
        return proto_type.startswith("map<")
    if prost == "message":
        return proto_type not in SCALARS and proto_type.split(".")[-1] not in enums and not proto_type.startswith("map<")
    if prost == "int32" and (proto_type in enums or proto_type.split(".")[-1] in {e.split(".")[-1] for e in enums}):
        return True
    return proto_type in SCALARS.get(prost, set())


def minor(version):
    m = re.match(r"v?(\d+)\.(\d+)", version)
    return (int(m.group(1)), int(m.group(2))) if m else None


def main():
    args = sys.argv[1:]
    given = lambda flag: args[args.index(flag) + 1] if flag in args else None
    source = open(MODAL_RS).read()
    proto = open(given("--proto")).read() if given("--proto") else fetch(PROTO_URL)
    go = open(given("--go-version")).read() if given("--go-version") else fetch(GO_VERSION_URL)
    pypi = given("--pypi-version") or json.loads(fetch(PYPI_URL))["info"]["version"]

    declared, calls, streams = ours(source)
    api, enums, rpcs = theirs(proto)
    drift = []
    for name, fields in declared.items():
        if name in NOT_MODALS:
            continue
        if name not in api:
            drift.append(f"message {name}: not in Modal's API")
            continue
        for number, (kind, repeated, field) in fields.items():
            theirs_field = api[name].get(number)
            if theirs_field is None:
                drift.append(f"{name}.{field} = {number}: Modal's {name} has no field {number}")
            elif not compatible(kind, theirs_field[0], enums) or repeated != (theirs_field[1] == "repeated"):
                drift.append(f"{name}.{field} = {number}: declared {'repeated ' if repeated else ''}{kind}, Modal's is {theirs_field[1] + ' ' if theirs_field[1] else ''}{theirs_field[0]} {theirs_field[2]}")
    for call in sorted(calls):
        if call not in rpcs:
            drift.append(f"method {call}: not in Modal's API")
        elif rpcs[call] != (call in streams):
            drift.append(f"method {call}: called as {'a stream' if call in streams else 'unary'}, Modal's {'streams' if rpcs[call] else 'is unary'}")
    for enum, value, meaning in ENUMS:
        if enums.get(enum, {}).get(value) != meaning:
            drift.append(f"{enum} {value}: SuperCI takes it as {meaning}, Modal's API says {enums.get(enum, {}).get(value)}")

    # (What the header is given: a string, `concat!` of strings and the crate's version, or a constant of these.)
    given_as = re.search(r'"x-modal-libmodal-version",\s*([A-Z_]+|"[^"]*"|concat!\(.*?\)\))', source).group(1).strip()
    constant = re.search(rf"const {re.escape(given_as)}: &str = ([^;]+);", source)
    said = "".join(re.findall(r'"([^"]*)"|env!\("CARGO_PKG_VERSION"\)', (constant.group(1) if constant else given_as).replace('env!("CARGO_PKG_VERSION")', '"<version>"')))
    newest_go = re.search(r'const sdkVersion = "([^"]+)"', go).group(1)
    # (As the Go SDK says it, or Modal creates no app for it: modal-go/ and a version.)
    version = said.removeprefix("modal-go/") if said.startswith("modal-go/") else ""
    if minor(version) is None:
        drift.append(f"Go SDK: SuperCI says {said}, not modal-go/<version> as the Go SDK does (the x-modal-libmodal-version header, {MODAL_RS})")
    elif minor(version) < minor(newest_go):
        drift.append(f"Go SDK: SuperCI says {said}, Modal's newest is {newest_go} (LIBMODAL_VERSION in {MODAL_RS})")
    client = re.search(r'const MODAL_CLIENT: &str = "([^"]+)"', source).group(1)
    builder = re.search(r'const IMAGE_BUILDER: &str = "([^"]+)"', source).group(1)
    if minor(client) < minor(pypi):
        drift.append(f"Python client: SuperCI installs modal=={client}, PyPI's newest is {pypi} (MODAL_CLIENT in {MODAL_RS})")
    written = sorted(set(re.findall(r"modal==(\d[\w.]*)", source)))
    if written:
        drift.append(f"Python client: {MODAL_RS} writes modal=={', '.join(written)} where MODAL_CLIENT is meant")
    knows = builders(open(given("--client-image")).read() if given("--client-image") else fetch(IMAGE_URL.format(client)))
    newest = builders(open(given("--newest-image")).read() if given("--newest-image") else fetch(IMAGE_URL.format(pypi)))
    if builder not in knows:
        drift.append(f"Image builder: SuperCI builds with {builder}, which modal=={client} does not know (it knows {', '.join(knows) or 'none'})")
    if newest and newest[-1] > builder:
        drift.append(f"Image builder: Modal's newest client knows {newest[-1]}, SuperCI builds with {builder}: new workspaces have it (IMAGE_BUILDER and MODAL_CLIENT in {MODAL_RS}, moved together)")

    print(f"Messages {len(declared) - len(NOT_MODALS & declared.keys())}, methods {len(calls)}, enum values {len(ENUMS)}; "
          f"{said} (newest {newest_go}); modal=={client} (newest {pypi}), image builder {builder} (newest {newest[-1] if newest else 'unknown'}).")
    if drift:
        print("Drifted from Modal's own:\n" + "\n".join(f"- {d}" for d in drift))
        sys.exit(1)
    print("As Modal's own.")


if __name__ == "__main__":
    main()
