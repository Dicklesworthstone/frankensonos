//! One deadline covers a loopback request, including incremental body reads.
//!
//! A socket's read/write timeout bounds each individual system call, not the
//! whole exchange. A daemon that keeps sending a little data must not hold a
//! CLI command or a forwarded stdio tool forever.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

fn remaining(deadline: Instant) -> io::Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "the daemon response deadline expired",
        ))
    } else {
        Ok(remaining)
    }
}

pub(super) fn exchange(
    address: SocketAddr,
    request: &[u8],
    timeout: Duration,
    connect_timeout: Duration,
) -> io::Result<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    let mut stream =
        TcpStream::connect_timeout(&address, remaining(deadline)?.min(connect_timeout))?;
    let mut unsent = request;
    while !unsent.is_empty() {
        stream.set_write_timeout(Some(remaining(deadline)?))?;
        match stream.write(unsent) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(written) => unsent = &unsent[written..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    let mut response = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        stream.set_read_timeout(Some(remaining(deadline)?))?;
        match stream.read(&mut buffer) {
            Ok(0) => return Ok(response),
            Ok(read) => response.extend_from_slice(&buffer[..read]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::sync::mpsc;

    const REQUEST: &[u8] = b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";

    fn receive(listener: &TcpListener) -> TcpStream {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(&mut stream);
        let mut head = String::new();
        while !head.ends_with("\r\n\r\n") {
            assert!(reader.read_line(&mut head).unwrap() > 0);
        }
        assert_eq!(head.as_bytes(), REQUEST);
        stream
    }

    #[test]
    fn a_complete_loopback_response_preserves_every_byte() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let reply = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";
        let worker = std::thread::spawn(move || {
            receive(&listener).write_all(reply).unwrap();
            listener
        });
        let result = exchange(
            address,
            REQUEST,
            Duration::from_secs(5),
            Duration::from_secs(1),
        );
        let listener = worker.join().unwrap();
        assert_eq!(result.unwrap(), reply);
        listener.set_nonblocking(true).unwrap();
        assert!(
            listener.accept().is_err(),
            "an exchange never repeats a request"
        );
    }

    #[test]
    fn incremental_response_progress_cannot_extend_the_total_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = std::thread::spawn(move || {
            let mut stream = receive(&listener);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\n")
                .unwrap();
            // Every byte arrives within the old per-read timeout. The complete
            // response would take a second, beyond this exchange's 200 ms.
            for _ in 0..20 {
                if stream.write_all(b"x").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        let began = Instant::now();
        let failure = exchange(
            address,
            REQUEST,
            Duration::from_millis(200),
            Duration::from_secs(1),
        )
        .unwrap_err();
        let elapsed = began.elapsed();
        worker.join().unwrap();
        assert!(matches!(
            failure.kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        ));
        assert!(elapsed < Duration::from_secs(1), "waited {elapsed:?}");
    }

    #[test]
    fn a_stalled_response_expires_without_waiting_for_server_close() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (release, hold) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _stream = receive(&listener);
            hold.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        let result = exchange(
            address,
            REQUEST,
            Duration::from_millis(100),
            Duration::from_secs(1),
        );
        release.send(()).unwrap();
        worker.join().unwrap();
        assert!(matches!(
            result.unwrap_err().kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        ));
    }
}
