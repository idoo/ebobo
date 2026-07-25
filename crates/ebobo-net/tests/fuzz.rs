//! Fuzz-style tests for the packet framing (seeded xorshift, no extra deps): random datagrams
//! never panic the parser, and every variant round-trips encode -> parse.

use ebobo_net::packet::{kind, Packet, MAGIC, ROM_CHUNK_DATA};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

const KINDS: [u8; 9] = [
    kind::PUNCH,
    kind::PUNCH_ACK,
    kind::KEEPALIVE,
    kind::CTRL,
    kind::CTRL_ACK,
    kind::ROM_CHUNK,
    kind::ROM_ACK,
    kind::GGRS,
    kind::BYE,
];

#[test]
fn parse_random_buffers_never_panics() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for i in 0..200_000 {
        let len = rng.below(1300);
        let mut buf = rng.bytes(len);
        if i % 4 != 0 && !buf.is_empty() {
            buf[0] = MAGIC;
            if buf.len() > 1 && i % 4 != 1 {
                buf[1] = KINDS[rng.below(KINDS.len())];
            }
        }
        if let Some(p) = Packet::parse(&buf) {
            // Whatever parses must re-encode to the same bytes.
            assert_eq!(p.to_vec(), buf, "{p:?}");
        }
    }
}

#[test]
fn every_variant_round_trips() {
    let mut rng = Rng(0x0123_4567_89AB_CDEF);
    for _ in 0..5_000 {
        let (jl, dl, gl) = (rng.below(1100), rng.below(ROM_CHUNK_DATA + 1), rng.below(1100));
        let json = rng.bytes(jl);
        let data = rng.bytes(dl);
        let ggrs = rng.bytes(gl);
        let (a, b, c) = (rng.next(), rng.next() as u8, rng.next() as u8);
        let packets = [
            Packet::Punch { token: a, from: b, to: c },
            Packet::PunchAck { token: a, from: b, to: c },
            Packet::Keepalive,
            Packet::Ctrl { seq: a as u32, json: &json },
            Packet::CtrlAck { seq: a as u32 },
            Packet::RomChunk { offset: a as u32, total: (a >> 32) as u32, data: &data },
            Packet::RomAck { next: a as u32 },
            Packet::Ggrs(&ggrs),
            Packet::Bye,
        ];
        for p in packets {
            let bytes = p.to_vec();
            assert_eq!(Packet::parse(&bytes), Some(p));
            // Every strict prefix must parse as nothing or as a different, shorter packet.
            for cut in 0..bytes.len().min(16) {
                if let Some(q) = Packet::parse(&bytes[..cut]) {
                    assert_eq!(q.to_vec(), &bytes[..cut]);
                }
            }
        }
    }
    // Oversized ROM chunk data is rejected rather than accepted.
    let big = vec![0u8; ROM_CHUNK_DATA + 1];
    let bytes = Packet::RomChunk { offset: 0, total: 0, data: &big }.to_vec();
    assert_eq!(Packet::parse(&bytes), None);
}
