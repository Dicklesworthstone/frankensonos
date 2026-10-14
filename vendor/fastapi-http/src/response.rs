//! HTTP response writer.

use asupersync::stream::Stream;
use fastapi_core::{BodyStream, Response, ResponseBody, StatusCode};
use std::borrow::Cow;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Serialized response output.
pub enum ResponseWrite {
    /// Fully-buffered response bytes.
    Full(Vec<u8>),
    /// Chunked stream (head + body chunks).
    Stream(ChunkedEncoder),
}

/// HTTP trailers sent after a chunked response body.
///
/// Per RFC 7230, trailers are headers sent after the final chunk in a
/// chunked transfer encoding. Common uses include content digests,
/// signatures, and final status after streaming.
///
/// # Example
///
/// ```
/// use fastapi_http::Trailers;
///
/// let trailers = Trailers::new()
///     .add("Content-MD5", "Q2hlY2tzdW0=")
///     .add("Server-Timing", "total;dur=123");
/// ```
#[derive(Debug, Clone, Default)]
pub struct Trailers {
    headers: Vec<(String, String)>,
}

impl Trailers {
    /// Create an empty trailers set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a trailer header.
    #[must_use]
    pub fn add(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Returns true if no trailers are set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.headers.is_empty()
    }

    /// Returns the trailer header names as a comma-separated string
    /// for the `Trailer` response header.
    #[must_use]
    pub fn trailer_header_value(&self) -> String {
        self.headers
            .iter()
            .map(|(n, _)| n.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Encode the trailers as bytes for the chunked encoding terminator.
    ///
    /// Format: `name: value\r\n` for each trailer.
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, value) in &self.headers {
            write_header_line(&mut out, name, value.as_bytes());
        }
        out
    }
}

/// Streaming chunked response encoder.
pub struct ChunkedEncoder {
    head: Option<Vec<u8>>,
    body: BodyStream,
    finished: bool,
    trailers: Option<Trailers>,
}

impl ChunkedEncoder {
    fn new(head: Vec<u8>, body: BodyStream) -> Self {
        Self {
            head: Some(head),
            body,
            finished: false,
            trailers: None,
        }
    }

    /// Set trailers to be sent after the final chunk.
    #[must_use]
    pub fn with_trailers(mut self, trailers: Trailers) -> Self {
        self.trailers = Some(trailers);
        self
    }

    fn encode_chunk(chunk: &[u8]) -> Vec<u8> {
        // Use std::io::Write to format hex directly into buffer without allocation.
        // Max hex digits for usize is 16 (64-bit), so we pre-allocate conservatively.
        use std::io::Write as _;
        let mut out = Vec::with_capacity(20 + chunk.len() + 4);
        write!(out, "{:x}\r\n", chunk.len()).expect("write to Vec cannot fail");
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\r\n");
        out
    }

    /// Encode the final chunk with optional trailers.
    ///
    /// Per RFC 7230 Section 4.1:
    /// - Without trailers: `0\r\n\r\n`
    /// - With trailers: `0\r\n<trailer-headers>\r\n`
    fn encode_final_chunk(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"0\r\n");
        if let Some(ref trailers) = self.trailers {
            out.extend_from_slice(&trailers.encode());
        }
        out.extend_from_slice(b"\r\n");
        out
    }
}

impl Stream for ChunkedEncoder {
    type Item = Vec<u8>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(head) = self.head.take() {
            return Poll::Ready(Some(head));
        }

        if self.finished {
            return Poll::Ready(None);
        }

        loop {
            match self.body.as_mut().poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(chunk)) => {
                    if chunk.is_empty() {
                        continue;
                    }
                    return Poll::Ready(Some(Self::encode_chunk(&chunk)));
                }
                Poll::Ready(None) => {
                    self.finished = true;
                    return Poll::Ready(Some(self.encode_final_chunk()));
                }
            }
        }
    }
}

/// Writes HTTP responses to a buffer.
pub struct ResponseWriter {
    buffer: Vec<u8>,
}

