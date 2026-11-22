//! Bounded HTTP response framing for the local daemon's blocking client.
//! A complete acknowledgement does not depend on the server closing its socket.

use std::io;

const MAX_HEAD: usize = 64 * 1024;
const MAX_BODY: usize = 16 * 1024 * 1024;

#[derive(Default)]
enum State {
    #[default]
    Head,
    Fixed(usize),
    UntilClose,
    ChunkSize,
    ChunkData(usize),
    ChunkEnd(usize),
    Trailers,
    Done,
}

/// One response only. Metadata and decoded body have independent bounds;
/// consumed chunk framing is discarded instead of retained with the body.
#[derive(Default)]
pub(super) struct Response {
    state: State,
    status: Option<u16>,
    length: Option<usize>,
    chunked: bool,
    line: Vec<u8>,
    metadata: usize,
    body: Vec<u8>,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn field(line: &str) -> io::Result<(&str, &str)> {
    let (name, value) = line
        .split_once(':')
        .ok_or_else(|| invalid("malformed response header"))?;
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
        || value
            .bytes()
            .any(|byte| byte < b' ' && byte != b'\t' || byte == 0x7f)
    {
        return Err(invalid("malformed response header"));
    }
    Ok((name, value.trim_matches([' ', '\t'])))
}

impl Response {
    /// Consume another read. `true` means the complete framed reply arrived;
    /// unframed replies still require EOF, even when their bytes form valid JSON.
    pub(super) fn push(&mut self, mut bytes: &[u8]) -> io::Result<bool> {
        while !bytes.is_empty() {
            match self.state {
                State::Fixed(left) | State::ChunkData(left) => {
                    let count = left.min(bytes.len());
                    self.append(&bytes[..count])?;
                    bytes = &bytes[count..];
                    self.state = match self.state {
                        State::Fixed(_) if count == left => State::Done,
                        State::Fixed(_) => State::Fixed(left - count),
                        _ if count == left => State::ChunkEnd(0),
                        _ => State::ChunkData(left - count),
                    };
                }
                State::UntilClose => {
                    self.append(bytes)?;
                    bytes = &[];
                }
                State::ChunkEnd(seen) => {
                    if bytes[0] != b"\r\n"[seen] {
                        return Err(invalid("missing chunk delimiter"));
                    }
                    bytes = &bytes[1..];
                    self.state = if seen == 0 {
                        State::ChunkEnd(1)
                    } else {
                        State::ChunkSize
                    };
                }
                State::Done => return Err(invalid("overlong response body")),
                State::Head | State::ChunkSize | State::Trailers => {
                    let count = bytes
                        .iter()
                        .position(|byte| *byte == b'\n')
                        .map_or(bytes.len(), |position| position + 1);
                    if self.line.len().saturating_add(count) > MAX_HEAD {
                        return Err(invalid("response metadata exceeds 64 KiB"));
                    }
                    if matches!(self.state, State::Head | State::Trailers) {
                        self.metadata = self.metadata.saturating_add(count);
                        if self.metadata > MAX_HEAD {
                            return Err(invalid("response metadata exceeds 64 KiB"));
                        }
                    }
                    self.line.extend_from_slice(&bytes[..count]);
                    bytes = &bytes[count..];
                    if self.line.last() != Some(&b'\n') {
                        continue;
                    }
                    let line = std::mem::take(&mut self.line);
                    let line = line
                        .strip_suffix(b"\r\n")
                        .ok_or_else(|| invalid("response metadata requires CRLF"))?;
                    let line = std::str::from_utf8(line)
                        .map_err(|_| invalid("response metadata is not UTF-8"))?;
                    match self.state {
                        State::Head => self.header(line)?,
                        State::ChunkSize => self.chunk(line)?,
                        State::Trailers if line.is_empty() => self.state = State::Done,
                        State::Trailers => {
                            let (name, _) = field(line)?;
                            if name.eq_ignore_ascii_case("content-length")
                                || name.eq_ignore_ascii_case("transfer-encoding")
                            {
                                return Err(invalid("response trailer changes message framing"));
                            }
                        }
                        _ => unreachable!("only line-oriented states collect metadata"),
                    }
                }
            }
        }
        Ok(matches!(self.state, State::Done))
    }

    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.body.len().saturating_add(bytes.len()) > MAX_BODY {
            return Err(invalid("daemon response body exceeds 16 MiB"));
        }
        self.body.extend_from_slice(bytes);
        Ok(())
    }

