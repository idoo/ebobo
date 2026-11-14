# ebobo network protocol (v1)

Up to 4 players. Peer 0 is the host (NES controller 1 by default), guests get ids 1..3.
GGRS runs as a full mesh: every peer exchanges inputs with every other peer, each pair over
its own route (direct UDP when hole punching works, otherwise relayed through the signaling
server's WebSocket).

Layers:

1. **Rendezvous**: room on the signaling server (WebSocket), or direct IP (2 players only).
2. **Routes**: per pair of peers, UDP hole punching, falling back to relay.
3. **Lobby**: over routes: game info, ROM check/transfer, ready, start (reliable CTRL channel).
4. **Game**: GGRS messages over routes.

## 1. Signaling server (Cloudflare Worker `ebobo-signal`, one Durable Object per room)

Default URL: `https://ebobo-signal.<account>.workers.dev` (client config `server`).

`GET /` → `200 text/plain` `ebobo-signal ok` (health).

`GET /v1/room/<CODE>?role=host|guest&name=<urlencoded nickname>` with `Upgrade: websocket`.

- `CODE`: 5 characters from `ABCDEFGHJKLMNPQRSTUVWXYZ23456789` (no I, O, 0, 1). Server and
  clients upper-case it; clients also strip spaces and dashes and reject characters outside
  the alphabet before connecting. The host picks a random code client-side and retries with a
  new one on `409 room_taken`.
- `name`: up to 16 chars, server truncates; default `Player`.
- HTTP errors before upgrade (JSON body `{"error": "<code>"}`):
  - `400 bad_request`: bad code, role or missing upgrade header (`426` if not websocket).
  - `409 room_taken`: role=host and the room already has a host (connected or away, 1.3).
  - `404 no_room`: role=guest and the room has no host.
  - `409 room_full`: role=guest and 4 peers are connected or away.
  - `409 bad_rejoin`: `rejoin` with an unknown/expired id, a wrong `secret` or another role.
  - `409 room_locked`: role=guest and the host locked the room (game started).
- Peer ids: host = 0; a guest gets the lowest free id in 1..3.

### Text frames (JSON, UTF-8)

Server → client:

| Message | When |
|---|---|
| `{"t":"welcome","id":<n>,"secret":"<32 hex>","peers":[{"id":0,"name":"Ivan"},{"id":2,"name":"Ola","away":true},...],"ip":"<public ip>"}` | right after accept (also after a rejoin, same id and secret); `peers` = everyone else connected or away; `secret` = per-peer random secret for rejoining; `ip` = `CF-Connecting-IP` |
| `{"t":"joined","id":<n>,"name":"..."}` | to everyone else when a peer connects |
| `{"t":"away","id":<n>}` | to everyone else when a peer's socket dropped (1.3) |
| `{"t":"back","id":<n>}` | to everyone else when an away peer rejoined |
| `{"t":"left","id":<n>}` | to everyone else when a peer left on purpose, or stayed away past the grace period |

When the host leaves (close 1000/1005) or stays away past the grace period, every guest gets
`left` for id 0 and the server closes the guests' sockets with close code `4000` reason
`host_left`; the room is gone.

Client → server:

| Message | Effect |
|---|---|
| `{"t":"lock"}` | host only: further guests get `409 room_locked` |
| `{"t":"unlock"}` | host only |
| any object with `"to": <id>` | forwarded to peer `<id>` with `"from": <sender id>` set by the server (client-supplied `from` is overwritten); dropped if no such peer |
| any object with `"to": "*"` | same, to every other peer |
| literal text `ping` | server answers `pong` (auto-response, keeps NAT/proxies alive; clients send every 20 s) |

Text frames over 4 KiB are dropped.

### Binary frames (relay)

Client → server: `[dest_id: u8][packet...]`. Server → client: `[src_id: u8][packet...]` (the
server replaces the first byte with the sender's id). `dest_id = 0xFF` broadcasts. Frames to an
absent peer are dropped. Frames over 2048 bytes are dropped. `packet` is exactly a UDP packet
from section 3, so the client treats relayed and UDP packets the same way.

Limits: a room with no guest is closed after 1 hour; any room is closed after 12 hours.

### 1.3 Drops and rejoin

A socket closed with code 1000 or 1005 (no code) is a leave. Any other close (1006 on a network
drop, socket errors, client codes 4xxx) is a **drop**: the server keeps the peer's id, name,
role and secret reserved for the grace period (60 s) and sends `away` to the others. The host
is no exception: a room whose host dropped stays open (guests may even join) until the host is
back or the grace period ends.

The client reconnects by itself after a drop (lobby and game alike), with backoff 0.5, 1, 2,
4, 4, ... s, giving up 60 s after the drop:

`GET /v1/room/<CODE>?role=<same role>&name=<...>&rejoin=<id>&secret=<secret from welcome>`

The server answers with a fresh `welcome` (same id and secret, current `peers`) and sends
`back` to the others. If the server has not noticed the drop yet (the old socket still looks
open), the new socket takes over and the old one is closed with `4002 replaced`, without
`away`/`back`. Rejoin ignores lock and fullness. An unknown or expired id, a wrong secret or a
different role → `409 bad_rejoin`, and the client gives up (the room or its id is gone).

On the client side relay frames sent while reconnecting are dropped (GGRS resends inputs);
signaling text is queued (up to 256 frames) and sent after the rejoin. The client compares the
rejoin `welcome` with the peers it knows: missing ones are treated as `left`, new ones as
`joined`; known peers keep their routes. Server close codes 4000-4999 (`4000 host_left`,
`4001 room_expired`, `4002 replaced`) are final and never rejoined.

Test hook: `&grace=<ms>` on the connect URL shortens that peer's grace period (it is capped at
60 000 ms, so it can never extend a reservation). `worker/test/smoke.mjs` uses `grace=4000`.

## 2. Route establishment (per pair of peers)

Each client binds one UDP socket for the whole session and uses it for STUN, punching and game
traffic: dual-stack `[::]:0` (IPV6_V6ONLY off), falling back to `0.0.0.0:0` when IPv6 is
unavailable; a direct-IP host keeps its configured port. Addresses are stored and compared in
normalized form: an IPv4-mapped `::ffff:a.b.c.d` is plain IPv4 `a.b.c.d:port`, and IPv4
destinations are mapped automatically when the socket is IPv6.

Candidates, in this order: the socket's LAN addresses (one per non-loopback IPv4 interface, same
port), the machine's global IPv6 addresses (2000::/3 only, never link-local, ULA or loopback;
listed only when the socket is IPv6), then the STUN reflexive address (`stun.cloudflare.com:3478`,
`stun.l.google.com:19302`, RFC 5389 Binding Request, XOR-MAPPED-ADDRESS; IPv4 servers only). All
use Rust `SocketAddr` text form (`192.168.1.6:53211`, `[2001:db8::1]:53211`); a peer whose
socket is IPv4-only skips IPv6 candidates. Punching an IPv6 candidate works like IPv4: the
outbound punch opens stateful firewalls. If two STUN servers report different ports the NAT is
symmetric: punching is still tried but the relay timeout is shortened to 1.5 s.