impl ResponseWriter {
    /// Create a new response writer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            buffer: Vec::with_capacity(4096),
        }
    }

    /// Write a response into either a full buffer or a stream.
    #[must_use]
    pub fn write(&mut self, response: Response) -> ResponseWrite {
        let (status, headers, body) = response.into_parts();
        match body {
            ResponseBody::Empty => {
                let bytes = self.write_full(status, &headers, &[]);
                ResponseWrite::Full(bytes)
            }
            ResponseBody::Bytes(body) => {
                let bytes = self.write_full(status, &headers, &body);
                ResponseWrite::Full(bytes)
            }
            ResponseBody::Stream(body) => {
                let head = self.write_stream_head(status, &headers);
                ResponseWrite::Stream(ChunkedEncoder::new(head, body))
            }
        }
    }

    fn write_full(
        &mut self,
        status: StatusCode,
        headers: &[(String, Vec<u8>)],
        body: &[u8],
    ) -> Vec<u8> {
        self.buffer.clear();

        // Status line
        self.buffer.extend_from_slice(b"HTTP/1.1 ");
        self.write_status(status);
        self.buffer.extend_from_slice(b"\r\n");

        // Headers (filter hop-by-hop content-length/transfer-encoding)
        for (name, value) in headers {
            if is_content_length(name) || is_transfer_encoding(name) {
                continue;
            }
            write_header_line(&mut self.buffer, name, value);
        }

        // Content-Length
        self.buffer.extend_from_slice(b"content-length: ");
        self.buffer
            .extend_from_slice(body.len().to_string().as_bytes());
        self.buffer.extend_from_slice(b"\r\n");

        // End of headers
        self.buffer.extend_from_slice(b"\r\n");

        // Body
        self.buffer.extend_from_slice(body);

        self.take_buffer()
    }

    fn write_stream_head(&mut self, status: StatusCode, headers: &[(String, Vec<u8>)]) -> Vec<u8> {
        self.buffer.clear();

        // Status line
        self.buffer.extend_from_slice(b"HTTP/1.1 ");
        self.write_status(status);
        self.buffer.extend_from_slice(b"\r\n");

        // Headers (filter hop-by-hop content-length/transfer-encoding)
        for (name, value) in headers {
            if is_content_length(name) || is_transfer_encoding(name) {
                continue;
            }
            write_header_line(&mut self.buffer, name, value);
        }

        // Transfer-Encoding: chunked
        self.buffer
            .extend_from_slice(b"transfer-encoding: chunked\r\n");

        // End of headers
        self.buffer.extend_from_slice(b"\r\n");

        self.take_buffer()
    }

    fn write_status(&mut self, status: StatusCode) {
        let code = status.as_u16();
        self.buffer.extend_from_slice(code.to_string().as_bytes());
        self.buffer.extend_from_slice(b" ");
        self.buffer
            .extend_from_slice(status.canonical_reason().as_bytes());
    }

    fn take_buffer(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        std::mem::swap(&mut out, &mut self.buffer);
        self.buffer = Vec::with_capacity(out.capacity());
        out
    }
}

fn is_content_length(name: &str) -> bool {
    name.eq_ignore_ascii_case("content-length")
}

fn is_transfer_encoding(name: &str) -> bool {
    name.eq_ignore_ascii_case("transfer-encoding")
}

fn write_header_line(buffer: &mut Vec<u8>, name: &str, value: &[u8]) {
    if !is_valid_header_name(name) {
        return;
    }
    buffer.extend_from_slice(name.as_bytes());
    buffer.extend_from_slice(b": ");
    buffer.extend_from_slice(sanitize_header_value(value).as_ref());
    buffer.extend_from_slice(b"\r\n");
}

fn sanitize_header_value(value: &[u8]) -> Cow<'_, [u8]> {
    if value
        .iter()
        .all(|&byte| byte != b'\r' && byte != b'\n' && byte != 0)
    {
        return Cow::Borrowed(value);
    }
    Cow::Owned(
        value
            .iter()
            .copied()
            .filter(|&byte| byte != b'\r' && byte != b'\n' && byte != 0)
            .collect(),
    )
}

fn is_valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            matches!(
                byte,
                b'!' | b'#'
                    | b'$'
                    | b'%'
                    | b'&'
                    | b'\''
                    | b'*'
                    | b'+'
                    | b'-'
                    | b'.'
                    | b'0'..=b'9'
                    | b'A'..=b'Z'
                    | b'^'
                    | b'_'
                    | b'`'
                    | b'a'..=b'z'
                    | b'|'
                    | b'~'
            )
        })
}

