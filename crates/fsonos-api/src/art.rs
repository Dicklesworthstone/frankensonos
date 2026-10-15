//! Artwork from a discovered player's own image endpoint.

use asupersync::Cx;
use asupersync::http::h1::codec::HttpError;
use asupersync::http::{Client, ClientError};
use fastapi::{Response, ResponseBody};
use fsonos_core::policy::Client as Caller;
use fsonos_types::Player;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::time::Duration;

use crate::failure::{ErrorCode, Failure, http_error};
use crate::http::percent_decode;

const MAX_ART_BYTES: usize = 4 * 1024 * 1024;

pub(crate) fn validate_path(path: &str) -> Result<(), Failure> {
    let invalid = || {
        Failure::invalid(
            "u must be a /getaa path of at most 1024 bytes without traversal or an authority",
        )
    };
    if path.len() > 1024 {
        return Err(invalid());
    }
    let mut decoded = path.to_string();
    loop {
        if !(decoded == "/getaa"
            || decoded.starts_with("/getaa?")
            || decoded.starts_with("/getaa/"))
            || decoded.contains("//")
            || decoded.contains("..")
            || decoded.contains([':', '@', '\\', '#'])
            || decoded.bytes().any(|b| b.is_ascii_control() || b == b' ')
        {
            return Err(invalid());
        }
        if !decoded.contains('%') {
            return Ok(());
        }
        let next = percent_decode(&decoded).ok_or_else(invalid)?;
        if next == decoded {
            return Ok(());
        }
        decoded = next;
    }
}

fn encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

pub(crate) fn normalize(uri: Option<&str>, player: &Player) -> Option<String> {
    let uri = uri?;
    if uri.starts_with("https://") {
        return Some(uri.to_string());
    }
    let path = if let Some(rest) = uri.strip_prefix("http://") {
        let (authority, rest) = rest.split_once('/')?;
        if authority != SocketAddr::new(player.ip, 1400).to_string() {
            return None;
        }
        format!("/{rest}")
    } else {
        uri.to_string()
    };
    validate_path(&path).ok()?;
    Some(format!(
        "/speaker-art?player={}&u={}",
        encode(&player.id.0),
        encode(&path)
    ))
}

impl crate::Surface {
    pub(crate) async fn art(&self, cx: &Cx, caller: &Caller, player: &str, path: &str) -> Response {
        if let Err(err) = self.guard(caller).authorize("get_speaker_art", true) {
            return err.http_response();
        }
        if let Err(err) = validate_path(path) {
            return http_error(&err, 400, false);
        }
        let households = match self.households() {
            Ok(h) => h,
            Err(err) => return err.http_response(),
        };
        let Some(player) = households
            .iter()
            .flat_map(|h| &h.players)
            .find(|p| p.id.0 == player)
        else {
            return Failure::new(
                ErrorCode::UnknownPlayer,
                "No discovered player has that id.",
            )
            .http_response();
        };
        let url = format!("http://{}{path}", SocketAddr::new(player.ip, 1400));
        let client = Client::builder()
            .no_redirects()
            .no_retries()
            .no_proxy()
            .max_body_size(MAX_ART_BYTES)
            .build();
        let upstream = match client
            .get(url)
            .timeout(Duration::from_secs(5))
            .send(cx)
            .await
        {
            Ok(response) => response,
            Err(ClientError::HttpError(
                HttpError::BodyTooLarge | HttpError::BodyTooLargeDetailed { .. },
            )) => {
                return bad_art();
            }
            Err(_) => {
                return http_error(
                    &Failure::new(
                        ErrorCode::PlayerUnreachable,
                        "Speaker artwork request failed or exceeded five seconds.",
                    ),
                    502,
                    true,
                );
            }
        };
        if upstream.status == 404 {
            return http_error(
                &Failure::new(ErrorCode::BadArt, "Speaker artwork was not found."),
                404,
                false,
            );
        }
        let kind = upstream
            .header_value("content-type")
            .unwrap_or_default()
            .trim();
        if upstream.status != 200 || !is_image(kind) || upstream.body.len() > MAX_ART_BYTES {
            return bad_art();
        }
        Response::ok()
            .header("content-type", kind.as_bytes().to_vec())
            .header("cache-control", b"public, max-age=86400".to_vec())
            .body(ResponseBody::Bytes(upstream.body))
    }
}

fn is_image(kind: &str) -> bool {
    let media = kind.split(';').next().unwrap_or_default().trim();
    let Some((prefix, subtype)) = media.split_once('/') else {
        return false;
    };
    prefix.eq_ignore_ascii_case("image")
        && !subtype.is_empty()
        && subtype.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'!' | b'#' | b'$' | b'&' | b'-' | b'^' | b'_' | b'.' | b'+'
                )
        })
}

fn bad_art() -> Response {
    Failure::new(
        ErrorCode::BadArt,
        "Speaker returned invalid or oversized artwork.",
    )
    .http_response()
}
