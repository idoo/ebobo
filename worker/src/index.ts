import { DurableObject } from "cloudflare:workers";

interface Env {
  ROOM: DurableObjectNamespace<Room>;
}

type Role = "host" | "guest";
interface Peer {
  id: number;
  name: string;
  role: Role;
  /** Issued in `welcome`; proves identity on `?rejoin=<id>&secret=<secret>`. */
  secret: string;
  /** How long this peer's id stays reserved after an abnormal close (ms). */
  grace: number;
}
/** A peer whose socket dropped; its id is reserved until `until`. Storage key `away:<id>`. */
interface Away {
  id: number;
  name: string;
  role: Role;
  secret: string;
  until: number;
}

const ALPHABET = /^[ABCDEFGHJKLMNPQRSTUVWXYZ23456789]{5}$/;
const MAX_TEXT = 4096;
const MAX_BINARY = 2048;
const NO_GUEST_MS = 60 * 60 * 1000;
const MAX_ROOM_MS = 12 * 60 * 60 * 1000;
/** Reservation of a dropped peer's id. `?grace=<ms>` (test hook) can only shorten it. */
const GRACE_MS = 60 * 1000;
/** Close codes that mean "I am leaving" (everything else is a drop that may come back). */
const LEAVE_CODES = new Set([1000, 1005]);

function err(status: number, code: string): Response {
  return new Response(JSON.stringify({ error: code }), {
    status,
    headers: { "content-type": "application/json" },
  });
}

export default {
  async fetch(req: Request, env: Env): Promise<Response> {
    const url = new URL(req.url);
    if (req.method === "GET" && url.pathname === "/") {
      return new Response("ebobo-signal ok", { headers: { "content-type": "text/plain" } });
    }
    const m = url.pathname.match(/^\/v1\/room\/([^/]+)$/);
    if (!m || req.method !== "GET") return err(404, "not_found");
    const code = decodeURIComponent(m[1]).toUpperCase();
    const role = url.searchParams.get("role");
    if (!ALPHABET.test(code) || (role !== "host" && role !== "guest")) return err(400, "bad_request");
    if ((req.headers.get("Upgrade") ?? "").toLowerCase() !== "websocket") return err(426, "bad_request");
    return env.ROOM.get(env.ROOM.idFromName(code)).fetch(req);
  },
} satisfies ExportedHandler<Env>;

