//! Fetching the media a player is told to play.
//!
//! A real player fetches the URI it plays, and a clip it cannot fetch leaves
//! it STOPPED. The simulator does the same, but only for loopback `http://`
//! URLs (in tests, the daemon's clip server); it never touches the network,
//! so anything else is taken to be a stream that plays.

use crate::model::State;
use asupersync::Cx;
use asupersync::http::Client;
use asupersync::runtime::{RuntimeBuilder, reactor::create_reactor};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

/// What a fetch came back with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Fetched {
    /// The HTTP status, or why the request failed.
    pub status: Result<u16, String>,
    pub bytes: usize,
    /// The length, when the body is a PCM WAV.
    pub wav_duration_ms: Option<u64>,
}

/// Whether `uri` is loopback `http://` media the simulator may fetch.
pub(crate) fn is_fetchable(uri: &str) -> bool {
    let Some(rest) = uri.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => authority
            .rsplit_once(':')
            .map_or(authority, |(host, _)| host),
    };
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Fetch `url` for player `p` on a thread of its own, then report back.
pub(crate) fn spawn(shared: Arc<Mutex<State>>, p: usize, url: String) {
    std::thread::spawn(move || {
        let fetched = get(&url);
        if let Ok(mut state) = shared.lock() {
            state.media_fetched(p, &url, fetched);
        }
    });
}

fn get(url: &str) -> Fetched {
    let failed = |why: String| Fetched {
        status: Err(why),
        bytes: 0,
        wav_duration_ms: None,
    };
    let runtime = match create_reactor().and_then(|reactor| {
        RuntimeBuilder::current_thread()
            .with_reactor(reactor)
            .build()
            .map_err(std::io::Error::other)
    }) {
        Ok(runtime) => runtime,
        Err(e) => return failed(e.to_string()),
    };
    let response = runtime.block_on(async {
        let cx = Cx::current().ok_or_else(|| "no runtime context".to_string())?;
        Client::default_for_runtime(&cx)
            .get(url.to_string())
            .send(&cx)
            .await
            .map_err(|e| e.to_string())
    });
    match response {
        Ok(r) => Fetched {
            status: Ok(r.status),
            bytes: r.body.len(),
            wav_duration_ms: wav_duration_ms(&r.body),
        },
        Err(e) => failed(e),
    }
}

/// The playing time of a PCM WAV, from its `fmt ` and `data` chunks.
pub(crate) fn wav_duration_ms(body: &[u8]) -> Option<u64> {
    if body.get(..4)? != b"RIFF" || body.get(8..12)? != b"WAVE" {
        return None;
    }
    let u32_at = |i: usize| {
        body.get(i..i + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    let mut byte_rate = None;
    let mut at = 12;
    while at + 8 <= body.len() {
        let len = usize::try_from(u32_at(at + 4)?).ok()?;
        let start = at + 8;
        match &body[at..at + 4] {
            b"fmt " => byte_rate = u32_at(start + 8).filter(|rate| *rate > 0),
            b"data" => {
                let data = u64::try_from(len.min(body.len() - start)).ok()?;
                return Some(data * 1000 / u64::from(byte_rate?));
            }
            _ => {}
        }
        at = start + len + (len & 1);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_http_is_fetched() {
        for uri in [
            "http://127.0.0.1:3400/media/a.wav",
            "http://127.9.9.9/x",
            "http://localhost:8080/clip.wav?x=1",
            "http://[::1]:9/clip.wav",
        ] {
            assert!(is_fetchable(uri), "{uri}");
        }
        for uri in [
            "https://127.0.0.1/a.wav",
            "http://192.0.2.10:3400/media/a.wav",
            "http://radio.example/stream",
            "x-rincon-mp3radio://127.0.0.1/a",
            "x-sonos-spotify:spotify%3atrack%3a1",
            "http://[2001:db8::1]/a",
        ] {
            assert!(!is_fetchable(uri), "{uri}");
        }
    }

    #[test]
    fn wav_lengths_come_from_the_data_chunk() {
        // 44.1 kHz mono 16-bit: 88 200 bytes a second; a padding chunk first.
        let mut wav = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&[1, 0, 1, 0]);
        wav.extend_from_slice(&44_100u32.to_le_bytes());
        wav.extend_from_slice(&88_200u32.to_le_bytes());
        wav.extend_from_slice(&[2, 0, 16, 0]);
        wav.extend_from_slice(b"FLLR");
        wav.extend_from_slice(&3u32.to_le_bytes());
        wav.extend_from_slice(&[0; 4]);
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&132_300u32.to_le_bytes());
        wav.extend_from_slice(&vec![0; 132_300]);
        assert_eq!(wav_duration_ms(&wav), Some(1_500));
        assert_eq!(wav_duration_ms(b"ID3\x04 not a wav"), None);
        assert_eq!(wav_duration_ms(&wav[..40]), None);
    }
}