impl Default for ResponseWriter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::stream::iter;
    use std::task::Waker;

    fn noop_waker() -> Waker {
        Waker::noop().clone()
    }

    fn collect_stream<S: Stream<Item = Vec<u8>> + Unpin>(mut stream: S) -> Vec<u8> {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut out = Vec::new();

        loop {
            match Pin::new(&mut stream).poll_next(&mut cx) {
                Poll::Ready(Some(chunk)) => out.extend_from_slice(&chunk),
                Poll::Ready(None) => break,
                Poll::Pending => panic!("unexpected pending stream"),
            }
        }

        out
    }

    #[test]
    fn write_full_sets_content_length() {
        let response = Response::ok()
            .header("content-type", b"text/plain".to_vec())
            .body(ResponseBody::Bytes(b"hello".to_vec()));
        let mut writer = ResponseWriter::new();
        let bytes = match writer.write(response) {
            ResponseWrite::Full(bytes) => bytes,
            ResponseWrite::Stream(_) => panic!("expected full response"),
        };
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("content-length: 5\r\n"));
        assert!(text.contains("\r\n\r\nhello"));
    }

    #[test]
    fn write_stream_uses_chunked_encoding() {
        let stream = iter(vec![b"hello".to_vec(), b"world".to_vec()]);
        let response = Response::ok()
            .header("content-type", b"text/plain".to_vec())
            .body(ResponseBody::stream(stream));
        let mut writer = ResponseWriter::new();
        let bytes = match writer.write(response) {
            ResponseWrite::Stream(stream) => collect_stream(stream),
            ResponseWrite::Full(_) => panic!("expected stream response"),
        };

        let expected = b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ntransfer-encoding: chunked\r\n\r\n5\r\nhello\r\n5\r\nworld\r\n0\r\n\r\n";
        assert_eq!(bytes, expected);
    }

    // ====================================================================
    // Trailer Tests
    // ====================================================================

    #[test]
    fn trailers_empty() {
        let t = Trailers::new();
        assert!(t.is_empty());
        assert_eq!(t.trailer_header_value(), "");
    }

    #[test]
    fn trailers_encode() {
        let t = Trailers::new()
            .add("Content-MD5", "abc123")
            .add("Server-Timing", "total;dur=50");
        assert!(!t.is_empty());
        assert_eq!(t.trailer_header_value(), "Content-MD5, Server-Timing");
        let encoded = t.encode();
        let s = std::str::from_utf8(&encoded).unwrap();
        assert!(s.contains("Content-MD5: abc123\r\n"));
        assert!(s.contains("Server-Timing: total;dur=50\r\n"));
    }

    #[test]
    fn chunked_encoder_with_trailers() {
        let stream = iter(vec![b"data".to_vec()]);
        let body = Box::pin(stream) as BodyStream;
        let head = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
        let trailers = Trailers::new().add("Checksum", "deadbeef");
        let encoder = ChunkedEncoder::new(head, body).with_trailers(trailers);
        let bytes = collect_stream(encoder);
        let s = std::str::from_utf8(&bytes).unwrap();
        // Should contain the trailer after the final chunk
        assert!(s.contains("0\r\nChecksum: deadbeef\r\n\r\n"));
    }

    #[test]
    fn chunked_encoder_without_trailers_unchanged() {
        let stream = iter(vec![b"hi".to_vec()]);
        let body = Box::pin(stream) as BodyStream;
        let head = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
        let encoder = ChunkedEncoder::new(head, body);
        let bytes = collect_stream(encoder);
        assert!(bytes.ends_with(b"0\r\n\r\n"));
    }

    #[test]
    fn final_chunk_format_with_multiple_trailers() {
        let t = Trailers::new()
            .add("Digest", "sha-256=abc")
            .add("Signature", "sig123");
        let encoder = ChunkedEncoder {
            head: None,
            body: Box::pin(iter(Vec::<Vec<u8>>::new())),
            finished: false,
            trailers: Some(t),
        };
        let final_chunk = encoder.encode_final_chunk();
        let s = std::str::from_utf8(&final_chunk).unwrap();
        assert_eq!(s, "0\r\nDigest: sha-256=abc\r\nSignature: sig123\r\n\r\n");
    }

    #[test]
    fn write_full_drops_invalid_header_names_and_sanitizes_values() {
        let mut writer = ResponseWriter::new();
        let headers = vec![
            ("x-ok".to_string(), b"safe".to_vec()),
            ("bad\r\nname".to_string(), b"ignored".to_vec()),
            ("x-test".to_string(), b"hello\r\nx-injected: yes".to_vec()),
        ];

        let bytes = writer.write_full(StatusCode::OK, &headers, b"body");
        let text = String::from_utf8_lossy(&bytes);

        assert!(text.contains("x-ok: safe\r\n"));
        assert!(!text.contains("bad\r\nname:"));
        assert!(text.contains("x-test: hellox-injected: yes\r\n"));
        assert!(!text.contains("\r\nx-injected: yes\r\n"));
    }

    #[test]
    fn write_stream_head_drops_invalid_header_names_and_sanitizes_values() {
        let mut writer = ResponseWriter::new();
        let headers = vec![
            ("content-type".to_string(), b"text/plain".to_vec()),
            ("bad\nname".to_string(), b"ignored".to_vec()),
            ("x-test".to_string(), b"hello\r\nx-injected: yes".to_vec()),
        ];

        let bytes = writer.write_stream_head(StatusCode::OK, &headers);
        let text = String::from_utf8_lossy(&bytes);

        assert!(text.contains("content-type: text/plain\r\n"));
        assert!(!text.contains("bad\nname:"));
        assert!(text.contains("x-test: hellox-injected: yes\r\n"));
        assert!(!text.contains("\r\nx-injected: yes\r\n"));
    }

    #[test]
    fn trailers_encode_drops_invalid_names_and_sanitizes_values() {
        let encoded = Trailers::new()
            .add("Checksum", "abc123")
            .add("Bad\r\nName", "ignored")
            .add("Signature", "sig\r\nInjected: yes")
            .encode();
        let text = std::str::from_utf8(&encoded).unwrap();

        assert!(text.contains("Checksum: abc123\r\n"));
        assert!(!text.contains("Bad\r\nName"));
        assert!(text.contains("Signature: sigInjected: yes\r\n"));
        assert!(!text.contains("\r\nInjected: yes\r\n"));
    }
}