export class Room extends DurableObject<Env> {
  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair("ping", "pong"));
  }

  private peers(exclude?: WebSocket): { ws: WebSocket; p: Peer }[] {
    const out: { ws: WebSocket; p: Peer }[] = [];
    for (const ws of this.ctx.getWebSockets("peer")) {
      if (ws === exclude || ws.readyState !== WebSocket.OPEN) continue;
      const p = ws.deserializeAttachment() as Peer | null;
      if (p) out.push({ ws, p });
    }
    return out;
  }

  private static send(ws: WebSocket, data: string | ArrayBuffer): void {
    try {
      ws.send(data);
    } catch {
      /* socket already gone */
    }
  }

  private async awayPeers(): Promise<Map<number, Away>> {
    const out = new Map<number, Away>();
    for (const a of (await this.ctx.storage.list<Away>({ prefix: "away:" })).values()) out.set(a.id, a);
    return out;
  }

  private broadcast(msg: string, exclude?: WebSocket): void {
    for (const x of this.peers(exclude)) Room.send(x.ws, msg);
  }

  async fetch(req: Request): Promise<Response> {
    const url = new URL(req.url);
    const role = url.searchParams.get("role") as Role;
    let name = (url.searchParams.get("name") ?? "").trim().slice(0, 16);
    if (!name) name = "Player";
    const graceParam = Number(url.searchParams.get("grace") ?? "");
    const grace = Number.isFinite(graceParam) && graceParam > 0 ? Math.min(graceParam, GRACE_MS) : GRACE_MS;
    const rejoin = url.searchParams.get("rejoin");

    const peers = this.peers();
    const away = await this.awayPeers();
    let me: Peer;
    let back = false;
    let replaced = false;
    if (rejoin !== null) {
      const rid = Number(rejoin);
      const secret = url.searchParams.get("secret") ?? "";
      const a = away.get(rid);
      const live = peers.find((x) => x.p.id === rid);
      if (secret && a && a.secret === secret && a.role === role) {
        await this.ctx.storage.delete(`away:${rid}`);
        me = { id: rid, name: a.name, role: a.role, secret, grace };
        back = true;
      } else if (secret && live && live.p.secret === secret && live.p.role === role) {
        // The server has not noticed the drop yet: the new socket takes over silently.
        live.ws.serializeAttachment(null);
        try {
          live.ws.close(4002, "replaced");
        } catch {
          /* already gone */
        }
        me = { ...live.p, grace };
        replaced = true;
      } else {
        return err(409, "bad_rejoin");
      }
    } else {
      const all = [...peers.map((x) => x.p), ...away.values()];
      const hasHost = all.some((p) => p.role === "host");
      let id: number;
      if (role === "host") {
        if (hasHost) return err(409, "room_taken");
        id = 0;
      } else {
        if (!hasHost) return err(404, "no_room");
        if (all.length >= 4) return err(409, "room_full");
        if (await this.ctx.storage.get<boolean>("locked")) return err(409, "room_locked");
        const used = new Set(all.map((p) => p.id));
        id = [1, 2, 3].find((i) => !used.has(i)) ?? -1;
        if (id < 0) return err(409, "room_full");
      }
      if (role === "host") {
        const now = Date.now();
        await this.ctx.storage.deleteAll();
        await this.ctx.storage.put({ locked: false, created: now });
        await this.ctx.storage.setAlarm(now + NO_GUEST_MS);
      }
      const secret = Array.from(crypto.getRandomValues(new Uint8Array(16)), (x) => x.toString(16).padStart(2, "0")).join("");
      me = { id, name, role, secret, grace };
    }

    const pair = new WebSocketPair();
    const [client, server] = [pair[0], pair[1]];
    this.ctx.acceptWebSocket(server, ["peer"]);
    server.serializeAttachment(me);

    // Re-read after the awaits so concurrent joins are reflected.
    const others = this.peers(server);
    const stillAway = [...(await this.awayPeers()).values()].filter((a) => a.id !== me.id);
    Room.send(
      server,
      JSON.stringify({
        t: "welcome",
        id: me.id,
        secret: me.secret,
        peers: [
          ...others.map((x) => ({ id: x.p.id, name: x.p.name })),
          ...stillAway.map((a) => ({ id: a.id, name: a.name, away: true })),
        ],
        ip: req.headers.get("CF-Connecting-IP") ?? "",
      }),
    );
    if (back) this.broadcast(JSON.stringify({ t: "back", id: me.id }), server);
    else if (!replaced) this.broadcast(JSON.stringify({ t: "joined", id: me.id, name: me.name }), server);
    return new Response(null, { status: 101, webSocket: client });
  }

  async webSocketMessage(ws: WebSocket, msg: string | ArrayBuffer): Promise<void> {
    const me = ws.deserializeAttachment() as Peer | null;
    if (!me) return;
    if (typeof msg === "string") {
      if (msg.length > MAX_TEXT || new TextEncoder().encode(msg).length > MAX_TEXT) return;
      let obj: unknown;
      try {
        obj = JSON.parse(msg);
      } catch {
        return;
      }
      if (!obj || typeof obj !== "object" || Array.isArray(obj)) return;
      const o = obj as Record<string, unknown>;
      if ((o.t === "lock" || o.t === "unlock") && !("to" in o)) {
        if (me.role === "host") await this.ctx.storage.put("locked", o.t === "lock");
        return;
      }
      // The host has given up on a guest whose connection dropped (its game timed the player
      // out): the id is free at once instead of after the grace, so the player can join again
      // right away, also into a room that would otherwise still count as full.
      if (o.t === "drop" && typeof o.id === "number" && !("to" in o)) {
        if (me.role !== "host") return;
        const gone = (await this.awayPeers()).get(o.id);
        if (gone && gone.role === "guest") {
          await this.ctx.storage.delete(`away:${gone.id}`);
          this.broadcast(JSON.stringify({ t: "left", id: gone.id }));
        }
        return;
      }
      const to = o.to;
      if (to !== "*" && typeof to !== "number") return;
      o.from = me.id;
      const out = JSON.stringify(o);
      for (const x of this.peers(ws)) if (to === "*" || x.p.id === to) Room.send(x.ws, out);
      return;
    }
    const bytes = new Uint8Array(msg);
    if (bytes.length < 1 || bytes.length > MAX_BINARY) return;
    const dest = bytes[0];
    const out = new Uint8Array(bytes);
    out[0] = me.id;
    for (const x of this.peers(ws)) if (dest === 0xff || x.p.id === dest) Room.send(x.ws, out.buffer);
  }

  async webSocketClose(ws: WebSocket, code: number, reason: string): Promise<void> {
    if (LEAVE_CODES.has(code)) await this.leave(ws);
    else await this.drop(ws);
  }

  async webSocketError(ws: WebSocket): Promise<void> {
    await this.drop(ws);
  }

  /** Abnormal close: keep the id reserved for `grace` ms, tell the others `away`. */
  private async drop(ws: WebSocket): Promise<void> {
    const me = ws.deserializeAttachment() as Peer | null;
    ws.serializeAttachment(null);
    try {
      ws.close(1011, "dropped");
    } catch {
      /* already closed */
    }
    if (!me) return;
    const until = Date.now() + me.grace;
    const away: Away = { id: me.id, name: me.name, role: me.role, secret: me.secret, until };
    await this.ctx.storage.put(`away:${me.id}`, away);
    this.broadcast(JSON.stringify({ t: "away", id: me.id }));
    const alarm = await this.ctx.storage.getAlarm();
    if (alarm === null || alarm > until) await this.ctx.storage.setAlarm(until);
  }

  /** Intentional leave (close 1000/1005). A leaving host closes the room. */
  private async leave(ws: WebSocket): Promise<void> {
    const me = ws.deserializeAttachment() as Peer | null;
    ws.serializeAttachment(null);
    try {
      ws.close(1000, "bye");
    } catch {
      /* already closed */
    }
    if (!me) return;
    this.broadcast(JSON.stringify({ t: "left", id: me.id }));
    if (me.role === "host") await this.closeRoom();
  }

  /** Host gone for good: guests get close 4000 (their attachments are cleared first so their
   * close events are ignored) and the room forgets everything. */
  private async closeRoom(): Promise<void> {
    for (const x of this.peers()) {
      x.ws.serializeAttachment(null);
      try {
        x.ws.close(4000, "host_left");
      } catch (e) {
        console.log("close guest failed", String(e));
      }
    }
    await this.ctx.storage.deleteAlarm();
    await this.ctx.storage.deleteAll();
  }

  async alarm(): Promise<void> {
    const now = Date.now();
    const away = await this.awayPeers();
    for (const a of away.values()) {
      if (a.until > now) continue;
      away.delete(a.id);
      await this.ctx.storage.delete(`away:${a.id}`);
      this.broadcast(JSON.stringify({ t: "left", id: a.id }));
      if (a.role === "host") {
        await this.closeRoom();
        return;
      }
    }
    const peers = this.peers();
    const created = (await this.ctx.storage.get<number>("created")) ?? 0;
    const hasGuest = peers.some((x) => x.p.role === "guest") || [...away.values()].some((a) => a.role === "guest");
    if ((peers.length === 0 && away.size === 0) || now - created >= MAX_ROOM_MS || (!hasGuest && now - created >= NO_GUEST_MS)) {
      for (const x of peers) {
        x.ws.serializeAttachment(null);
        try {
          x.ws.close(4001, "room_expired");
        } catch {
          /* ignore */
        }
      }
      await this.ctx.storage.deleteAll();
      return;
    }
    // Check again at the next away deadline, the 12 h mark, or hourly in case all guests leave.
    let next = Math.min(created + MAX_ROOM_MS, now + NO_GUEST_MS);
    for (const a of away.values()) next = Math.min(next, a.until);
    await this.ctx.storage.setAlarm(next);
  }
}
