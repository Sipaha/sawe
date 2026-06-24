//! App-level wire compression for JSON-RPC frames — the server-side twin of the
//! mobile `core/.../WireCompression.kt`. A compressed message travels as a
//! WebSocket BINARY frame with a 10-byte header that keeps it unambiguous from
//! the only other binary frames on this socket (upload chunks, whose first byte
//! is the `0x00` high byte of a small `u64 upload_id`, never the `0x73` magic):
//!
//! ```text
//! [0..4)  magic "spkz" (0x73 0x70 0x6B 0x7A)
//! [4]     format  (1 = zlib/DEFLATE)
//! [5]     dict id (0 = none, 1 = proto-v1 preset dictionary)
//! [6..10) u32 BE  decompressed length (bounds the inflate buffer + integrity)
//! [10..)  zlib stream (RFC 1950; FDICT set iff dict id != 0)
//! ```
//!
//! Standard RFC-1950 zlib (via flate2's pure-Rust `zlib-rs` backend), so it
//! round-trips byte-for-byte with the mobile `java.util.zip` codec.

#![allow(dead_code)]

use anyhow::{Result, anyhow, bail};
use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};

use crate::wire_dict::{WIRE_DICT_NONE, dictionary_for};

/// Handshake codec token negotiated in the `compress`/`welcome` frames.
pub(crate) const CODEC_DEFLATE: &str = "deflate";

const MAGIC: [u8; 4] = [0x73, 0x70, 0x6B, 0x7A];
const FORMAT_DEFLATE: u8 = 1;
pub(crate) const HEADER_BYTES: usize = 10;

/// Hard cap on a single decompressed message (DEFLATE-bomb guard).
pub(crate) const MAX_DECOMPRESSED_BYTES: usize = 32 * 1024 * 1024;

/// Below this raw size compression rarely beats the framing overhead.
pub(crate) const DEFAULT_COMPRESS_THRESHOLD_BYTES: usize = 180;

pub(crate) fn is_compressed(frame: &[u8]) -> bool {
    frame.len() >= HEADER_BYTES && frame[0..4] == MAGIC && frame[4] == FORMAT_DEFLATE
}

/// Frame `text` as a compressed binary message.
pub(crate) fn compress(text: &[u8], dict_id: u8) -> Result<Vec<u8>> {
    let mut c = Compress::new(Compression::best(), /* zlib_header = */ true);
    if dict_id != WIRE_DICT_NONE {
        if let Some(dict) = dictionary_for(dict_id) {
            c.set_dictionary(dict)
                .map_err(|e| anyhow!("set_dictionary: {e}"))?;
        }
    }
    let mut out = Vec::with_capacity(HEADER_BYTES + text.len() / 2 + 16);
    out.extend_from_slice(&MAGIC);
    out.push(FORMAT_DEFLATE);
    out.push(dict_id);
    out.extend_from_slice(&(text.len() as u32).to_be_bytes());

    let mut scratch = [0u8; 16384];
    let mut pos = 0usize;
    loop {
        let in_before = c.total_in();
        let out_before = c.total_out();
        let status = c
            .compress(&text[pos..], &mut scratch, FlushCompress::Finish)
            .map_err(|e| anyhow!("deflate: {e}"))?;
        pos += (c.total_in() - in_before) as usize;
        let produced = (c.total_out() - out_before) as usize;
        out.extend_from_slice(&scratch[..produced]);
        match status {
            Status::StreamEnd => break,
            Status::Ok | Status::BufError => {
                if produced == 0 && pos >= text.len() {
                    bail!("deflate stalled before stream end");
                }
            }
        }
    }
    Ok(out)
}

/// Compress only when it's worth it: `Some(frame)` when `text` is at least
/// `threshold` bytes AND the result is strictly smaller than the raw bytes,
/// else `None` (caller sends the original as a TEXT frame).
pub(crate) fn compress_if_worthwhile(
    text: &[u8],
    dict_id: u8,
    threshold: usize,
) -> Option<Vec<u8>> {
    if text.len() < threshold {
        return None;
    }
    match compress(text, dict_id) {
        Ok(frame) if frame.len() < text.len() => Some(frame),
        _ => None,
    }
}