On `welcome` (for every listed peer) and on `joined`, each side sends to that peer via the
server:

```json
{"t":"cands","to":<peer>,"cands":["192.168.1.20:53211","81.2.3.4:53211"],"token":"<16 hex>","relay_only":false}
```

`token` is the sender's random 64-bit session token; punch packets carry it so stray packets
are ignored. Then both sides send `PUNCH` to every candidate of the other every 50 ms. On
receiving `PUNCH` from a peer, reply `PUNCH_ACK` to the source address. When a side receives a
`PUNCH_ACK` (or a `PUNCH`, which proves the reverse path is open) from a candidate, the route
for that peer becomes `Udp(source address)`.

**Decider**: the lower id of the pair decides the final route and announces it:
`{"t":"route","to":<peer>,"mode":"direct","addr":"<peer address as seen by decider>"}` once it
has a UDP route and has received at least one `PUNCH_ACK`, or
`{"t":"route","to":<peer>,"mode":"relay"}` after 3 s (1.5 s with symmetric NAT) without one,
or immediately if either side set `relay_only` (config `force_relay`). The other side adopts the
decision (for `direct` it uses the address it received packets from). Punching stops once the
route is decided. Relay routes send packets as binary WebSocket frames (section 1).

The mesh outlives the lobby: the game drives the same signaling, punching and route health on
every GGRS poll, and only GGRS and BYE packets reach GGRS (PUNCH, PUNCH_ACK, KEEPALIVE and CTRL
stay in the mesh). Any packet from a direct route's address counts as alive.

