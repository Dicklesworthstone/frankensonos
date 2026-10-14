//! HTTP/2 (RFC 7540) framing + HPACK (RFC 7541).
//!
//! Scope (bd-2c9t):
//! - Implement enough of HTTP/2 to accept cleartext prior-knowledge connections (h2c)
//! - Provide a correct HPACK decoder (including Huffman) for request headers
//!
//! This module intentionally avoids Tokio/Hyper and uses only asupersync for async I/O.

use asupersync::io::{AsyncRead, AsyncWrite, ReadBuf};
use asupersync::net::TcpStream;
use std::collections::VecDeque;
use std::future::poll_fn;
use std::io;
use std::pin::Pin;
use std::sync::OnceLock;
use std::task::Poll;

/// HTTP/2 connection preface for prior-knowledge cleartext.
pub const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// HTTP/2 frame type (RFC 7540).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameType {
    Data = 0x0,
    Headers = 0x1,
    Priority = 0x2,
    RstStream = 0x3,
    Settings = 0x4,
    PushPromise = 0x5,
    Ping = 0x6,
    Goaway = 0x7,
    WindowUpdate = 0x8,
    Continuation = 0x9,
    Unknown = 0xFF,
}

impl FrameType {
    #[must_use]
    pub fn from_u8(v: u8) -> Self {
        match v {
            0x0 => Self::Data,
            0x1 => Self::Headers,
            0x2 => Self::Priority,
            0x3 => Self::RstStream,
            0x4 => Self::Settings,
            0x5 => Self::PushPromise,
            0x6 => Self::Ping,
            0x7 => Self::Goaway,
            0x8 => Self::WindowUpdate,
            0x9 => Self::Continuation,
            _ => Self::Unknown,
        }
    }
}

/// A parsed HTTP/2 frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub length: u32, // 24-bit
    pub frame_type: u8,
    pub flags: u8,
    pub stream_id: u32, // 31-bit
}

impl FrameHeader {
    pub const LEN: usize = 9;

    #[must_use]
    pub fn frame_type(&self) -> FrameType {
        FrameType::from_u8(self.frame_type)
    }

    #[must_use]
    pub fn is_stream_zero(&self) -> bool {
        self.stream_id == 0
    }
}

/// A full HTTP/2 frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub header: FrameHeader,
    pub payload: Vec<u8>,
}

#[derive(Debug)]
pub enum Http2Error {
    Io(io::Error),
    Protocol(&'static str),
    Hpack(HpackError),
}

impl std::fmt::Display for Http2Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "http2 I/O error: {e}"),
            Self::Protocol(m) => write!(f, "http2 protocol error: {m}"),
            Self::Hpack(e) => write!(f, "hpack error: {e}"),
        }
    }
}

impl std::error::Error for Http2Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Hpack(e) => Some(e),
            Self::Protocol(_) => None,
        }
    }
}

impl From<io::Error> for Http2Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<HpackError> for Http2Error {
    fn from(e: HpackError) -> Self {
        Self::Hpack(e)
    }
}

/// A simple framed HTTP/2 I/O wrapper.
#[derive(Debug)]
pub struct FramedH2 {
    stream: TcpStream,
    rx: Vec<u8>,
}

impl FramedH2 {
    #[must_use]
    pub fn new(stream: TcpStream, buffered: Vec<u8>) -> Self {
        Self {
            stream,
            rx: buffered,
        }
    }

    /// Read the next HTTP/2 frame.
    pub async fn read_frame(&mut self, max_frame_size: u32) -> Result<Frame, Http2Error> {
        let header_bytes = self.read_exact(FrameHeader::LEN).await?;
        let length = ((u32::from(header_bytes[0])) << 16)
            | ((u32::from(header_bytes[1])) << 8)
            | u32::from(header_bytes[2]);
        let frame_type = header_bytes[3];
        let flags = header_bytes[4];
        let stream_id = u32::from_be_bytes([
            header_bytes[5],
            header_bytes[6],
            header_bytes[7],
            header_bytes[8],
        ]) & 0x7FFF_FFFF;

        if length > max_frame_size {
            return Err(Http2Error::Protocol("frame length exceeds max_frame_size"));
        }

        let payload = self.read_exact(length as usize).await?;
        Ok(Frame {
            header: FrameHeader {
                length,
                frame_type,
                flags,
                stream_id,
            },
            payload,
        })
    }

