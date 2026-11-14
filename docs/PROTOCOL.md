# ebobo network protocol (v2)

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
answered with PUNCH_ACK. This covers NAT rebinding and Wi-Fi → LTE. A relay-routed pair does not
roam. Direct-IP mode (token 0) never roams.

Return to direct (game only): a pair that fell back from a direct route to the relay during the
game probes the direct path again. Pairs relayed by decision (`relay_only`, `force_relay`, or
punching never worked before the game) never probe and stay on the relay; so does a pair that
fell back in the lobby. Probing starts 5 s after the fallback. Each further fallback of the same
pair doubles that wait (10 s, 20 s, 40 s), up to 60 s; a direct route that stayed in use for 30 s
resets it. While probing, both sides send a PUNCH once per second to the old route address, to the
peer's candidates and to the source of the peer's latest token-matched PUNCH (the peer may have
moved); every such PUNCH is answered with PUNCH_ACK. The decider (lower id) returns once it has
3 PUNCH_ACKs from one address, at least 0.5 s apart, within 4 s: it sends
`{"t":"route","to":<peer>,"mode":"direct","addr":"<peer address>"}` and switches. The other side
accepts `route` `direct` from the lower id only for a fallen-back pair, and uses the address of
its own PUNCH_ACKs, else the source of the peer's PUNCH, else the old route address. Both sides
start the route's silence clock fresh. Each side also mirrors every packet over the relay until a
non-punch packet arrives from the peer on the direct address (proof that the peer switched), so
nothing is lost during the switch. Candidates are not gathered again: probes use the candidates
from the last `cands` message.

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
{"t":"game","proto":2,"core":"0.1.0","name":"Ivan","rom":{"crc32":"1a2b3c4d","size":131088,"name":"Test Game (USA).nes"},"region":"ntsc","four_score":false}
```

Guest → host: `{"t":"rom","have":true,"name":"Sasha","proto":2,"ver":"0.1.0"}` (the guest
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
{"t":"start","slots":[0,1,2],"delay":2,"auto":true,"four_score":true}
```

`slots[i]` = peer id playing NES controller i+1. `delay` = GGRS input delay every peer starts
with, chosen by the host: its setting (1-6), or auto. Auto: L = ceil(R / 2 / frame_ms) is the
one-way latency in frames; delay = clamp(min(L, 3) + max(L - 9, 0), 1, 6). The first 3 frames of
latency are hidden by delay (no rollbacks), the next 6 are left to rollback, the rest is delay
again. R is the worst peer's planning RTT: SRTT + 2·RTTVAR once that peer has at least 8 samples,
otherwise SRTT·1.25 (RFC 6298's cold-start RTTVAR = R/2 would add a whole extra RTT). A peer with
no samples counts as 0 ms, and the start never waits for samples. frame_ms is 16.64 (NTSC) or 20
(PAL). `auto` = true when the host's input delay setting is Auto: then every peer adapts its own
delay during the game (section 5); false keeps `delay` fixed for everyone. Direct-IP mode uses
the same rule. `four_score` = true when more than 2 slots are used or the ROM header asks for it.
Each peer starts GGRS when it receives (guests) or sends (host) `start`.

`{"t":"leave"}` (any → all) and BYE packets end the lobby/game for that peer.

## 5. Game (GGRS)

GGRS is pinned to upstream revision `25bd998` (0.13.0 plus fixes; its input packets carry the
frame they are delta-encoded against, which 0.13.0's did not, so the two do not interoperate).
`ggrs::Config`: `Input = u16`, `State =
ebobo_core::Snapshot`, `Address = PeerId(u8)`. Players = `slots.len()`, local handle = own slot
index, every other slot is `PlayerType::Remote(PeerId)`. FPS = 60 (NTSC) / 50 (PAL). The
session takes the `Link` from `GameStart.link` (the lobby's live mesh, section 2), or
`Link::udp(socket, me, peers)` for fixed UDP routes without a server; every GGRS poll also
drives its signaling, route health, punching, relay mirroring and roaming. A BYE from a peer
disconnects its player.
Max prediction 10, desync detection every 60 frames (checksum = `Nes::state_hash` truncated to
u128), disconnect timeout 20 s, notify after 1.5 s. A frame whose combined input has any reset
bit runs `Nes::reset()` before `run_frame`.

Input word: bits 0-7 = NES pad held, bit 8 = reset request (one frame), bit 9 = turbo A held,
bit 10 = turbo B held. Turbo travels as a held bit; every peer expands it from the GGRS frame
number f being simulated: the turbo phase is on when `(f / 2) % 2 == 0` (2 frames on, 2 off).
A pad button is pressed when its bit is held, or when its turbo bit is set and the phase is on.
Disconnected players read as all released.

Prediction: a missing remote input is predicted as that player's last input without the reset
bit. Held and turbo bits repeat, so a held turbo button predicts correctly.

Smoothed RTT: each peer samples GGRS's ping to every remote player every 200 ms (none during the
first second, none for players that left) into the same RFC 6298 estimator the lobby uses
(SRTT, RTTVAR, warm after 8 samples). The in-game ping shown is the SRTT.

Adaptive input delay (only when `start.auto` is true): each peer changes only its own delay. The
target is the section 4 formula applied to the worst warm remote link. The delay rises by one
frame after the target was above it for 1 s without a break, and falls by one after it was below
for 10 s without a break. Two changes are at least 1 s apart, the delay stays within 1-6, and
nothing changes while no link is warm. A change is applied after a frame advanced, never on a
frame carrying a reset. GGRS sends copies of the last input to fill the gap on a rise and drops
the next inputs on a fall, so peers stay in sync without extra messages.

Tuning (measured with the network profiles in `crates/ebobo-net/src/session_tests.rs`, release
build): prediction 10 instead of 8 cuts stalls on a 150 ms one-way link from 14-33 % to 4-11 % of
frames; a 4 s blackout never disconnects and 20 s lets a phone in a tunnel come back. No profile
(mobile bursty loss, 150 ms far, 15/180 ms asymmetric, 4 s blackout; 2 and 4 peers) ever
desynced.
