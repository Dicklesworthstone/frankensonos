use super::*;
use fsonos_api::ErrorCode;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::time::Duration;

fn parse(words: &[&str]) -> Result<ScheduleRequest, Failure> {
    super::super::schedule_request(
        "weekdays 08:15",
        &words
            .iter()
            .map(|word| (*word).to_owned())
            .collect::<Vec<_>>(),
    )
}

#[test]
fn speech_and_chimes_keep_the_immediate_command_options_in_a_nested_recipe() {
    let speech = parse(&[
        "say",
        "Time to leave — bring your coat",
        "--rooms",
        "Kitchen,Office",
        "--rooms",
        "Living Room@S1",
        "--voice",
        "Voice Name",
        "--volume",
        "25",
    ])
    .unwrap();
    assert_eq!(speech.action, "announce");
    assert_eq!(speech.when, "weekdays 08:15");
    assert!(speech.zone.is_none() && speech.volume.is_none());
    let nested = speech.announcement.as_ref().unwrap();
    assert_eq!(
        nested.text.as_deref(),
        Some("Time to leave — bring your coat")
    );
    assert_eq!(nested.voice.as_deref(), Some("Voice Name"));
    assert_eq!(nested.rooms, ["Kitchen", "Office", "Living Room@S1"]);
    assert_eq!(nested.volume, Some(25));
    let wire = serde_json::to_value(&speech).unwrap();
    assert_eq!(wire["announcement"]["volume"], 25);
    assert_eq!(
        serde_json::from_value::<ScheduleRequest>(wire).unwrap(),
        speech
    );

    let chime = parse(&["chime", "bell", "--room", "Office@S2", "--volume", "0"]).unwrap();
    let nested = chime.announcement.unwrap();
    assert_eq!(nested.chime.as_deref(), Some("bell"));
    assert!(nested.text.is_none() && nested.voice.is_none());
    assert_eq!(nested.rooms, ["Office@S2"]);
    assert_eq!(nested.volume, Some(0));

    let all = parse(&["say", "Dinner is ready"])
        .unwrap()
        .announcement
        .unwrap();
    assert!(all.rooms.is_empty());
    assert!(all.volume.is_none());
}

#[test]
fn invalid_scheduled_speech_and_chimes_fail_before_daemon_selection() {
    for words in [
        vec!["say"],
        vec!["say", "   "],
        vec!["say", "hello", "--volume", "101"],
        vec!["say", "hello", "--voice", ""],
        vec!["say", "hello", "--file", "clip.wav"],
        vec!["say", "hello", "--mood", "calm"],
        vec!["chime", "not-a-chime"],
        vec!["chime", "bell", "--voice", "Voice Name"],
        vec!["chime", "bell", "--volume", "-1"],
        vec!["chime", "bell", "--rooms", ""],
    ] {
        assert_eq!(
            parse(&words).unwrap_err().code,
            ErrorCode::InvalidArgument,
            "{words:?}"
        );
    }
    let long = "a".repeat(1001);
    assert_eq!(
        parse(&["say", &long]).unwrap_err().code,
        ErrorCode::InvalidArgument
    );
}

fn read_request(stream: &mut std::net::TcpStream) -> (String, Value) {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(stream);
    let mut first = String::new();
    reader.read_line(&mut first).unwrap();
    let mut length = 0;
    loop {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).unwrap() > 0);
        if line == "\r\n" {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().unwrap();
        }
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).unwrap();
    (
        first,
        if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        },
    )
}

#[test]
fn a_url_selected_daemon_receives_the_complete_scheduled_announcement_once() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let worker = std::thread::spawn(move || {
        let mut received = Value::Null;
        for index in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let (first, body) = read_request(&mut stream);
            let reply = if index == 0 {
                assert!(first.starts_with("GET /health "));
                json!({"status":"ok"})
            } else {
                assert!(first.starts_with("POST /schedules "));
                received = body.clone();
                json!({"id":17,"when":body["when"],"action":"announce",
                    "announcement":body["announcement"],"summary":"speak in Kitchen, Office",
                    "creator":"loopback-http","enabled":true})
            };
            let response = serde_json::to_string(&reply).unwrap();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
        }
        received
    });
    let cli = crate::Cli::try_parse_from([
        "fsonos",
        "--daemon-url",
        &url,
        "--json",
        "schedule",
        "add",
        "weekdays 08:15",
        "say",
        "Time to leave",
        "--rooms",
        "Kitchen,Office",
        "--volume",
        "25",
        "--voice",
        "Voice Name",
    ])
    .unwrap();
    let crate::Command::Schedule { action } = cli.command else {
        panic!("schedule command");
    };
    super::super::schedule(&cli.global, &action).unwrap();
    let body = worker.join().unwrap();
    assert_eq!(body["when"], "weekdays 08:15");
    assert_eq!(body["action"], "announce");
    assert_eq!(
        body["announcement"],
        json!({
            "text":"Time to leave", "chime":null, "wav_base64":null, "voice":"Voice Name",
            "rooms":["Kitchen","Office"], "volume":25,
        })
    );
    assert!(body["zone"].is_null() && body["volume"].is_null());
}