    /// Write an HTTP/2 frame.
    pub async fn write_frame(
        &mut self,
        frame_type: FrameType,
        flags: u8,
        stream_id: u32,
        payload: &[u8],
    ) -> Result<(), Http2Error> {
        if stream_id & 0x8000_0000 != 0 {
            return Err(Http2Error::Protocol("reserved bit set in stream_id"));
        }
        let len = u32::try_from(payload.len())
            .map_err(|_| Http2Error::Protocol("payload length too large"))?;
        if len > 0x00FF_FFFF {
            return Err(Http2Error::Protocol("payload length exceeds 24-bit limit"));
        }

        let mut out = Vec::with_capacity(FrameHeader::LEN + payload.len());
        out.push(((len >> 16) & 0xff) as u8);
        out.push(((len >> 8) & 0xff) as u8);
        out.push((len & 0xff) as u8);
        out.push(frame_type as u8);
        out.push(flags);
        out.extend_from_slice(&(stream_id & 0x7FFF_FFFF).to_be_bytes());
        out.extend_from_slice(payload);

        write_all(&mut self.stream, &out).await?;
        flush(&mut self.stream).await?;
        Ok(())
    }

    async fn read_exact(&mut self, n: usize) -> io::Result<Vec<u8>> {
        while self.rx.len() < n {
            let mut tmp = vec![0u8; 8192];
            let read = read_once(&mut self.stream, &mut tmp).await?;
            if read == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "EOF"));
            }
            self.rx.extend_from_slice(&tmp[..read]);
        }
        Ok(self.rx.drain(..n).collect())
    }
}

async fn read_once(stream: &mut TcpStream, buffer: &mut [u8]) -> io::Result<usize> {
    poll_fn(|cx| {
        let mut read_buf = ReadBuf::new(buffer);
        match Pin::new(&mut *stream).poll_read(cx, &mut read_buf) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(read_buf.filled().len())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    })
    .await
}

async fn write_all(stream: &mut TcpStream, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        let n = poll_fn(|cx| Pin::new(&mut *stream).poll_write(cx, buf)).await?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "write zero"));
        }
        buf = &buf[n..];
    }
    Ok(())
}

async fn flush(stream: &mut TcpStream) -> io::Result<()> {
    poll_fn(|cx| Pin::new(&mut *stream).poll_flush(cx)).await
}

// =============================================================================
// HPACK decoder (RFC 7541)
// =============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HpackError {
    InvalidInteger,
    InvalidString,
    InvalidIndex,
    InvalidHuffman,
    DynamicTableSizeUpdateOutOfRange,
    HeaderListTooLarge,
}

impl std::fmt::Display for HpackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for HpackError {}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HeaderField {
    name: Vec<u8>,
    value: Vec<u8>,
    size: usize,
}

impl HeaderField {
    fn new(name: Vec<u8>, value: Vec<u8>) -> Self {
        let size = 32 + name.len() + value.len();
        Self { name, value, size }
    }
}

/// HPACK decoder with a dynamic table.
#[derive(Debug)]
pub struct HpackDecoder {
    dynamic: VecDeque<HeaderField>,
    dynamic_size: usize,
    dynamic_max_size: usize,
    max_header_list_size: usize,
}

/// Decoded HPACK headers (name, value) as raw bytes.
pub type HeaderList = Vec<(Vec<u8>, Vec<u8>)>;