Lobby (1 Hz `KEEPALIVE` to every UDP route, NAT mappings expire): a direct route silent for 3 s
is punched again (PUNCH every 50 ms to the route address and the peer's candidates); if it is
still silent after 5 s the decider (lower id) sends `{"t":"route","to":<peer>,"mode":"relay"}`
and both sides switch that pair to the relay.

Game (GGRS sends about 60 packets per second, so silence means something): a direct route silent
for 1 s is re-punched the same way, and every outgoing packet to that peer is also mirrored over
the relay (duplicates are harmless: GGRS ignores inputs it already has). Mirroring stops as soon
as a packet arrives on the direct address. After 3 s of silence the lower id sends `route`
`relay` and both sides switch.

Roaming (lobby and game): a PUNCH or PUNCH_ACK carrying the peer's session token from a new
address moves a decided direct route to that source address once the current address has been
silent for at least 1 s (so late pairing punches cannot flap a working route); the PUNCH is
answered with PUNCH_ACK. This covers NAT rebinding and Wi-Fi → LTE. A relay-routed pair stays on
the relay: a `route` `relay` for a decided direct pair is the only switch the decider ever sends,
and nothing switches back to direct. Direct-IP mode (token 0) never roams.

### Direct IP mode (2 players, no server)

Host binds UDP port `P` (default 7777) and waits. Guest binds `[::]:0` (or `0.0.0.0:0`) and
sends `PUNCH` (token 0, from id 1, to id 0) to `host:P` every 50 ms; host answers `PUNCH_ACK` and
both use that address. The guest accepts `host[:port]`, `[v6]:port` or a bare IPv6 address
(default port 7777; IPv4 preferred when a hostname resolves to both). Ids: host 0, guest 1. Port
forwarding / LAN / VPN (Tailscale) is the user's problem in this mode.

## 3. Packets (UDP payload = relay payload)

All little-endian. Max packet 1200 bytes.

```
[0xEB][kind][body...]
```

| kind | name | body |
|---|---|---|
| 0x01 | PUNCH | `token: u64` of sender, `from: u8`, `to: u8` |
| 0x02 | PUNCH_ACK | `token: u64` of sender, `from: u8`, `to: u8` |
| 0x03 | KEEPALIVE | empty |
| 0x10 | CTRL | `seq: u32`, JSON bytes (lobby message, section 4) |
| 0x11 | CTRL_ACK | `seq: u32` |
| 0x20 | ROM_CHUNK | `offset: u32`, `total: u32`, data (≤ 1024 bytes) |
| 0x21 | ROM_ACK | `next: u32` (all bytes below `next` received) |
| 0x30 | GGRS | bincode 1.3 of `ggrs::Message` |
| 0x3F | BYE | empty (peer is leaving; drop the route) |

Packets from an address that is not a known route are ignored, except PUNCH/PUNCH_ACK with a
valid token.

CTRL is reliable: the sender retransmits until it gets `CTRL_ACK` with that seq (per-peer seq
counter starting at 1), after RTO = clamp(SRTT + 4·RTTVAR, 200 ms, 1 s); the receiver acks every
copy and processes each seq once. RTT samples come only from acks of first transmissions (Karn's
rule) and feed an RFC 6298 estimator per peer (alpha 1/8, beta 1/4).

ROM transfer: sender keeps up to 32 chunks in flight from `next`, retransmits from the last
acked `next` after 300 ms without progress; receiver acks the highest contiguous byte count on
every chunk.

## 4. Lobby (CTRL JSON over routes)

Host → each guest, as soon as the route is up:

```json
{"t":"game","proto":1,"core":"0.1.0","name":"Ivan","rom":{"crc32":"1a2b3c4d","size":131088,"name":"Test Game (USA).nes"},"region":"ntsc","four_score":false}
```

Guest → host: `{"t":"rom","have":true,"name":"Sasha","proto":1,"ver":"0.1.0"}` (the guest
looked the CRC up in its library; `name` is the guest's nickname, needed in direct-IP mode where
no server announces it; `ver` is the guest's ebobo version). If `have` is false the host streams
the raw ROM file (header included) with ROM_CHUNK; the guest verifies the PRG+CHR CRC-32 and
answers the same `rom` message with `have: true`, or `{"t":"error","msg":"rom_crc"}`.

Versions: peers play together only with the same `proto` **and** the same ebobo version
(`core` in `game`, `ver` in `rom`), so e.g. a Mac and a Steam Deck build of different releases
never start a game. A guest whose `proto`/version differs from the host's `game` sends
`{"t":"error","msg":"version"}` and fails with `NetError::Version`. A host that gets a `rom`
with a different or missing `proto`/`ver` sends the guest the same error (the guest fails with
`NetError::Version`), reports `NetError::Version` itself and never counts that guest as ready,
so the game cannot start with it.

Guest → host whenever its state changes: `{"t":"ready","links":[0,2]}` = ROM loaded plus the
peer ids it has a decided route to.

Any → every routed peer every 250 ms while in the lobby: `{"t":"ping"}` (about 25 bytes). The
receiver only acks it (unknown `t` values are acked and ignored); its job is to give the RTT
estimator steady samples for the input delay.

Host → every guest whenever anything changes (lobby UI):

```json
{"t":"players","list":[{"id":0,"name":"Ivan","route":"self","rtt":0,"ready":true},{"id":1,"name":"Sasha","route":"direct","rtt":34,"ready":true}]}
```

The host may start when ≥ 2 peers are present and every guest is ready with links to all other
present peers. Host sends `{"t":"lock"}` to the server, then to every guest:

```json
{"t":"start","slots":[0,1,2],"delay":2,"four_score":true}
```

`slots[i]` = peer id playing NES controller i+1. `delay` = GGRS input delay chosen by the host:
its setting (1-6), or auto = clamp(ceil(R / 2 / frame_ms), 1, 6), where R is the worst peer's
planning RTT: SRTT + 2·RTTVAR once that peer has at least 8 samples, otherwise SRTT·1.25 (RFC
6298's cold-start RTTVAR = R/2 would add a whole extra RTT). A peer with no samples counts as
0 ms, and the start never waits for samples. frame_ms is 16.64 (NTSC) or 20 (PAL). `four_score`
= true when more than 2 slots are used or the ROM header asks for it. Each peer starts GGRS when
it receives (guests) or sends (host) `start`.

`{"t":"leave"}` (any → all) and BYE packets end the lobby/game for that peer.

## 5. Game (GGRS)

`ggrs::Config`: `Input = u16` (bits 0-7 NES pad, bit 8 = reset request), `State =
ebobo_core::Snapshot`, `Address = PeerId(u8)`. Players = `slots.len()`, local handle = own slot
index, every other slot is `PlayerType::Remote(PeerId)`. FPS = 60 (NTSC) / 50 (PAL). The
session takes the `Link` from `GameStart.link` (the lobby's live mesh, section 2), or
`Link::udp(socket, me, peers)` for fixed UDP routes without a server; every GGRS poll also
drives its signaling, route health, punching, relay mirroring and roaming. A BYE from a peer
disconnects its player.
Max prediction 10, desync detection every 60 frames (checksum = `Nes::state_hash` truncated to
u128), disconnect timeout 20 s, notify after 1.5 s. A frame whose combined input has any reset
bit runs `Nes::reset()` before `run_frame`.

Tuning (measured with the network profiles in `crates/ebobo-net/src/session_tests.rs`, release
build): prediction 10 instead of 8 cuts stalls on a 150 ms one-way link from 14-33 % to 4-11 % of
frames; a 4 s blackout never disconnects and 20 s lets a phone in a tunnel come back. No profile
(mobile bursty loss, 150 ms far, 15/180 ms asymmetric, 4 s blackout; 2 and 4 peers) ever
desynced.