    fn header(&mut self, line: &str) -> io::Result<()> {
        if self.status.is_none() {
            let mut words = line.splitn(3, ' ');
            let version = words.next().unwrap_or_default();
            let status = words.next().unwrap_or_default();
            if !matches!(version, "HTTP/1.0" | "HTTP/1.1")
                || status.len() != 3
                || !status.bytes().all(|byte| byte.is_ascii_digit())
            {
                return Err(invalid("invalid response status line"));
            }
            let status: u16 = status
                .parse()
                .map_err(|_| invalid("invalid response status"))?;
            if !(100..600).contains(&status) {
                return Err(invalid("invalid response status"));
            }
            self.status = Some(status);
        } else if line.is_empty() {
            if self.chunked && self.length.is_some() {
                return Err(invalid("conflicting response framing headers"));
            }
            match self.status {
                Some(101) => return Err(invalid("the daemon unexpectedly switched protocols")),
                Some(100..=199) => {
                    // Informational replies precede the actual result. Keep
                    // the shared metadata budget across them.
                    if self.chunked || self.length.is_some_and(|length| length != 0) {
                        return Err(invalid("informational response has a body"));
                    }
                    self.status = None;
                    self.length = None;
                }
                Some(204 | 304) => self.state = State::Done,
                _ if self.chunked => self.state = State::ChunkSize,
                _ => {
                    self.state = match self.length {
                        Some(0) => State::Done,
                        Some(length) => State::Fixed(length),
                        None => State::UntilClose,
                    }
                }
            }
        } else {
            let (name, value) = field(line)?;
            if name.eq_ignore_ascii_case("content-length") {
                if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(invalid("invalid Content-Length"));
                }
                let length = value
                    .parse()
                    .map_err(|_| invalid("invalid Content-Length"))?;
                if length > MAX_BODY {
                    return Err(invalid("daemon response body exceeds 16 MiB"));
                }
                if self
                    .length
                    .replace(length)
                    .is_some_and(|previous| previous != length)
                {
                    return Err(invalid("conflicting Content-Length headers"));
                }
            } else if name.eq_ignore_ascii_case("transfer-encoding") {
                if self.chunked || !value.eq_ignore_ascii_case("chunked") {
                    return Err(invalid("unsupported response Transfer-Encoding"));
                }
                self.chunked = true;
            }
        }
        Ok(())
    }

    fn chunk(&mut self, line: &str) -> io::Result<()> {
        let size = line.split(';').next().unwrap_or_default();
        if size.is_empty() || !size.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(invalid("bad chunk size"));
        }
        let size = usize::from_str_radix(size, 16).map_err(|_| invalid("bad chunk size"))?;
        if size > MAX_BODY.saturating_sub(self.body.len()) {
            return Err(invalid("daemon response body exceeds 16 MiB"));
        }
        self.state = if size == 0 {
            State::Trailers
        } else {
            State::ChunkData(size)
        };
        Ok(())
    }

    /// Finish after framed completion or actual EOF. A premature close never
    /// promotes a parseable JSON prefix into a complete acknowledgement.
    pub(super) fn finish(self) -> io::Result<(u16, String)> {
        if !matches!(self.state, State::Done | State::UntilClose) {
            return Err(invalid("incomplete daemon response"));
        }
        let status = self.status.ok_or_else(|| invalid("no response status"))?;
        let body =
            String::from_utf8(self.body).map_err(|_| invalid("response body is not UTF-8"))?;
        Ok((status, body))
    }
}