impl Default for HpackDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl HpackDecoder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            dynamic: VecDeque::new(),
            dynamic_size: 0,
            dynamic_max_size: 4096,
            max_header_list_size: 64 * 1024,
        }
    }

    pub fn set_dynamic_table_max_size(&mut self, n: usize) {
        self.dynamic_max_size = n;
        self.evict_to_max();
    }

    pub fn set_max_header_list_size(&mut self, n: usize) {
        self.max_header_list_size = n;
    }

    pub fn decode(&mut self, block: &[u8]) -> Result<HeaderList, HpackError> {
        let mut out: HeaderList = Vec::new();
        let mut i = 0usize;

        while i < block.len() {
            let b = block[i];

            if (b & 0x80) != 0 {
                // Indexed Header Field Representation (1xxxxxxx)
                let (index, used) = decode_integer(&block[i..], 7)?;
                i += used;
                let (name, value) = self.get_indexed(index)?;
                out.push((name, value));
                continue;
            }

            if (b & 0xC0) == 0x40 {
                // Literal Header Field with Incremental Indexing (01xxxxxx)
                let (name, value, used) = self.decode_literal(&block[i..], 6)?;
                i += used;
                self.insert_dynamic(name.clone(), value.clone());
                out.push((name, value));
                continue;
            }

            if (b & 0xE0) == 0x20 {
                // Dynamic Table Size Update (001xxxxx)
                let (new_size, used) = decode_integer(&block[i..], 5)?;
                i += used;
                if new_size > self.dynamic_max_size {
                    return Err(HpackError::DynamicTableSizeUpdateOutOfRange);
                }
                self.set_dynamic_table_max_size(new_size);
                continue;
            }

            // Literal Header Field without Indexing / Never Indexed.
            // 0000xxxx and 0001xxxx share the same literal payload shape.
            let (name, value, used) = self.decode_literal(&block[i..], 4)?;
            i += used;
            out.push((name, value));
        }

        let total_list_bytes: usize = out.iter().map(|(n, v)| n.len() + v.len() + 32).sum();
        if total_list_bytes > self.max_header_list_size {
            return Err(HpackError::HeaderListTooLarge);
        }

        Ok(out)
    }

    fn decode_literal(
        &mut self,
        buf: &[u8],
        name_prefix_bits: u8,
    ) -> Result<(Vec<u8>, Vec<u8>, usize), HpackError> {
        // Name: either indexed (prefix integer) or literal string (0 + string)
        let first = buf[0];
        let name_index_prefix_mask = (1u8 << name_prefix_bits) - 1;
        let name_index = usize::from(first & name_index_prefix_mask);

        let mut used = 0usize;
        let name = if name_index == 0 {
            used += 1;
            let (name_bytes, n_used) = decode_string(&buf[used..])?;
            used += n_used;
            name_bytes
        } else {
            let (index, n_used) = decode_integer(buf, name_prefix_bits)?;
            used += n_used;
            let (name, _value) = self.get_indexed(index)?;
            name
        };

        let (value, v_used) = decode_string(&buf[used..])?;
        used += v_used;
        Ok((name, value, used))
    }

    fn get_indexed(&self, index: usize) -> Result<(Vec<u8>, Vec<u8>), HpackError> {
        if index == 0 {
            return Err(HpackError::InvalidIndex);
        }
        let static_len = STATIC_TABLE.len();
        if index <= static_len {
            let (n, v) = STATIC_TABLE[index - 1];
            return Ok((n.to_vec(), v.to_vec()));
        }
        let dyn_index = index - static_len - 1;
        let field = self
            .dynamic
            .get(dyn_index)
            .ok_or(HpackError::InvalidIndex)?;
        Ok((field.name.clone(), field.value.clone()))
    }

    fn insert_dynamic(&mut self, name: Vec<u8>, value: Vec<u8>) {
        let field = HeaderField::new(name, value);
        if field.size > self.dynamic_max_size {
            self.dynamic.clear();
            self.dynamic_size = 0;
            return;
        }
        self.dynamic.push_front(field);
        self.dynamic_size = self.dynamic.iter().map(|f| f.size).sum();
        self.evict_to_max();
    }

    fn evict_to_max(&mut self) {
        while self.dynamic_size > self.dynamic_max_size {
            let Some(back) = self.dynamic.pop_back() else {
                self.dynamic_size = 0;
                break;
            };
            self.dynamic_size = self.dynamic_size.saturating_sub(back.size);
        }
    }
}

fn decode_integer(buf: &[u8], prefix_bits: u8) -> Result<(usize, usize), HpackError> {
    if buf.is_empty() || prefix_bits == 0 || prefix_bits > 8 {
        return Err(HpackError::InvalidInteger);
    }
    let prefix_max = (1usize << prefix_bits) - 1;
    let mut value = usize::from(buf[0] & (prefix_max as u8));
    if value < prefix_max {
        return Ok((value, 1));
    }
    let mut m = 0usize;
    let mut idx = 1usize;
    loop {
        let b = *buf.get(idx).ok_or(HpackError::InvalidInteger)?;
        idx += 1;
        value = value
            .checked_add((usize::from(b & 0x7f)) << m)
            .ok_or(HpackError::InvalidInteger)?;
        if (b & 0x80) == 0 {
            break;
        }
        m = m.checked_add(7).ok_or(HpackError::InvalidInteger)?;
        if m > 63 {
            return Err(HpackError::InvalidInteger);
        }
    }
    Ok((value, idx))
}

