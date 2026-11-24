//! A WebSocket echo endpoint for the fixture server (`/ws`): each text
//! frame `m` comes back as `echo:m`. RFC 6455, just enough for tests.

use std::io::{Read, Write};

use base64::Engine as _;
use tiny_http::{Header, Request, Response};

/// Answer a `/ws` upgrade and echo on its own thread.
pub fn accept(req: Request) {
    let key = req
        .headers()
        .iter()
        .find(|h| h.field.equiv("Sec-WebSocket-Key"))
        .map(|h| h.value.as_str().to_string())
        .unwrap_or_default();
    let accept = base64::engine::general_purpose::STANDARD.encode(sha1(
        format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes(),
    ));
    let response = Response::empty(101)
        .with_header(Header::from_bytes("Sec-WebSocket-Accept", accept).expect("header"));
    let mut stream = req.upgrade("websocket", response);
    std::thread::spawn(move || {
        while let Some(text) = read_text(&mut stream) {
            if write_text(&mut stream, &format!("echo:{text}")).is_err() {
                break;
            }
        }
    });
}

/// The next text frame's payload (client frames are masked); `None` on
/// close or error.
fn read_text(s: &mut impl Read) -> Option<String> {
    let mut head = [0u8; 2];
    s.read_exact(&mut head).ok()?;
    if head[0] & 0x0f == 0x8 {
        return None;
    }
    let mut len = u64::from(head[1] & 0x7f);
    if len == 126 {
        let mut b = [0u8; 2];
        s.read_exact(&mut b).ok()?;
        len = u64::from(u16::from_be_bytes(b));
    } else if len == 127 {
        let mut b = [0u8; 8];
        s.read_exact(&mut b).ok()?;
        len = u64::from_be_bytes(b);
    }
    let mut mask = [0u8; 4];
    if head[1] & 0x80 != 0 {
        s.read_exact(&mut mask).ok()?;
    }
    let mut payload = vec![0u8; usize::try_from(len).ok()?];
    s.read_exact(&mut payload).ok()?;
    for (i, b) in payload.iter_mut().enumerate() {
        *b ^= mask[i % 4];
    }
    String::from_utf8(payload).ok()
}

fn write_text(s: &mut impl Write, text: &str) -> std::io::Result<()> {
    let bytes = text.as_bytes();
    let mut frame = vec![0x81];
    if bytes.len() < 126 {
        frame.push(u8::try_from(bytes.len()).expect("< 126"));
    } else {
        frame.push(126);
        frame.extend_from_slice(
            &u16::try_from(bytes.len())
                .expect("test frames are small")
                .to_be_bytes(),
        );
    }
    frame.extend_from_slice(bytes);
    s.write_all(&frame)?;
    s.flush()
}

/// SHA-1, for the handshake's accept key only. Variable names follow
/// the FIPS 180-4 text.
#[allow(clippy::many_single_char_names)]
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 80];
        for (i, word) in chunk.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 20];
    for (i, x) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&x.to_be_bytes());
    }
    out
}

#[test]
fn sha1_matches_the_rfc_example() {
    // RFC 6455 section 1.3.
    let key = "dGhlIHNhbXBsZSBub25jZQ==258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
    assert_eq!(
        base64::engine::general_purpose::STANDARD.encode(sha1(key.as_bytes())),
        "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
    );
}
