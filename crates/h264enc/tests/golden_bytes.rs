//! Golden output hashes: speed work (the lane-parallel kernels, removed recomputation) must not change a
//! single output byte. The hashes were recorded with the scalar encoder before those changes; a change
//! that is meant to alter the output has to update them deliberately (and say why in the commit).

mod common;

use common::synth;
use filmcraft_h264enc::*;

/// FNV-1a 64 over every NAL unit except SEI (whose identification string carries the crate version).
fn stream_hash(packets: &[Packet]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for p in packets {
        let mut rest = &p.data[..];
        while rest.len() >= 4 {
            let n = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
            let nal = &rest[4..4 + n];
            if nal[0] & 0x1f != 6 {
                for &b in nal {
                    h = (h ^ b as u64).wrapping_mul(0x100_0000_01b3);
                }
            }
            rest = &rest[4 + n..];
        }
        assert!(rest.is_empty());
    }
    h
}

fn encode(w: usize, h: usize, frames: usize, setup: impl Fn(&mut EncoderConfig)) -> u64 {
    let mut c = EncoderConfig::new(w as u32, h as u32, 30, 1);
    c.format = PacketFormat::LengthPrefixed;
    setup(&mut c);
    let mut enc = Encoder::new(c).unwrap();
    let mut packets = Vec::new();
    for t in 0..frames {
        packets.extend(enc.encode(&synth(w, h, t).frame(), t as i64).unwrap());
    }
    packets.extend(enc.flush());
    stream_hash(&packets)
}

#[test]
fn encoder_output_is_unchanged() {
    let got = [
        // the export's settings: High, Balanced, 2 B-frames, VBR, many slices
        encode(640, 352, 10, |c| {
            c.rate = RateControl::Vbr { target_kbps: 3000, max_kbps: 4500 };
            c.slices = 6;
        }),
        encode(352, 288, 8, |c| {
            c.profile = Profile::Main;
            c.preset = Preset::Speed;
            c.bframes = 0;
            c.rate = RateControl::Crf(23.0);
        }),
        encode(320, 240, 8, |c| {
            c.preset = Preset::Quality;
            c.bframes = 3;
            c.rate = RateControl::Crf(18.0);
            c.slices = 2;
        }),
        encode(320, 240, 8, |c| {
            c.profile = Profile::Baseline;
            c.rate = RateControl::Cbr { kbps: 1500 };
        }),
        // odd size (cropping, partial macroblocks), low and high QP
        encode(162, 98, 6, |c| {
            c.bframes = 1;
            c.rate = RateControl::Qp(10);
        }),
        encode(162, 98, 6, |c| {
            c.preset = Preset::Quality;
            c.rate = RateControl::Qp(45);
        }),
    ];
    let want: [u64; 6] = [0xd913d97463c31940, 0x71279a5a10664008, 0xdfe5bf6c7edb4705, 0x5c352281c8b4952f, 0x2ee16937ffe11c6b, 0xa4756f33b291b4ae];
    for (i, g) in got.iter().enumerate() {
        println!("config {i}: {g:#018x}");
    }
    assert_eq!(got, want);
}
