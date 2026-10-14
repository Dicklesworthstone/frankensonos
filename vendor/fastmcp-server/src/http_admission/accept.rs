//! Bounded, quote-aware response negotiation for the modern POST boundary.
//!
//! RFC 9110 sections 12.4.2 and 12.5.1: determine each representation's quality
//! from its most specific matching range, THEN compare the representations.
//! In particular, an exact q=0 cannot be undone by a positive wildcard. JSON
//! wins equal qualities, retaining the existing default for ordinary clients.
//!
//! The response writer offers parameter-free application/json and
//! text/event-stream. A range requiring other media parameters does not match
//! either offer. Invalid syntax rejects negotiation rather than salvaging
//! preferences from a partially parsed field. Duplicate equally specific
//! ranges use the lower quality, independent of field or member order.

use super::{ModernPostRejection, ResponseRepresentation};
use fastmcp_transport::http::{HttpResponsePreferences, HttpResponseRepresentation};

pub(super) fn negotiate_representation(
    headers: &[(String, String)],
) -> Result<ResponseRepresentation, ModernPostRejection> {
    let preferences = HttpResponsePreferences::from_headers(
        headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str())),
    )
    .map_err(|_| ModernPostRejection::NotAcceptable)?;
    Ok(match preferences.preferred() {
        HttpResponseRepresentation::Json => ResponseRepresentation::Json,
        HttpResponseRepresentation::Sse => ResponseRepresentation::RequestScopedSse,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::{HttpAdmissionLimits, HttpEndpointConfig, admit_modern_post};
    use fastmcp_protocol::FINAL_PROTOCOL_VERSION;
    use serde_json::json;

    const MAX_ACCEPT_MEMBERS: usize = 16;
    const MAX_ACCEPT_PARAMETERS: usize = 16;
    const MAX_IGNORED_ACCEPT_EMPTY_ELEMENTS: usize = 16;

    fn negotiate(values: &[&str]) -> Result<ResponseRepresentation, ModernPostRejection> {
        let fields = values.iter().map(|value| ("Accept".to_owned(), (*value).to_owned())).collect::<Vec<_>>();
        negotiate_representation(&fields)
    }

    fn headers(values: &[&str]) -> Vec<(String, String)> {
        let mut fields = vec![
            ("Content-Type".to_owned(), "application/json".to_owned()),
            ("MCP-Protocol-Version".to_owned(), FINAL_PROTOCOL_VERSION.to_owned()),
            ("Mcp-Method".to_owned(), "server/discover".to_owned()),
        ];
        fields.extend(values.iter().map(|value| ("aCcEpT".to_owned(), (*value).to_owned())));
        fields
    }

    fn config() -> HttpEndpointConfig {
        HttpEndpointConfig::new("/mcp", HttpAdmissionLimits::new(32, 8192, 65536).unwrap()).unwrap()
    }

    fn body() -> Vec<u8> {
        serde_json::to_vec(&json!({
            "jsonrpc":"2.0", "id":1, "method":"server/discover", "params":{
                "_meta":{
                    "io.modelcontextprotocol/protocolVersion":FINAL_PROTOCOL_VERSION,
                    "io.modelcontextprotocol/clientCapabilities":{}
                }
            }
        })).unwrap()
    }

    #[test]
    fn exact_exclusions_override_wildcards_in_either_order_or_field() {
        for fields in [
            vec!["application/json;q=0, */*;q=1"],
            vec!["*/*;q=1, application/json;q=0"],
            vec!["application/json;q=0", "*/*;q=1"],
            vec!["*/*;q=1", "application/json;q=0"],
            vec!["application/*;q=0, */*;q=1"],
        ] {
            assert_eq!(negotiate(&fields), Ok(ResponseRepresentation::RequestScopedSse));
        }
        assert_eq!(negotiate(&["*/*;q=1, application/json;q=0, text/event-stream;q=0"]),
            Err(ModernPostRejection::NotAcceptable));
        assert_eq!(negotiate(&["*/*;q=0, application/json;q=0.1"]), Ok(ResponseRepresentation::Json));
    }

    #[test]
    fn quality_selects_streaming_and_json_wins_only_ties() {
        assert_eq!(negotiate(&["application/json;q=0.1, text/event-stream;q=0.9"]),
            Ok(ResponseRepresentation::RequestScopedSse));
        assert_eq!(negotiate(&["text/*;q=0.7, */*;q=0.8, text/event-stream;q=0.9"]),
            Ok(ResponseRepresentation::RequestScopedSse));
        assert_eq!(negotiate(&["application/json;q=0.7, text/event-stream;q=0.7"]),
            Ok(ResponseRepresentation::Json));
        assert_eq!(negotiate(&[]), Ok(ResponseRepresentation::Json));
        assert_eq!(negotiate(&[""]), Err(ModernPostRejection::NotAcceptable));
    }

    #[test]
    fn duplicate_equal_specificity_cannot_resurrect_an_exclusion() {
        for fields in [vec!["application/json", "application/json;q=0"],
            vec!["application/json;q=0", "application/json"]] {
            assert_eq!(negotiate(&fields), Err(ModernPostRejection::NotAcceptable));
        }
        assert_eq!(negotiate(&["application/json, APPLICATION/JSON;q=0, text/event-stream"]),
            Ok(ResponseRepresentation::RequestScopedSse));
    }

    #[test]
    fn quality_grammar_is_exact_and_exhaustive_at_thousandth_precision() {
        for quality in 0_u16..1000 {
            let expected = if quality == 0 {
                Err(ModernPostRejection::NotAcceptable)
            } else {
                Ok(ResponseRepresentation::Json)
            };
            assert_eq!(negotiate(&[&format!("application/json;q=0.{quality:03}")]), expected);
        }
        for valid in ["1", "1.", "1.0", "1.00", "1.000"] {
            assert_eq!(negotiate(&[&format!("application/json;q={valid}")]),
                Ok(ResponseRepresentation::Json));
        }
        for invalid in ["", ".5", "00", "01", "1.001", "0.0001", "1.0000", "-0.1",
            "+0.5", "NaN", "inf", "1e-1", "2", "0.5.0", " 0.5", "\"0.5\""] {
            assert_eq!(negotiate(&[&format!("application/json;q={invalid}"), "text/event-stream"]),
                Err(ModernPostRejection::NotAcceptable));
        }
        assert_eq!(negotiate(&["application/json;q=1;Q=0"]), Err(ModernPostRejection::NotAcceptable));
    }

    #[test]
    fn quoted_delimiters_do_not_inject_supported_media_ranges() {
        for value in [
            r#"application/json;profile="x, text/event-stream;q=1""#,
            r#"application/json;profile="x; q=1""#,
            r#"application/json;profile="x\", text/event-stream;q=1""#,
        ] {
            assert_eq!(negotiate(&[value]), Err(ModernPostRejection::NotAcceptable));
            assert_eq!(negotiate(&[value, "text/event-stream"]), Ok(ResponseRepresentation::RequestScopedSse));
        }
        assert_eq!(negotiate(&[r#"application/json;profile="unterminated, text/event-stream"#]),
            Err(ModernPostRejection::NotAcceptable));
        for value in ["*/json", "application /json", "/json", "application/json/extra",
            "application/json;q =1", "application/json\r\n", "application/json;profile=\""] {
            assert_eq!(negotiate(&[value]), Err(ModernPostRejection::NotAcceptable));
        }
    }

    #[test]
    fn member_parameter_and_empty_element_limits_are_aggregate_and_exact() {
        let at_limit = vec!["application/json"; MAX_ACCEPT_MEMBERS].join(",");
        assert_eq!(negotiate(&[&at_limit]), Ok(ResponseRepresentation::Json));
        assert_eq!(negotiate(&[&at_limit, "text/event-stream"]), Err(ModernPostRejection::NotAcceptable));
        let empties = format!("{}application/json", ",".repeat(MAX_IGNORED_ACCEPT_EMPTY_ELEMENTS));
        assert_eq!(negotiate(&[&empties]), Ok(ResponseRepresentation::Json));
        assert_eq!(negotiate(&[&empties, ""]), Err(ModernPostRejection::NotAcceptable));
        let parameters = format!("application/json{}", ";".repeat(MAX_ACCEPT_PARAMETERS));
        assert_eq!(negotiate(&[&parameters]), Ok(ResponseRepresentation::Json));
        assert_eq!(negotiate(&[&format!("{parameters};")]), Err(ModernPostRejection::NotAcceptable));
    }

    #[test]
    fn public_post_admission_uses_negotiated_weights_without_changing_body() {
        let config = config();
        let body = body();
        let original = body.clone();
        let fields = headers(&["application/json;q=0", "*/*;q=0.8"]);
        let admitted = admit_modern_post(&config, "POST", "/mcp", &fields, &body).unwrap();
        assert_eq!(admitted.representation(), ResponseRepresentation::RequestScopedSse);
        assert_eq!(admitted.request().method, "server/discover");
        assert_eq!(body, original);
        let rejected = headers(&["application/json;q=0", "text/event-stream;q=0", "*/*;q=1"]);
        assert_eq!(admit_modern_post(&config, "POST", "/mcp", &rejected, &body).map(|_| ()),
            Err(ModernPostRejection::NotAcceptable));
    }

    #[test]
    fn invalid_accept_rejects_before_json_decode() {
        let fields = headers(&["application/json;q=NaN"]);
        assert_eq!(admit_modern_post(&config(), "POST", "/mcp", &fields, b"not JSON").map(|_| ()),
            Err(ModernPostRejection::NotAcceptable));
    }
}