/// Decode a compressed binary frame back to the original JSON bytes.
pub(crate) fn decompress(frame: &[u8]) -> Result<Vec<u8>> {
    if !is_compressed(frame) {
        bail!("not a compressed frame (bad magic/format)");
    }
    let dict_id = frame[5];
    let declared = u32::from_be_bytes([frame[6], frame[7], frame[8], frame[9]]) as usize;
    if declared > MAX_DECOMPRESSED_BYTES {
        bail!("declared length {declared} exceeds cap {MAX_DECOMPRESSED_BYTES}");
    }
    let payload = &frame[HEADER_BYTES..];
    let mut d = Decompress::new(/* zlib_header = */ true);
    // One extra slot so the final call has output room to read the end-of-stream
    // marker and report StreamEnd instead of stalling exactly at `declared`.
    let mut out = vec![0u8; declared + 1];
    let mut in_pos = 0usize;
    let mut out_pos = 0usize;
    loop {
        let in_before = d.total_in();
        let out_before = d.total_out();
        let res = d.decompress(
            &payload[in_pos..],
            &mut out[out_pos..],
            FlushDecompress::Finish,
        );
        let consumed = (d.total_in() - in_before) as usize;
        let produced = (d.total_out() - out_before) as usize;
        // Advance offsets by what was actually consumed/produced — even when the
        // call returns NeedsDictionary (it has already read the zlib header +
        // DICTID, so re-feeding from `in_pos` would corrupt the stream).
        in_pos += consumed;
        out_pos += produced;
        let status = match res {
            Ok(status) => status,
            Err(e) if e.needs_dictionary().is_some() => {
                let dict = dictionary_for(dict_id)
                    .ok_or_else(|| anyhow!("frame needs dictionary {dict_id}, none known"))?;
                d.set_dictionary(dict)
                    .map_err(|e| anyhow!("set_dictionary: {e}"))?;
                continue;
            }
            Err(e) => bail!("inflate failed: {e}"),
        };
        match status {
            Status::StreamEnd => break,
            Status::Ok | Status::BufError => {
                if out_pos > declared {
                    bail!("output exceeds declared length {declared}");
                }
                // No forward progress under a Finish flush → the frame is
                // truncated/corrupt (or claims fewer bytes than it encodes).
                if consumed == 0 && produced == 0 {
                    bail!("truncated frame: produced {out_pos} of declared {declared}");
                }
            }
        }
    }
    if out_pos != declared {
        bail!("length mismatch: produced {out_pos}, declared {declared}");
    }
    out.truncate(declared);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire_dict::{WIRE_DICT_PROTO_V1, WIRE_DICT_PROTO_V1_ADLER32, WIRE_DICT_PROTO_V1_BYTES};

    fn sample() -> String {
        let body = "lorem ipsum dolor sit amet ".repeat(40);
        format!(
            "{{\"jsonrpc\":\"2.0\",\"method\":\"remote/notification\",\"params\":{{\"kind\":\
             \"session_entry_appended\",\"session_id\":\"s-1\",\"entry\":{{\"index\":42,\
             \"role\":\"assistant\",\"text\":\"{body}\",\"tool_call\":null,\
             \"created_ms\":1700000000000}}}}}}"
        )
    }

    #[test]
    fn round_trips_with_dictionary() {
        let s = sample();
        let frame = compress(s.as_bytes(), WIRE_DICT_PROTO_V1).unwrap();
        assert!(is_compressed(&frame));
        assert_eq!(decompress(&frame).unwrap(), s.as_bytes());
    }

    #[test]
    fn round_trips_without_dictionary() {
        let s = sample();
        let frame = compress(s.as_bytes(), WIRE_DICT_NONE).unwrap();
        assert_eq!(decompress(&frame).unwrap(), s.as_bytes());
    }

    #[test]
    fn compresses_repetitive_payload_under_half() {
        let s = sample();
        let frame = compress(s.as_bytes(), WIRE_DICT_PROTO_V1).unwrap();
        assert!(frame.len() < s.len() / 2, "got {} of {}", frame.len(), s.len());
    }

    #[test]
    fn unicode_round_trips() {
        let s = "{\"text\":\"Привет, мир — 你好 🌍 🚀\"}";
        let frame = compress(s.as_bytes(), WIRE_DICT_PROTO_V1).unwrap();
        assert_eq!(decompress(&frame).unwrap(), s.as_bytes());
    }

    #[test]
    fn is_compressed_false_for_text_and_upload_frame() {
        assert!(!is_compressed(sample().as_bytes()));
        // Upload chunk: first 8 bytes are a small u64 upload_id BE → byte[0]==0.
        let mut uploadish = vec![0u8; 32];
        uploadish[7] = 7;
        assert!(!is_compressed(&uploadish));
    }

    #[test]
    fn decompress_rejects_bad_magic() {
        let mut frame = compress(sample().as_bytes(), WIRE_DICT_NONE).unwrap();
        frame[0] = 0x00;
        assert!(decompress(&frame).is_err());
    }

    #[test]
    fn decompress_rejects_oversized_declared_length() {
        let mut frame = compress(sample().as_bytes(), WIRE_DICT_NONE).unwrap();
        frame[6..10].copy_from_slice(&0x7FFF_FFFFu32.to_be_bytes());
        assert!(decompress(&frame).is_err());
    }

    #[test]
    fn decompress_rejects_truncated() {
        let frame = compress(sample().as_bytes(), WIRE_DICT_NONE).unwrap();
        let truncated = &frame[..frame.len() - 5];
        assert!(decompress(truncated).is_err());
    }

    #[test]
    fn compress_if_worthwhile_below_threshold_is_none() {
        assert!(compress_if_worthwhile(b"hi", WIRE_DICT_PROTO_V1, 64).is_none());
    }

    #[test]
    fn compress_if_worthwhile_round_trips_large() {
        let s = sample();
        let frame = compress_if_worthwhile(s.as_bytes(), WIRE_DICT_PROTO_V1, 64).unwrap();
        assert_eq!(decompress(&frame).unwrap(), s.as_bytes());
    }

    #[test]
    fn decodes_a_mobile_frame() {
        // Golden frame produced by the Kotlin codec (WireCompressionInteropTest):
        // `compress("{...idle...}", WIRE_DICT_PROTO_V1)`. Proves the mobile→server
        // direction round-trips byte-for-byte.
        let golden_text =
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"sessions\":[],\"total_count\":0,\
             \"state\":{\"kind\":\"idle\"}}}";
        let frame = hex_to_bytes(
            "73706b7a01010000005978f9262169dcc36e8ba10e926ea4a08ad5410d2c031d8c60027949a9b6b61600fcda1d2f",
        );
        assert!(is_compressed(&frame));
        assert_eq!(decompress(&frame).unwrap(), golden_text.as_bytes());
    }

    fn hex_to_bytes(hex: &str) -> Vec<u8> {
        (0..hex.len() / 2)
            .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn dictionary_adler32_matches_cross_language_pin() {
        // flate2's set_dictionary returns the Adler-32 zlib stamps as the DICTID.
        let mut c = flate2::Compress::new(flate2::Compression::best(), true);
        let adler = c.set_dictionary(WIRE_DICT_PROTO_V1_BYTES).unwrap();
        assert_eq!(adler, WIRE_DICT_PROTO_V1_ADLER32);
    }
}
