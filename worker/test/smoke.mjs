// Smoke test for ebobo-signal. Usage: node test/smoke.mjs [url]
import https from "node:https";
import http from "node:http";

const BASE = (process.argv[2] ?? "https://ebobo-signal.mc-se.workers.dev").replace(/\/$/, "");
const WS = BASE.replace(/^http/, "ws");
const ALPHA = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
const code = () => Array.from({ length: 5 }, () => ALPHA[Math.floor(Math.random() * ALPHA.length)]).join("");
let failed = 0;
const check = (name, ok, extra = "") => {
  if (!ok) failed++;
  console.log(`${ok ? "PASS" : "FAIL"} ${name}${extra ? " — " + extra : ""}`);
};

// HTTP status of a would-be WebSocket upgrade (Node's WebSocket hides it).
function upgradeStatus(path) {
  return new Promise((resolve) => {
    const lib = BASE.startsWith("https") ? https : http;
    const req = lib.request(BASE + path, {
      // RFC 6455 sample key ("the sample nonce"), not a secret.
      headers: { Connection: "Upgrade", Upgrade: "websocket", "Sec-WebSocket-Version": "13", "Sec-WebSocket-Key": "dGhlIHNhbXBsZSBub25jZQ==" }, // gitleaks:allow
    });
    req.on("upgrade", (res, sock) => { sock.destroy(); resolve({ status: 101 }); });
    req.on("response", (res) => {
      let body = "";
      res.on("data", (c) => (body += c));
      res.on("end", () => resolve({ status: res.statusCode, body }));
    });
    req.on("error", (e) => resolve({ status: 0, body: String(e) }));
    req.end();
  });
}

class Client {
  constructor(c, role, name, extra = "") {
    this.queue = [];
    this.waiters = [];
    this.closed = new Promise((r) => (this.onClose = r));
    this.ws = new WebSocket(`${WS}/v1/room/${c}?role=${role}&name=${encodeURIComponent(name)}${extra}`);
    this.ws.binaryType = "arraybuffer";
    this.ws.onmessage = (e) => {
      const m = typeof e.data === "string" ? (e.data.startsWith("{") ? JSON.parse(e.data) : e.data) : [...new Uint8Array(e.data)];
      const w = this.waiters.findIndex((x) => x.pred(m));
      if (w >= 0) this.waiters.splice(w, 1)[0].resolve(m);
      else this.queue.push(m);
    };
    this.ws.onclose = (e) => this.onClose({ code: e.code, reason: e.reason });
  }
  next(pred, ms = 5000) {
    const i = this.queue.findIndex(pred);
    if (i >= 0) return Promise.resolve(this.queue.splice(i, 1)[0]);
    return new Promise((resolve) => {
      const w = { pred, resolve };
      this.waiters.push(w);
      setTimeout(() => { const k = this.waiters.indexOf(w); if (k >= 0) { this.waiters.splice(k, 1); resolve(null); } }, ms);
    });
  }
  msg(t, ms) { return this.next((m) => m && m.t === t, ms); }
  waitClose(ms = 5000) { return Promise.race([this.closed, new Promise((r) => setTimeout(() => r(null), ms))]); }
}

const health = await fetch(BASE + "/").then(async (r) => [r.status, await r.text()]).catch((e) => [0, String(e)]);
check("health 200", health[0] === 200 && health[1] === "ebobo-signal ok", `${health[0]} ${health[1]}`);

const C = code();
const host = new Client(C, "host", "Host");
const hw = await host.msg("welcome");
check("host welcome id 0", hw?.id === 0, JSON.stringify(hw));

const dup = await upgradeStatus(`/v1/room/${C}?role=host&name=x`);
check("second host 409 room_taken", dup.status === 409 && dup.body.includes("room_taken"), `${dup.status} ${dup.body}`);

const g1 = new Client(C.toLowerCase(), "guest", "Guest1");
const gw = await g1.msg("welcome");
check("guest welcome id 1 lists host", gw?.id === 1 && gw.peers?.length === 1 && gw.peers[0].id === 0 && gw.peers[0].name === "Host", JSON.stringify(gw));
const hj = await host.msg("joined");
check("host gets joined", hj?.id === 1 && hj.name === "Guest1", JSON.stringify(hj));

