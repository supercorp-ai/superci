// A SuperCI Worker (the control plane's, or the Cloudflare launcher's) as Cloudflare runs it, for its tests under
// `wrangler dev` (see `worker.rs`): the module worker-build made, with what a local run has not put in around it
// (nothing of the Worker's own changed):
// - what it fetches from outside (GitHub, GitLab, AWS, Cloudflare's API...) reaches the test kit's stand-in at
//   `SUPERCI_STAND_IN`, as `io::stand_in` has it for the other control planes;
// - each runner's container (Cloudflare Containers, which a local run has none of) is a stand-in too: what the Worker
//   asks of it is told to the test (`POST <SUPERCI_CONTAINERS>/<op>`), which answers how it goes;
// - what the test asks of the runtime itself, at `/__test/...` (only here): whether it answers, a Durable Object's
//   alarm now, the containers as Cloudflare may have them, the class kept from before 0.5.0.
import * as built from "BUILT";

let standIn = "", containers = "";
const given = (env) => { standIn = env.SUPERCI_STAND_IN ?? ""; containers = env.SUPERCI_CONTAINERS ?? "" };
const outside = (host) => host.endsWith(".amazonaws.com") || host.endsWith(".signin.aws.amazon.com") || host.endsWith(".blob.core.windows.net") || host === "api.github.com"
  || host.endsWith(".ghe.com") || host === "gitlab.com" || host === "api.cloudflare.com" || host === "dash.cloudflare.com" || host === "registry.cloudflare.com"
  || host === "ghcr.io" || host.endsWith(".workers.dev") || host.endsWith(".modal.run");
const realFetch = globalThis.fetch;
globalThis.fetch = (input, init) => {
  const request = new Request(input, init);
  const url = new URL(request.url);
  if (!standIn || url.protocol !== "https:" || !outside(url.hostname)) return realFetch(request);
  return realFetch(new Request(`${standIn.replace(/\/$/, "")}/${url.host}${url.pathname}${url.search}`, request));
};

// How the containers are, as the test sets them (`POST /__test/containers`): none at all (a Worker without its
// container application), no runner image named, starts or keep-alives answered at once rather than as a promise, or
// refused at once (thrown, `throwStart`, `throwKeep`); and the Durable Objects' storage operations named in
// `failStorage` failing (as Cloudflare's storage fails one: the promise rejected).
let mode = {};

// What the test says of a container operation: `{}` (done), `{"throw": "why"}` (thrown as an Error; with `"as"`:
// "string", "object" or "nothing" thrown as that), `{"value": ...}`.
async function told(op, body) {
  const r = await realFetch(`${containers.replace(/\/$/, "")}/${op}`, { method: "POST", body: JSON.stringify(body) });
  const said = await r.json();
  if (said.throw !== undefined) {
    if (said.as === "string") throw said.throw;
    if (said.as === "object") throw { said: said.throw };
    if (said.as === "nothing") { const o = {}; o.self = o; throw o }
    throw new Error(said.throw);
  }
  return said.value;
}

// A runner's container: as Cloudflare's `ctx.container` is to the Worker (`running`, `start`, `destroy`,
// `setInactivityTimeout`, the image it names), each operation told to the test.
const running = new Map();
function container(id) {
  return {
    get running() { return running.get(id) === true },
    get images() { return mode.noImage ? undefined : { runner: "registry.cloudflare.com/acme/superci-runner:test" } },
    start(options) {
      if (mode.throwStart) throw new Error(mode.throwStart);
      if (mode.syncStart) { running.set(id, true); told("start", { id, options }).catch(() => {}); return undefined }
      return told("start", { id, options }).then(() => { running.set(id, true) });
    },
    destroy(reason) { return told("destroy", { id, reason: reason ?? null }).then(() => { running.set(id, false) }) },
    setInactivityTimeout(ms) {
      if (mode.throwKeep) throw new Error(mode.throwKeep);
      if (mode.syncKeep) { told("keep", { id, ms }).catch(() => {}); return undefined }
      return told("keep", { id, ms });
    },
  };
}

// What a Durable Object really has, as it is (its own `this`), unless `instead` says otherwise.
const as = (t, p, instead) => { const v = instead(p); if (v !== undefined) return v; const r = Reflect.get(t, p, t); return typeof r === "function" ? r.bind(t) : r };

// A Durable Object's state, its storage's operations failing as the test says.
function withStorage(ctx, more = () => undefined) {
  const storage = new Proxy(ctx.storage, { get: (t, p) => as(t, p, (p) => mode.failStorage?.includes(p) ? () => Promise.reject(new Error(`storage ${p} failed`)) : undefined) });
  return new Proxy(ctx, { get: (t, p) => p === "storage" ? storage : as(t, p, more) });
}

// The same, with that container in it.
function withContainer(ctx) {
  const c = container(ctx.id.toString());
  return withStorage(ctx, (p) => p === "container" ? (mode.noContainer ? null : c) : undefined);
}

// A Durable Object's alarm, run now (as the runtime runs it when it is due).
const alarmNow = (self, request) => new URL(request.url).pathname === "/__test/alarm" ? self.alarm() : null;

export default class extends built.default {
  async fetch(request) {
    given(this.env);
    const url = new URL(request.url);
    // (Whether it answers at all: the module loaded, nothing done.)
    if (url.pathname === "/__test/ready") return new Response("ok");
    if (url.pathname === "/__test/containers") { mode = await request.json(); return new Response("ok") }
    if (url.pathname === "/__test/plane-alarm") return this.env.PLANE.get(this.env.PLANE.idFromName("plane")).fetch("https://plane/__test/alarm");
    if (url.pathname === "/__test/runner-alarm") {
      const name = url.searchParams.get("name");
      const id = /^[0-9a-f]{64}$/.test(name) ? this.env.RUNNER.idFromString(name) : this.env.RUNNER.idFromName(name);
      return this.env.RUNNER.get(id).fetch("https://runner/__test/alarm");
    }
    if (url.pathname === "/__test/runner") {
      // (A runner asked directly, as the control plane's Durable Object asks it: `/start` or `/stop`.)
      const name = url.searchParams.get("name"), what = url.searchParams.get("what");
      return this.env.RUNNER.get(this.env.RUNNER.idFromName(name)).fetch(`https://runner/${what}`, { method: "POST", body: await request.text() });
    }
    if (url.pathname === "/__test/legacy") return this.env.LEGACY.get(this.env.LEGACY.idFromName("old")).fetch("https://runner/start", { method: "POST", body: "{}" });
    return super.fetch(request);
  }
}
// (The control plane's own Durable Object, left out for a Worker that has none: the launcher's.)
// PLANE {
export class PlaneObject extends built.PlaneObject {
  constructor(ctx, env) { given(env); super(withStorage(ctx), env) }
  async fetch(request) { return await alarmNow(this, request) ?? super.fetch(request) }
}
// } PLANE
export class JobRunner extends built.JobRunner {
  constructor(ctx, env) { given(env); super(withContainer(ctx), env) }
  async fetch(request) { return await alarmNow(this, request) ?? super.fetch(request) }
}
export class Runner extends built.Runner {}