fn decode_string(buf: &[u8]) -> Result<(Vec<u8>, usize), HpackError> {
    if buf.is_empty() {
        return Err(HpackError::InvalidString);
    }
    let huffman = (buf[0] & 0x80) != 0;
    let (len, used) = decode_integer(buf, 7)?;
    let start = used;
    let end = start.checked_add(len).ok_or(HpackError::InvalidString)?;
    let s = buf.get(start..end).ok_or(HpackError::InvalidString)?;
    if huffman {
        let decoded = huffman_decode(s)?;
        Ok((decoded, end))
    } else {
        Ok((s.to_vec(), end))
    }
}

// =============================================================================
// HPACK encoder (minimal, for HTTP/2 responses)
// =============================================================================

fn encode_integer(out: &mut Vec<u8>, first: u8, prefix_bits: u8, mut value: usize) {
    let prefix_max = (1usize << prefix_bits) - 1;
    if value < prefix_max {
        out.push(first | (value as u8));
        return;
    }

    out.push(first | (prefix_max as u8));
    value -= prefix_max;
    while value >= 128 {
        out.push(((value & 0x7f) as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn encode_string(out: &mut Vec<u8>, bytes: &[u8]) {
    // Huffman bit = 0 (no huffman); length uses a 7-bit prefixed integer.
    encode_integer(out, 0x00, 7, bytes.len());
    out.extend_from_slice(bytes);
}

/// Encode a literal header field without indexing (RFC 7541).
///
/// This is intentionally minimal:
/// - never uses huffman
/// - never indexes into dynamic table
/// - always encodes the name as a literal (name-index = 0)
pub fn hpack_encode_literal_without_indexing(out: &mut Vec<u8>, name: &[u8], value: &[u8]) {
    // Literal Header Field without Indexing:
    // 0000xxxx where xxxx is the name index (4-bit prefix integer).
    // We always use name-index = 0 (literal name follows).
    encode_integer(out, 0x00, 4, 0);
    encode_string(out, name);
    encode_string(out, value);
}

// Static table: RFC 7541 Appendix A.
// Kept as bytes to avoid UTF-8 assumptions (header fields are bytes in HTTP/2).
const STATIC_TABLE: [(&[u8], &[u8]); 61] = [
    (b":authority", b""),
    (b":method", b"GET"),
    (b":method", b"POST"),
    (b":path", b"/"),
    (b":path", b"/index.html"),
    (b":scheme", b"http"),
    (b":scheme", b"https"),
    (b":status", b"200"),
    (b":status", b"204"),
    (b":status", b"206"),
    (b":status", b"304"),
    (b":status", b"400"),
    (b":status", b"404"),
    (b":status", b"500"),
    (b"accept-charset", b""),
    (b"accept-encoding", b"gzip, deflate"),
    (b"accept-language", b""),
    (b"accept-ranges", b""),
    (b"accept", b""),
    (b"access-control-allow-origin", b""),
    (b"age", b""),
    (b"allow", b""),
    (b"authorization", b""),
    (b"cache-control", b""),
    (b"content-disposition", b""),
    (b"content-encoding", b""),
    (b"content-language", b""),
    (b"content-length", b""),
    (b"content-location", b""),
    (b"content-range", b""),
    (b"content-type", b""),
    (b"cookie", b""),
    (b"date", b""),
    (b"etag", b""),
    (b"expect", b""),
    (b"expires", b""),
    (b"from", b""),
    (b"host", b""),
    (b"if-match", b""),
    (b"if-modified-since", b""),
    (b"if-none-match", b""),
    (b"if-range", b""),
    (b"if-unmodified-since", b""),
    (b"last-modified", b""),
    (b"link", b""),
    (b"location", b""),
    (b"max-forwards", b""),
    (b"proxy-authenticate", b""),
    (b"proxy-authorization", b""),
    (b"range", b""),
    (b"referer", b""),
    (b"refresh", b""),
    (b"retry-after", b""),
    (b"server", b""),
    (b"set-cookie", b""),
    (b"strict-transport-security", b""),
    (b"transfer-encoding", b""),
    (b"user-agent", b""),
    (b"vary", b""),
    (b"via", b""),
    (b"www-authenticate", b""),
];

#[derive(Debug, Clone, Copy)]
struct HuffmanNode {
    left: Option<usize>,
    right: Option<usize>,
    sym: Option<u16>,
}

fn huffman_tree() -> &'static Vec<HuffmanNode> {
    static TREE: OnceLock<Vec<HuffmanNode>> = OnceLock::new();
    TREE.get_or_init(|| {
        let mut nodes = vec![HuffmanNode {
            left: None,
            right: None,
            sym: None,
        }];

        for (sym, (&code, &bits)) in HUFFMAN_CODES.iter().zip(HUFFMAN_BITS.iter()).enumerate() {
            let mut cur = 0usize;
            for bit_index in (0..bits).rev() {
                let bit = (code >> bit_index) & 1;
                let next_idx = if bit == 0 {
                    nodes[cur].left
                } else {
                    nodes[cur].right
                };

                cur = if let Some(idx) = next_idx {
                    idx
                } else {
                    let idx = nodes.len();
                    nodes.push(HuffmanNode {
                        left: None,
                        right: None,
                        sym: None,
                    });
                    if bit == 0 {
                        nodes[cur].left = Some(idx);
                    } else {
                        nodes[cur].right = Some(idx);
                    }
                    idx
                };
            }
            nodes[cur].sym = Some(u16::try_from(sym).unwrap_or(256));
        }

        nodes
    })
}

fn eos_prefix_nodes() -> &'static Vec<bool> {
    static NODES: OnceLock<Vec<bool>> = OnceLock::new();
    NODES.get_or_init(|| {
        let tree = huffman_tree();
        let mut is_prefix = vec![false; tree.len()];
        let eos_code = HUFFMAN_CODES[256];
        let eos_bits = HUFFMAN_BITS[256];

        let mut cur = 0usize;
        is_prefix[cur] = true;
        for bit_index in (0..eos_bits).rev() {
            let bit = (eos_code >> bit_index) & 1;
            cur = if bit == 0 {
                tree[cur].left.expect("eos left")
            } else {
                tree[cur].right.expect("eos right")
            };
            if cur >= is_prefix.len() {
                break;
            }
            is_prefix[cur] = true;
        }
        is_prefix
    })
}

fn huffman_decode(bytes: &[u8]) -> Result<Vec<u8>, HpackError> {
    let tree = huffman_tree();
    let eos_prefix = eos_prefix_nodes();

    let mut out = Vec::with_capacity(bytes.len());
    let mut cur = 0usize;

    for &byte in bytes {
        for bit_shift in (0..8).rev() {
            let bit = (byte >> bit_shift) & 1;
            cur = if bit == 0 {
                tree[cur].left.ok_or(HpackError::InvalidHuffman)?
            } else {
                tree[cur].right.ok_or(HpackError::InvalidHuffman)?
            };
            if let Some(sym) = tree[cur].sym {
                if sym == 256 {
                    // EOS must not appear in the decoded stream.
                    return Err(HpackError::InvalidHuffman);
                }
                out.push(u8::try_from(sym).map_err(|_| HpackError::InvalidHuffman)?);
                cur = 0;
            }
        }
    }

    // Validate padding: the terminal state must be a prefix of EOS.
    if cur != 0 && !eos_prefix.get(cur).copied().unwrap_or(false) {
        return Err(HpackError::InvalidHuffman);
    }

    Ok(out)
}

// Huffman table: RFC 7541 Appendix B.
// code (MSB-first) + bit-length for symbols 0..=256 (EOS).
//
// This is large but stable; keep it as constants so the decoder stays dependency-free.
#[rustfmt::skip]
#[allow(clippy::unreadable_literal)]
const HUFFMAN_CODES: [u32; 257] = [
    0x1ff8,0x7fffd8,0xfffffe2,0xfffffe3,0xfffffe4,0xfffffe5,0xfffffe6,0xfffffe7,
    0xfffffe8,0xffffea,0x3ffffffc,0xfffffe9,0xfffffea,0x3ffffffd,0xfffffeb,0xfffffec,
    0xfffffed,0xfffffee,0xfffffef,0xffffff0,0xffffff1,0xffffff2,0x3ffffffe,0xffffff3,
    0xffffff4,0xffffff5,0xffffff6,0xffffff7,0xffffff8,0xffffff9,0xffffffa,0xffffffb,
    0x14,0x3f8,0x3f9,0xffa,0x1ff9,0x15,0xf8,0x7fa,0x3fa,0x3fb,0xf9,0x7fb,0xfa,
    0x16,0x17,0x18,0x0,0x1,0x2,0x19,0x1a,0x1b,0x1c,0x1d,0x1e,0x1f,0x5c,0xfb,
    0x7ffc,0x20,0xffb,0x3fc,0x1ffa,0x21,0x5d,0x5e,0x5f,0x60,0x61,0x62,0x63,
    0x64,0x65,0x66,0x67,0x68,0x69,0x6a,0x6b,0x6c,0x6d,0x6e,0x6f,0x70,0x71,
    0x72,0xfc,0x73,0xfd,0x1ffb,0x7fff0,0x1ffc,0x3ffc,0x22,0x7ffd,0x3,0x23,0x4,
    0x24,0x5,0x25,0x26,0x27,0x6,0x74,0x75,0x28,0x29,0x2a,0x7,0x2b,0x76,0x2c,
    0x8,0x9,0x2d,0x77,0x78,0x79,0x7a,0x7b,0x7ffe,0x7fc,0x3ffd,0x1ffd,0xffffffc,
    0xfffe6,0x3fffd2,0xfffe7,0xfffe8,0x3fffd3,0x3fffd4,0x3fffd5,0x7fffd9,0x3fffd6,
    0x7fffda,0x7fffdb,0x7fffdc,0x7fffdd,0x7fffde,0xffffeb,0x7fffdf,0xffffec,0xffffed,
    0x3fffd7,0x7fffe0,0xffffee,0x7fffe1,0x7fffe2,0x7fffe3,0x7fffe4,0x1fffdc,0x3fffd8,
    0x7fffe5,0x3fffd9,0x7fffe6,0x7fffe7,0xffffef,0x3fffda,0x1fffdd,0xfffe9,0x3fffdb,
    0x3fffdc,0x7fffe8,0x7fffe9,0x1fffde,0x7fffea,0x3fffdd,0x3fffde,0xfffff0,0x1fffdf,
    0x3fffdf,0x7fffeb,0x7fffec,0x1fffe0,0x1fffe1,0x3fffe0,0x1fffe2,0x7fffed,0x3fffe1,
    0x7fffee,0x7fffef,0xfffea,0x3fffe2,0x3fffe3,0x3fffe4,0x7ffff0,0x3fffe5,0x3fffe6,
    0x7ffff1,0x3ffffe0,0x3ffffe1,0xfffeb,0x7fff1,0x3fffe7,0x7ffff2,0x3fffe8,0x1ffffec,
    0x3ffffe2,0x3ffffe3,0x3ffffe4,0x7ffffde,0x7ffffdf,0x3ffffe5,0xfffff1,0x1ffffed,
    0x7fff2,0x1fffe3,0x3ffffe6,0x7ffffe0,0x7ffffe1,0x3ffffe7,0x7ffffe2,0xfffff2,
    0x1fffe4,0x1fffe5,0x3ffffe8,0x3ffffe9,0xffffffd,0x7ffffe3,0x7ffffe4,0x7ffffe5,
    0xfffec,0xfffff3,0xfffed,0x1fffe6,0x3fffe9,0x1fffe7,0x1fffe8,0x7ffff3,0x3fffea,
    0x3fffeb,0x1ffffee,0x1ffffef,0xfffff4,0xfffff5,0x3ffffea,0x7ffff4,0x3ffffeb,
    0x7ffffe6,0x3ffffec,0x3ffffed,0x7ffffe7,0x7ffffe8,0x7ffffe9,0x7ffffea,0x7ffffeb,
    0xffffffe,0x7ffffec,0x7ffffed,0x7ffffee,0x7ffffef,0x7fffff0,0x3ffffee,0x3fffffff,
];

#[rustfmt::skip]
const HUFFMAN_BITS: [u8; 257] = [
    13,23,28,28,28,28,28,28,28,24,30,28,28,30,28,28,
    28,28,28,28,28,28,30,28,28,28,28,28,28,28,28,28,
    6,10,10,12,13,6,8,11,10,10,8,11,8,6,6,6,
    5,5,5,6,6,6,6,6,6,6,7,8,15,6,12,10,
    13,6,7,7,7,7,7,7,7,7,7,7,7,7,7,7,
    7,7,7,7,7,7,7,7,8,7,8,13,19,13,14,6,
    15,5,6,5,6,5,6,6,6,5,7,7,6,6,6,5,
    6,7,6,5,5,6,7,7,7,7,7,15,11,14,13,28,
    20,22,20,20,22,22,22,23,22,23,23,23,23,23,24,23,
    24,24,22,23,24,23,23,23,23,21,22,23,22,23,23,24,
    22,21,20,22,22,23,23,21,23,22,22,24,21,22,23,23,
    21,21,22,21,23,22,23,23,20,22,22,22,23,22,22,23,
    26,26,20,19,22,23,22,25,26,26,26,27,27,26,24,25,
    19,21,26,27,27,26,27,24,21,21,26,26,28,27,27,27,
    20,24,20,21,22,21,21,23,22,22,25,25,24,24,26,23,
    26,27,26,26,27,27,27,27,27,28,27,27,27,27,27,26,
    30,
];

// =============================================================================
// HTTP/2 Flow Control (RFC 7540 §6.9)
// =============================================================================

/// RFC 7540 default initial window size (65,535 bytes).
pub const DEFAULT_INITIAL_WINDOW_SIZE: u32 = 65_535;

/// When the consumed (unreported) bytes exceed this fraction of the initial
/// window size, a WINDOW_UPDATE is sent. Using half the window keeps the
/// sender from stalling while avoiding excessive WINDOW_UPDATE traffic.
const WINDOW_UPDATE_THRESHOLD_DIVISOR: u32 = 2;

/// Tracks HTTP/2 flow-control receive windows at connection and stream level.
///
/// The receiver (server) decrements windows when DATA frames arrive and sends
/// WINDOW_UPDATE frames when enough data has been consumed to keep the sender
/// from stalling.
#[derive(Debug)]
pub struct H2FlowControl {
    /// Connection-level receive window remaining.
    conn_window: i64,
    /// How many bytes have been consumed at connection level since the last
    /// WINDOW_UPDATE was sent.
    conn_consumed: u32,
    /// The initial window size advertised (or default). Used to compute the
    /// threshold at which a WINDOW_UPDATE should be emitted.
    initial_window_size: u32,
    /// Connection-level send window remaining (peer's receive window).
    send_conn_window: i64,
    /// Per-stream initial send window (from peer's SETTINGS_INITIAL_WINDOW_SIZE).
    peer_initial_window_size: u32,
}

impl H2FlowControl {
    /// Create a new flow-control tracker with the RFC default initial window.
    #[must_use]
    pub fn new() -> Self {
        Self {
            conn_window: i64::from(DEFAULT_INITIAL_WINDOW_SIZE),
            conn_consumed: 0,
            initial_window_size: DEFAULT_INITIAL_WINDOW_SIZE,
            send_conn_window: i64::from(DEFAULT_INITIAL_WINDOW_SIZE),
            peer_initial_window_size: DEFAULT_INITIAL_WINDOW_SIZE,
        }
    }

    /// Update the initial window size (from SETTINGS_INITIAL_WINDOW_SIZE).
    /// This only affects future streams; the connection-level window is
    /// independent of this setting per RFC 7540 §6.9.2.
    pub fn set_initial_window_size(&mut self, size: u32) {
        self.initial_window_size = size;
    }

    /// Record that `n` bytes of DATA payload were received on the connection.
    /// Returns the connection-level WINDOW_UPDATE increment to send, or 0 if
    /// no update is needed yet.
    pub fn data_received_connection(&mut self, n: u32) -> u32 {
        self.conn_window -= i64::from(n);
        self.conn_consumed += n;

        let threshold = self.initial_window_size / WINDOW_UPDATE_THRESHOLD_DIVISOR;
        if self.conn_consumed >= threshold {
            let increment = self.conn_consumed;
            self.conn_window += i64::from(increment);
            self.conn_consumed = 0;
            increment
        } else {
            0
        }
    }

    /// Compute a stream-level WINDOW_UPDATE increment after `total_received`
    /// bytes have been received for a stream. Returns the increment to send,
    /// or 0 if no update is needed.
    ///
    /// For simplicity, the server sends a stream-level WINDOW_UPDATE when the
    /// consumed bytes exceed the threshold. Since streams are short-lived
    /// request bodies, we track this per-call rather than storing per-stream
    /// state.
    pub fn stream_window_update(&self, total_received: u32) -> u32 {
        let threshold = self.initial_window_size / WINDOW_UPDATE_THRESHOLD_DIVISOR;
        if total_received >= threshold {
            total_received
        } else {
            0
        }
    }

    /// The initial window size for new streams.
    #[must_use]
    pub fn initial_window_size(&self) -> u32 {
        self.initial_window_size
    }

    // --- Send-side flow control ---

    /// Set the peer's initial window size (from peer's SETTINGS_INITIAL_WINDOW_SIZE).
    /// This determines the send window for new streams.
    pub fn set_peer_initial_window_size(&mut self, size: u32) {
        self.peer_initial_window_size = size;
    }

    /// The peer's initial window size for new streams (send window).
    #[must_use]
    pub fn peer_initial_window_size(&self) -> u32 {
        self.peer_initial_window_size
    }

    /// The maximum number of bytes we can send on the connection right now.
    #[must_use]
    pub fn send_conn_window(&self) -> i64 {
        self.send_conn_window
    }

    /// Record a WINDOW_UPDATE received from the peer for the connection
    /// (stream_id == 0). Increases the connection-level send window.
    pub fn peer_window_update_connection(&mut self, increment: u32) {
        self.send_conn_window += i64::from(increment);
    }

    /// Consume `n` bytes of the connection-level send window.
    pub fn consume_send_conn_window(&mut self, n: u32) {
        self.send_conn_window -= i64::from(n);
    }
}

impl Default for H2FlowControl {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hpack_rfc_vector_first_request() {
        // RFC 7541 C.2.1 "First Request" header block.
        let block: [u8; 17] = [
            0x82, 0x86, 0x84, 0x41, 0x8c, 0xf1, 0xe3, 0xc2, 0xe5, 0xf2, 0x3a, 0x6b, 0xa0, 0xab,
            0x90, 0xf4, 0xff,
        ];
        let mut dec = HpackDecoder::new();
        let headers = dec.decode(&block).unwrap();

        assert!(headers.contains(&(b":method".to_vec(), b"GET".to_vec())));
        assert!(headers.contains(&(b":scheme".to_vec(), b"http".to_vec())));
        assert!(headers.contains(&(b":path".to_vec(), b"/".to_vec())));
        assert!(headers.contains(&(b":authority".to_vec(), b"www.example.com".to_vec())));
    }

    #[test]
    fn hpack_rejects_eos_symbol() {
        // A single EOS symbol (all-ones 30-bit code) is invalid. Construct a buffer
        // that decodes to EOS by providing exactly the EOS code bytes.
        //
        // EOS code = 0x3fffffff (30 bits). Provide it as 4 bytes with huffman flag.
        let buf: [u8; 5] = [0x80 | 4, 0xff, 0xff, 0xff, 0xff];
        let res = decode_string(&buf);
        assert!(matches!(res, Err(HpackError::InvalidHuffman)));
    }

    #[test]
    fn preface_constant_matches_rfc() {
        assert_eq!(PREFACE, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
        assert_eq!(PREFACE.len(), 24);
    }

    #[test]
    fn flow_control_no_update_below_threshold() {
        let mut fc = H2FlowControl::new();
        // Receive 1000 bytes — well below half of 65535.
        let increment = fc.data_received_connection(1000);
        assert_eq!(increment, 0);
    }

    #[test]
    fn flow_control_emits_update_at_threshold() {
        let mut fc = H2FlowControl::new();
        // Threshold is 65535 / 2 = 32767. Receive exactly that amount.
        let increment = fc.data_received_connection(32_767);
        assert_eq!(increment, 32_767);
    }

    #[test]
    fn flow_control_accumulates_across_calls() {
        let mut fc = H2FlowControl::new();
        // Two small receives that together exceed threshold.
        assert_eq!(fc.data_received_connection(20_000), 0);
        assert_eq!(fc.data_received_connection(20_000), 40_000);
    }

    #[test]
    fn flow_control_resets_consumed_after_update() {
        let mut fc = H2FlowControl::new();
        assert_eq!(fc.data_received_connection(40_000), 40_000);
        // After reset, small amounts should not trigger again.
        assert_eq!(fc.data_received_connection(1_000), 0);
    }

    #[test]
    fn flow_control_stream_below_threshold() {
        let fc = H2FlowControl::new();
        assert_eq!(fc.stream_window_update(1_000), 0);
    }

    #[test]
    fn flow_control_stream_at_threshold() {
        let fc = H2FlowControl::new();
        assert_eq!(fc.stream_window_update(32_767), 32_767);
    }

    #[test]
    fn flow_control_custom_initial_window() {
        let mut fc = H2FlowControl::new();
        fc.set_initial_window_size(100_000);
        // Threshold is now 50_000.
        assert_eq!(fc.data_received_connection(49_999), 0);
        assert_eq!(fc.data_received_connection(1), 50_000);
    }
}