host.ws.send(JSON.stringify({ t: "x", to: 1, from: 3 }));
const fx = await g1.msg("x");
check("text forward from 0", fx?.from === 0 && fx.to === 1, JSON.stringify(fx));

host.ws.send(new Uint8Array([1, 9, 8, 7]));
const bin = await g1.next((m) => Array.isArray(m));
check("binary relay [0,9,8,7]", JSON.stringify(bin) === "[0,9,8,7]", JSON.stringify(bin));

g1.ws.send("ping");
const pong = await g1.next((m) => m === "pong");
check("ping -> pong", pong === "pong", String(pong));

const g2 = new Client(C, "guest", "Guest2");
const g3 = new Client(C, "guest", "Guest3");
const [w2, w3] = await Promise.all([g2.msg("welcome"), g3.msg("welcome")]);
const ids = [w2?.id, w3?.id].sort().join(",");
const full = await upgradeStatus(`/v1/room/${C}?role=guest&name=x`);
check("room fills with 3 guests, 4th 409 room_full", ids === "2,3" && full.status === 409 && full.body.includes("room_full"), `ids ${ids}; ${full.status} ${full.body}`);

g3.ws.close();
await host.msg("left");
host.ws.send(JSON.stringify({ t: "lock" }));
await new Promise((r) => setTimeout(r, 500));
const locked = await upgradeStatus(`/v1/room/${C}?role=guest&name=x`);
check("locked room 409 room_locked", locked.status === 409 && locked.body.includes("room_locked"), `${locked.status} ${locked.body}`);

const empty = await upgradeStatus(`/v1/room/${code()}?role=guest&name=x`);
check("guest on empty code 404 no_room", empty.status === 404 && empty.body.includes("no_room"), `${empty.status} ${empty.body}`);

// Node's WebSocket fires `close` only after TCP teardown, which the edge may delay ~10 s.
host.ws.close();
const [l1, c1, c2] = await Promise.all([g1.next((m) => m?.t === "left" && m.id === 0), g1.waitClose(20000), g2.waitClose(20000)]);
check("host close: left id 0, guests closed 4000", l1?.id === 0 && c1?.code === 4000 && c1.reason === "host_left" && c2?.code === 4000, `${JSON.stringify(l1)} ${JSON.stringify(c1)} ${JSON.stringify(c2)}`);

// ---- Drops and rejoin (PROTOCOL.md 1.3). `grace=4000` is the test hook shortening the 60 s
// reservation; any close code other than 1000/1005 counts as a drop. ----
const R = code();
const G = "&grace=4000";
const rh = new Client(R, "host", "RHost", G);
const rhw = await rh.msg("welcome");
check("welcome carries secret", typeof rhw?.secret === "string" && rhw.secret.length >= 16, JSON.stringify(rhw));
const rg = new Client(R, "guest", "RGuest", G);
const rgw = await rg.msg("welcome");
await rh.msg("joined");
rg.ws.close(4100, "test_drop");
const away1 = await rh.msg("away");
check("guest drop: host gets away", away1?.id === rgw?.id, JSON.stringify(away1));
const bad = await upgradeStatus(`/v1/room/${R}?role=guest&name=x&rejoin=${rgw?.id}&secret=deadbeef`);
check("rejoin with wrong secret 409 bad_rejoin", bad.status === 409 && bad.body.includes("bad_rejoin"), `${bad.status} ${bad.body}`);
const rg2 = new Client(R, "guest", "RGuest", `${G}&rejoin=${rgw?.id}&secret=${rgw?.secret}`);
const [rg2w, back1] = await Promise.all([rg2.msg("welcome"), rh.msg("back")]);
check("rejoin keeps id, host gets back", rg2w?.id === rgw?.id && rg2w?.secret === rgw?.secret && back1?.id === rgw?.id, `${JSON.stringify(rg2w)} ${JSON.stringify(back1)}`);
rh.ws.send(new Uint8Array([rgw?.id, 5, 6]));
const rbin = await rg2.next((m) => Array.isArray(m));
check("relay works after rejoin", JSON.stringify(rbin) === "[0,5,6]", JSON.stringify(rbin));