#[cfg(test)]
pub(super) fn parse_response(raw: &[u8]) -> io::Result<(u16, String)> {
    let mut response = Response::default();
    response.push(raw)?;
    response.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framed_replies_complete_at_every_possible_read_boundary() {
        for raw in [
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n[]",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1;name=value\r\n[\r\n1\r\n]\r\n0\r\nChecked: yes\r\n\r\n",
            "HTTP/1.1 103 Early Hints\r\nLink: </remote.css>\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n[]",
        ] {
            for split in 0..raw.len() {
                let mut response = Response::default();
                assert!(
                    !response.push(&raw.as_bytes()[..split]).unwrap(),
                    "split {split}"
                );
                assert!(response.push(&raw.as_bytes()[split..]).unwrap());
                assert_eq!(response.finish().unwrap(), (200, "[]".into()));
            }
            let mut response = Response::default();
            for (position, byte) in raw.as_bytes().iter().enumerate() {
                assert_eq!(response.push(&[*byte]).unwrap(), position == raw.len() - 1);
            }
            assert_eq!(response.finish().unwrap(), (200, "[]".into()));
        }
    }

    #[test]
    fn only_an_unframed_body_uses_connection_close_as_completion() {
        let mut response = Response::default();
        assert!(!response.push(b"HTTP/1.0 200 OK\r\n\r\n[]").unwrap());
        assert_eq!(response.finish().unwrap(), (200, "[]".into()));
        for raw in [
            "HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\n[]",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n[]\r\n",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n[]\r\n0\r\n",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n[]\r\n0\r\nChecked: yes\r\n",
            "HTTP/1.1 100 Continue\r\n\r\n",
        ] {
            let mut response = Response::default();
            assert!(!response.push(raw.as_bytes()).unwrap());
            assert_eq!(
                response.finish().unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
        assert_eq!(
            parse_response(b"HTTP/1.1 204 No Content\r\n\r\n").unwrap(),
            (204, String::new())
        );
    }

    #[test]
    fn declared_body_limits_are_checked_before_allocating_or_waiting() {
        for raw in [
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                MAX_BODY + 1
            ),
            format!(
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
                MAX_BODY + 1
            ),
        ] {
            let mut response = Response::default();
            let error = response.push(raw.as_bytes()).unwrap_err();
            assert!(error.to_string().contains("16 MiB"), "{error}");
            assert!(response.body.is_empty());
        }
    }

    #[test]
    fn body_limit_applies_to_unframed_and_accumulated_chunked_content() {
        for chunked in [false, true] {
            let mut response = Response::default();
            let header = if chunked {
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n"
            } else {
                "HTTP/1.1 200 OK\r\n\r\n"
            };
            response.push(header.as_bytes()).unwrap();
            let block = vec![b'x'; 8192];
            for _ in 0..MAX_BODY / block.len() {
                if chunked {
                    response.push(b"2000\r\n").unwrap();
                }
                response.push(&block).unwrap();
                if chunked {
                    response.push(b"\r\n").unwrap();
                }
            }
            assert_eq!(response.body.len(), MAX_BODY);
            let overflow: &[u8] = if chunked { b"1\r\n" } else { b"x" };
            assert!(
                response
                    .push(overflow)
                    .unwrap_err()
                    .to_string()
                    .contains("16 MiB")
            );
            assert_eq!(response.body.len(), MAX_BODY);
        }
    }

    #[test]
    fn metadata_bounds_cover_headers_chunk_extensions_and_trailers() {
        for prefix in [
            "HTTP/1.1 200 OK\r\nHeader: ",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1;extension=",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\nTrailer: ",
        ] {
            let mut response = Response::default();
            response.push(prefix.as_bytes()).unwrap();
            assert!(
                response
                    .push(&vec![b'x'; MAX_HEAD])
                    .unwrap_err()
                    .to_string()
                    .contains("64 KiB")
            );
            assert!(response.line.len() < MAX_HEAD);
        }
        let mut response = Response::default();
        let many_headers = format!("HTTP/1.1 200 OK\r\n{}", "X: a\r\n".repeat(MAX_HEAD / 6 + 1));
        assert!(response.push(many_headers.as_bytes()).is_err());
        let mut response = Response::default();
        let hints = "HTTP/1.1 103 Early Hints\r\n\r\n".repeat(MAX_HEAD / 20);
        assert!(response.push(hints.as_bytes()).is_err());
    }

    #[test]
    fn ambiguous_or_malformed_framing_never_confirms_a_result() {
        for raw in [
            "garbage 200 OK\r\nContent-Length: 2\r\n\r\n[]",
            "HTTP/1.1 200 OK\nContent-Length: 2\r\n\r\n[]",
            "HTTP/1.1 200 OK\r\nContent-Length: +2\r\n\r\n[]",
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n[]\r\n0\r\n\r\n",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\n\r\n[]",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked, chunked\r\n\r\n[]",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n\r\n[]",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n[]\r\n0\r\nnot a trailer\r\n\r\n",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n[]\r\n0\r\nContent-Length: 2\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n[]extra",
            "HTTP/1.1 101 Switching Protocols\r\n\r\n",
        ] {
            assert!(parse_response(raw.as_bytes()).is_err(), "{raw:?}");
        }
    }
}