// Rejoin while the server still thinks the old socket is open: the new one takes over.
const rg3 = new Client(R, "guest", "RGuest", `${G}&rejoin=${rgw?.id}&secret=${rgw?.secret}`);
const [rg3w, oldClosed] = await Promise.all([rg3.msg("welcome"), rg2.waitClose(20000)]);
check("rejoin over a live socket replaces it (4002)", rg3w?.id === rgw?.id && oldClosed?.code === 4002, `${JSON.stringify(rg3w)} ${JSON.stringify(oldClosed)}`);

rh.ws.close(4100, "test_drop");
const away0 = await rg3.msg("away");
check("host drop: guests get away id 0, room stays", away0?.id === 0, JSON.stringify(away0));
const rh2 = new Client(R, "host", "RHost", `${G}&rejoin=0&secret=${rhw?.secret}`);
const [rh2w, back0] = await Promise.all([rh2.msg("welcome"), rg3.msg("back")]);
check("host rejoins id 0 with guest listed, guests get back", rh2w?.id === 0 && rh2w.peers?.some((x) => x.id === rgw?.id) && back0?.id === 0, `${JSON.stringify(rh2w)} ${JSON.stringify(back0)}`);
rg3.ws.send(new Uint8Array([0, 7]));
const rbin2 = await rh2.next((m) => Array.isArray(m));
check("room usable after host rejoin", JSON.stringify(rbin2) === `[${rgw?.id},7]`, JSON.stringify(rbin2));

rg3.ws.close(4100, "test_drop");
await rh2.msg("away");
const gone = await rh2.msg("left", 20000);
check("dropped guest -> left after grace", gone?.id === rgw?.id, JSON.stringify(gone));
const late = await upgradeStatus(`/v1/room/${R}?role=guest&name=x&rejoin=${rgw?.id}&secret=${rgw?.secret}`);
check("rejoin after grace 409", late.status === 409, `${late.status} ${late.body}`);

// The host gives up on a dropped guest: `left` at once (well inside the 4 s grace), the id is
// free for the next guest, and the old secret no longer rejoins. A guest may not drop anyone.
const rg5 = new Client(R, "guest", "RGuest5", G);
const rg5w = await rg5.msg("welcome");
await rh2.msg("joined");
const rg6 = new Client(R, "guest", "RGuest6", G);
const rg6w = await rg6.msg("welcome");
await rh2.msg("joined");
rg5.ws.close(4100, "test_drop");
await rh2.msg("away");
rg6.ws.send(JSON.stringify({ t: "drop", id: rg5w?.id }));
const early = await rh2.msg("left", 1000);
check("a guest cannot drop a player", early === null, JSON.stringify(early));
rh2.ws.send(JSON.stringify({ t: "drop", id: rg5w?.id }));
const [dropH, dropG] = await Promise.all([rh2.msg("left", 1500), rg6.msg("left", 1500)]);
check("host drops an away guest: left at once for everyone", dropH?.id === rg5w?.id && dropG?.id === rg5w?.id, `${JSON.stringify(dropH)} ${JSON.stringify(dropG)}`);
const stale = await upgradeStatus(`/v1/room/${R}?role=guest&name=x&rejoin=${rg5w?.id}&secret=${rg5w?.secret}`);
check("dropped guest's secret no longer rejoins", stale.status === 409 && stale.body.includes("bad_rejoin"), `${stale.status} ${stale.body}`);
const rg7 = new Client(R, "guest", "RGuest7", G);
const rg7w = await rg7.msg("welcome");
check("the freed id goes to the next guest", rg7w?.id === rg5w?.id && rg6w?.id !== rg5w?.id, JSON.stringify(rg7w));
rh2.ws.send(JSON.stringify({ t: "drop", id: rg6w?.id }));
const live = await rg6.msg("left", 1000);
check("a connected guest is not dropped", live === null, JSON.stringify(live));
rg6.ws.close();
rg7.ws.close();
await rh2.msg("left");
await rh2.msg("left");

const rg4 = new Client(R, "guest", "RGuest4", G);
await rg4.msg("welcome");
rh2.ws.close(4100, "test_drop");
await rg4.msg("away");
const [hostGone, hostClose] = await Promise.all([rg4.next((m) => m?.t === "left" && m.id === 0, 20000), rg4.waitClose(30000)]);
check("dropped host -> left id 0 + close 4000 after grace", hostGone?.id === 0 && hostClose?.code === 4000, `${JSON.stringify(hostGone)} ${JSON.stringify(hostClose)}`);

process.exit(failed ? 1 : 0);
