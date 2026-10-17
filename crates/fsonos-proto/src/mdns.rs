//! mDNS / DNS-SD discovery (UDP multicast `224.0.0.251:5353`).
//!
//! A second, independent discovery channel next to SSDP — the players' own
//! daemon reconciles SSDP and mDNS sightings, and so do we. Verified live on
//! the owner's LAN (2026-10-07): S2 players advertise
//! `RINCON_<UUID>01400@<Room>` with TXT `uuid=`, `hhid=`, `mhhid=`,
//! `bootseq=`, `location=`, `sslport=1443`, `wss=/websocket/api`; S1 players
//! advertise `Sonos-<MAC>` with a minimal TXT (`info=`, `vers=1`,
//! `protovers=`). Bridges do not advertise. Parsing is pure here; the socket
//! lives in [`crate::net`].

use crate::ProtoError;
use std::net::Ipv4Addr;

/// The mDNS multicast group and port.
pub const MDNS_ADDR: &str = "224.0.0.251:5353";

/// The DNS-SD service type Sonos players advertise.
pub const SONOS_SERVICE: &str = "_sonos._tcp.local";

/// A multicast-QU PTR question for [`SONOS_SERVICE`]: `id` 0, recursion
/// off, one question, the unicast-response (QU) class bit set. Both
/// generations answer this pattern (S1 players ignore raw ephemeral-port
/// probes; the capture notes in `docs/PROTOCOL.md` §1 record the
/// difference), so it is the one query [`crate::net::Lan::mdns_search`]
/// sends.
#[must_use]
pub fn query() -> Vec<u8> {
    let mut b = Vec::with_capacity(12 + SONOS_SERVICE.len() + 2 + 6);
    // Header: id 0, flags 0 (query), 1 question, nothing else.
    b.extend_from_slice(&[0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
    for label in SONOS_SERVICE.split('.') {
        b.push(u8::try_from(label.len()).expect("service labels are short"));
        b.extend_from_slice(label.as_bytes());
    }
    b.push(0);
    b.extend_from_slice(&TYPE_PTR.to_be_bytes());
    // QU (top bit) + class IN: the responder may unicast its reply.
    b.extend_from_slice(&0x8001u16.to_be_bytes());
    b
}

/// A resource record's payload, decoded for the types Sonos uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordData {
    /// PTR: the instance name (e.g. `RINCON_…01400@Kitchen Counter`).
    Ptr(String),
    /// SRV: port and target host.
    Srv { port: u16, target: String },
    /// TXT: the raw `key=value` (or bare `key`) strings, in order.
    Txt(Vec<String>),
    /// A: the host's IPv4 address.
    A(Ipv4Addr),
    /// Any other record type: ignored on purpose.
    Other,
}

/// One resource record from the answer/authority/additional sections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub name: String,
    pub rr_type: u16,
    pub ttl: u32,
    pub data: RecordData,
}

/// A parsed DNS message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnsMessage {
    pub id: u16,
    pub is_response: bool,
    /// Questions as (name, qtype).
    pub questions: Vec<(String, u16)>,
    pub records: Vec<Record>,
}

const TYPE_A: u16 = 1;
const TYPE_PTR: u16 = 12;
const TYPE_TXT: u16 = 16;
const TYPE_SRV: u16 = 33;

fn malformed(msg: &str) -> ProtoError {
    ProtoError::Malformed(format!("mdns: {msg}"))
}

/// Read a possibly-compressed domain name at `pos`. Returns the name and the
/// position AFTER it (compression pointers are followed but not consumed).
/// Pointer hops are bounded (against loops) separately from ordinary labels:
/// a valid name may have far more than 32 labels; its real bound is the
/// 255-octet expanded wire length (RFC 1035 §2.3.4).
fn read_name(bytes: &[u8], mut pos: usize) -> Result<(String, usize), ProtoError> {
    const MAX_POINTERS: usize = 32;
    const MAX_NAME_WIRE: usize = 255;
    let mut labels: Vec<String> = Vec::new();
    let mut end: Option<usize> = None;
    let mut pointers = 0;
    // Expanded length so far: sum of (length octet + label) per label, plus
    // the terminating root octet at the end.
    let mut wire_len = 0usize;
    loop {
        let &len = bytes.get(pos).ok_or_else(|| malformed("truncated name"))?;
        if len == 0 {
            wire_len += 1;
            if wire_len > MAX_NAME_WIRE {
                return Err(malformed("name too long"));
            }
            let next = pos + 1;
            return Ok((labels.join("."), end.unwrap_or(next)));
        }
        if len & 0xC0 == 0xC0 {
            // Two-byte compression pointer.
            let &b2 = bytes
                .get(pos + 1)
                .ok_or_else(|| malformed("truncated pointer"))?;
            let offset = (usize::from(len & 0x3F) << 8) | usize::from(b2);
            if offset >= bytes.len() {
                return Err(malformed("pointer out of bounds"));
            }
            pointers += 1;
            if pointers > MAX_POINTERS {
                return Err(malformed("name compression loop"));
            }
            if end.is_none() {
                end = Some(pos + 2);
            }
            pos = offset;
            continue;
        }
        if len & 0xC0 != 0 {
            return Err(malformed("bad label length bits"));
        }
        let len = usize::from(len);
        let text = bytes
            .get(pos + 1..pos + 1 + len)
            .ok_or_else(|| malformed("truncated label"))?;
        labels.push(String::from_utf8_lossy(text).into_owned());
        wire_len += 1 + len;
        if wire_len > MAX_NAME_WIRE {
            return Err(malformed("name too long"));
        }
        pos += 1 + len;
    }
}

fn u16_at(bytes: &[u8], pos: usize) -> Result<u16, ProtoError> {
    let b = bytes
        .get(pos..pos + 2)
        .ok_or_else(|| malformed("truncated u16"))?;
    Ok(u16::from_be_bytes([b[0], b[1]]))
}

fn u32_at(bytes: &[u8], pos: usize) -> Result<u32, ProtoError> {
    let b = bytes
        .get(pos..pos + 4)
        .ok_or_else(|| malformed("truncated u32"))?;
    Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// Parse a whole DNS message (query or response), all four sections' records
/// flattened into `records` in wire order.
pub fn parse_message(bytes: &[u8]) -> Result<DnsMessage, ProtoError> {
    if bytes.len() < 12 {
        return Err(malformed("truncated header"));
    }
    let id = u16_at(bytes, 0)?;
    let flags = u16_at(bytes, 2)?;
    let qd = usize::from(u16_at(bytes, 4)?);
    let an = usize::from(u16_at(bytes, 6)?);
    let ns = usize::from(u16_at(bytes, 8)?);
    let ar = usize::from(u16_at(bytes, 10)?);
    let mut pos = 12;
    // Clamp count-derived preallocation by what the remaining bytes could
    // plausibly hold: a question takes at least 5 octets (root name, qtype,
    // qclass), a record at least 11 (root name plus the fixed fields), so a
    // 12-byte packet with ANCOUNT 65535 cannot reserve 65535 slots.
    let plausible_questions = (bytes.len() - pos) / 5;
    let mut questions = Vec::with_capacity(qd.min(plausible_questions));
    for _ in 0..qd {
        let (name, next) = read_name(bytes, pos)?;
        let qtype = u16_at(bytes, next)?;
        let _qclass = u16_at(bytes, next + 2)?;
        questions.push((name, qtype));
        pos = next + 4;
    }
    let plausible_records = (bytes.len() - pos) / 11;
    let mut records = Vec::with_capacity((an + ns + ar).min(plausible_records));
    for _ in 0..(an + ns + ar) {
        let (name, next) = read_name(bytes, pos)?;
        let rr_type = u16_at(bytes, next)?;
        let _class = u16_at(bytes, next + 2)?;
        let ttl = u32_at(bytes, next + 4)?;
        let rdlen = usize::from(u16_at(bytes, next + 8)?);
        let rstart = next + 10;
        let rend = rstart + rdlen;
        if rend > bytes.len() {
            return Err(malformed("truncated rdata"));
        }
        let data = match rr_type {
            TYPE_A if rdlen == 4 => RecordData::A(Ipv4Addr::new(
                bytes[rstart],
                bytes[rstart + 1],
                bytes[rstart + 2],
                bytes[rstart + 3],
            )),
            // PTR and SRV carry names: compression pointers inside them may
            // reference anywhere in the datagram, but the name's own octets
            // must exactly fill the RDATA — a shorter read (rdlen 0 included)
            // or a longer one means the record is malformed, and accepting it
            // would consume the next record's bytes or leave padding behind.
            TYPE_PTR => {
                let (name, consumed) = read_name(bytes, rstart)?;
                if consumed != rend {
                    return Err(malformed("PTR rdata length mismatch"));
                }
                RecordData::Ptr(name)
            }
            TYPE_SRV => {
                if rdlen < 7 {
                    return Err(malformed("truncated SRV"));
                }
                let port = u16_at(bytes, rstart + 4)?;
                let (target, consumed) = read_name(bytes, rstart + 6)?;
                if consumed != rend {
                    return Err(malformed("SRV rdata length mismatch"));
                }
                RecordData::Srv { port, target }
            }
            TYPE_TXT => {
                // Character-strings are length-prefixed and never compressed;
                // one must end exactly at the RDATA end, never cross it.
                let mut txts = Vec::new();
                let mut t = rstart;
                while t < rend {
                    let slen = usize::from(bytes[t]);
                    let send = t + 1 + slen;
                    if send > rend {
                        return Err(malformed("TXT string crosses rdata end"));
                    }
                    txts.push(String::from_utf8_lossy(&bytes[t + 1..send]).into_owned());
                    t = send;
                }
                RecordData::Txt(txts)
            }
            _ => RecordData::Other,
        };
        records.push(Record {
            name,
            rr_type,
            ttl,
            data,
        });
        pos = rend;
    }
    Ok(DnsMessage {
        id,
        is_response: flags & 0x8000 != 0,
        questions,
        records,
    })
}

/// One Sonos player advertisement, assembled from a message's records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SonosAdvert {
    /// The PTR instance name, e.g. `RINCON_…01400@Kitchen Counter` (S2 style)
    /// or `Sonos-<MAC>` (S1 style).
    pub instance: String,
    /// Player UUID: TXT `uuid=`, else parsed from the instance name.
    pub uuid: Option<String>,
    /// Household ID from TXT `hhid=` (S2 only; S1 omits it).
    pub household: Option<String>,
    /// TXT `bootseq=` — increments when the player reboots.
    pub boot_seq: Option<u32>,
    /// TXT `location=` — the device-description URL.
    pub location: Option<String>,
    /// SRV port (1443 on both generations observed).
    pub port: Option<u16>,
    /// The host's IPv4 address from the A record, if present.
    pub addr: Option<Ipv4Addr>,
}

/// Extract Sonos player advertisements from one response. For a discovery
/// window whose replies span multiple datagrams, use
/// [`sonos_adverts_from_records`] after collecting their resource records.
#[must_use]
pub fn sonos_adverts(msg: &DnsMessage) -> Vec<SonosAdvert> {
    if !msg.is_response {
        return Vec::new();
    }
    sonos_adverts_from_records(&msg.records)
}

/// Resolve the PTR, SRV, TXT and A records collected during a discovery
/// window. DNS-SD is not obliged to send those records in one datagram, or
/// in any particular order. A PTR-only response can therefore be completed
/// by SRV/TXT/A records from subsequent responses.
///
/// Records apply in wire order with latest-record-wins semantics. A TTL-zero
/// goodbye never resurrects a value: a PTR goodbye removes the player, and a
/// SRV/TXT/A goodbye invalidates exactly that value (no fallback to an older,
/// staler record for the same name and type). A later alive record — a new
/// PTR for the player, or a fresh SRV/TXT/A — rediscovers or repopulates it.
#[must_use]
pub fn sonos_adverts_from_records(records: &[Record]) -> Vec<SonosAdvert> {
    // The latest record (in wire order) for one name and type; the caller
    // checks whether it is alive or a TTL-zero goodbye.
    fn latest<'a>(records: &'a [Record], name: &str, rr_type: u16) -> Option<&'a Record> {
        records
            .iter()
            .rev()
            .find(|r| r.rr_type == rr_type && r.name.eq_ignore_ascii_case(name))
    }

    let mut out: Vec<SonosAdvert> = Vec::new();
    for rec in records {
        let RecordData::Ptr(instance) = &rec.data else {
            continue;
        };
        if !rec.name.eq_ignore_ascii_case(SONOS_SERVICE) {
            continue;
        }
        let bare = strip_service_domain(instance);
        if rec.ttl == 0 {
            out.retain(|ad| !ad.instance.eq_ignore_ascii_case(bare));
            continue;
        }
        if out.iter().any(|ad| ad.instance.eq_ignore_ascii_case(bare)) {
            continue;
        }
        let mut advert = SonosAdvert {
            instance: bare.to_string(),
            uuid: None,
            household: None,
            boot_seq: None,
            location: None,
            port: None,
            addr: None,
        };

        // mDNS additional records can precede the PTR, or arrive in another
        // packet. Locate the SRV target first, then its A record, without
        // relying on the order in which either appeared on the wire. A
        // goodbye as the latest record of either kind leaves the value empty.
        if let Some(srv) = latest(records, instance, TYPE_SRV)
            && srv.ttl > 0
            && let RecordData::Srv { port, target } = &srv.data
        {
            advert.port = Some(*port);
            if let Some(a) = latest(records, target, TYPE_A)
                && a.ttl > 0
                && let RecordData::A(addr) = &a.data
            {
                advert.addr = Some(*addr);
            }
        }
        if let Some(txt) = latest(records, instance, TYPE_TXT)
            && txt.ttl > 0
            && let RecordData::Txt(txts) = &txt.data
        {
            for t in txts {
                if let Some((key, value)) = t.split_once('=') {
                    match key.to_ascii_lowercase().as_str() {
                        "uuid" => advert.uuid = Some(value.to_string()),
                        "hhid" => advert.household = Some(value.to_string()),
                        "bootseq" => advert.boot_seq = value.parse().ok(),
                        "location" => advert.location = Some(value.to_string()),
                        _ => {}
                    }
                }
            }
        }
        if advert.uuid.is_none() {
            advert.uuid = uuid_from_instance(&advert.instance);
        }
        out.push(advert);
    }
    out
}

/// Strip the `._sonos._tcp.local` service suffix from an instance FQDN
/// (case-insensitive), leaving the bare instance name dns-sd displays.
/// Compares on bytes: slicing the `&str` at a computed offset could land
/// inside a multi-byte UTF-8 character of a non-ASCII room name and panic.
/// The suffix starts with ASCII `.`, so whenever it matches, the cut is a
/// char boundary.
fn strip_service_domain(name: &str) -> &str {
    const SUFFIX: &str = "._sonos._tcp.local";
    let bytes = name.as_bytes();
    if bytes.len() > SUFFIX.len()
        && bytes[bytes.len() - SUFFIX.len()..].eq_ignore_ascii_case(SUFFIX.as_bytes())
    {
        &name[..name.len() - SUFFIX.len()]
    } else {
        name
    }
}

/// The `RINCON_<12-hex>01400` UUID embedded in either bare instance style:
/// `RINCON_<UUID>01400@<room>` (S2) or `Sonos-<MAC>` (S1).
#[must_use]
pub fn uuid_from_instance(instance: &str) -> Option<String> {
    if let Some(rest) = instance.strip_prefix("RINCON_") {
        return rest.split('@').next().map(|u| format!("RINCON_{u}"));
    }
    if let Some(rest) = instance.strip_prefix("Sonos-") {
        let mac: String = rest.chars().take(12).collect();
        if mac.len() == 12 && mac.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(format!("RINCON_{}01400", mac.to_uppercase()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal response with one Sonos PTR + SRV/TXT/A additional
    /// records, exercising compression (the service name repeats).
    #[allow(clippy::cast_possible_truncation)] // test packets: every length fits
    fn build_response(instance: &str, txts: &[&str], port: u16, ip: [u8; 4]) -> Vec<u8> {
        let mut b = Vec::new();
        // header: id 0, response, 0 questions, 1 answer, 0 authority, 3 additional
        b.extend_from_slice(&[0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 3]);
        let svc = b"_sonos._tcp.local";
        let put_name = |b: &mut Vec<u8>, name: &str| {
            for label in name.split('.') {
                b.push(label.len() as u8);
                b.extend_from_slice(label.as_bytes());
            }
            b.push(0);
        };
        // answer: PTR _sonos._tcp.local -> instance
        put_name(&mut b, std::str::from_utf8(svc).unwrap());
        b.extend_from_slice(&TYPE_PTR.to_be_bytes());
        b.extend_from_slice(&0x8001u16.to_be_bytes()); // cache-flush class IN
        b.extend_from_slice(&4500u32.to_be_bytes());
        let rlen_pos = b.len();
        b.extend_from_slice(&[0, 0]);
        let rstart = b.len();
        put_name(&mut b, instance);
        let rdlen = (b.len() - rstart) as u16;
        b[rlen_pos] = (rdlen >> 8) as u8;
        b[rlen_pos + 1] = rdlen as u8;
        // additional: SRV on instance (name via pointer to its first occurrence)
        // find instance offset in packet
        let first_label = instance.split('.').next().unwrap();
        let inst_off = b
            .windows(first_label.len())
            .position(|w| w == first_label.as_bytes())
            .unwrap()
            - 1;
        let put_ptr = |b: &mut Vec<u8>| {
            let ptr = 0xC000u16 | (inst_off as u16);
            b.extend_from_slice(&ptr.to_be_bytes());
        };
        // SRV
        put_ptr(&mut b);
        b.extend_from_slice(&TYPE_SRV.to_be_bytes());
        b.extend_from_slice(&0x8001u16.to_be_bytes());
        b.extend_from_slice(&4500u32.to_be_bytes());
        let target = "sonos000E58A00001.local";
        let mut rdata = Vec::new();
        rdata.extend_from_slice(&[0, 0, 0, 0]);
        rdata.extend_from_slice(&port.to_be_bytes());
        for label in target.split('.') {
            rdata.push(label.len() as u8);
            rdata.extend_from_slice(label.as_bytes());
        }
        rdata.push(0);
        b.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        b.extend_from_slice(&rdata);
        // TXT
        put_ptr(&mut b);
        b.extend_from_slice(&TYPE_TXT.to_be_bytes());
        b.extend_from_slice(&0x8001u16.to_be_bytes());
        b.extend_from_slice(&4500u32.to_be_bytes());
        let mut rdata = Vec::new();
        for t in txts {
            rdata.push(t.len() as u8);
            rdata.extend_from_slice(t.as_bytes());
        }
        b.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        b.extend_from_slice(&rdata);
        // A on target host: point at the SRV target's first label (unique —
        // searching bare "sonos" would hit _sonos._tcp.local instead).
        let host_label = target.split('.').next().unwrap();
        let host_off = b
            .windows(host_label.len())
            .rposition(|w| w == host_label.as_bytes())
            .unwrap()
            - 1;
        b.extend_from_slice(&(0xC000u16 | (host_off as u16)).to_be_bytes());
        b.extend_from_slice(&TYPE_A.to_be_bytes());
        b.extend_from_slice(&0x8001u16.to_be_bytes());
        b.extend_from_slice(&120u32.to_be_bytes());
        b.extend_from_slice(&4u16.to_be_bytes());
        b.extend_from_slice(&ip);
        b
    }

    #[test]
    fn query_is_the_standard_multicast_qu_ptr_question() {
        let bytes = query();
        // Header (id 0, query, one question), `_sonos._tcp.local`,
        // QTYPE PTR, QCLASS QU+IN — byte for byte.
        let expected: Vec<u8> = [
            &b"\x00\x00\x00\x00\x00\x01\x00\x00\x00\x00\x00\x00"[..],
            b"\x06_sonos\x04_tcp\x05local\x00",
            &b"\x00\x0c\x80\x01"[..],
        ]
        .concat();
        assert_eq!(bytes, expected);
    }

    #[test]
    fn parses_response_with_compression() {
        let bytes = build_response(
            "RINCON_000E58A0000101400@Kitchen",
            &[
                "uuid=RINCON_000E58A0000101400",
                "hhid=Sonos_EXAMPLEHH",
                "bootseq=39",
            ],
            1443,
            [192, 0, 2, 22],
        );
        let msg = parse_message(&bytes).unwrap();
        assert!(msg.is_response);
        assert_eq!(msg.records.len(), 4);
        let ads = sonos_adverts(&msg);
        assert_eq!(ads.len(), 1);
        let ad = &ads[0];
        assert_eq!(ad.instance, "RINCON_000E58A0000101400@Kitchen");
        assert_eq!(ad.uuid.as_deref(), Some("RINCON_000E58A0000101400"));
        assert_eq!(ad.household.as_deref(), Some("Sonos_EXAMPLEHH"));
        assert_eq!(ad.boot_seq, Some(39));
        assert_eq!(ad.port, Some(1443));
        assert_eq!(ad.addr, Some(Ipv4Addr::new(192, 0, 2, 22)));
    }

    #[test]
    fn derives_uuid_from_either_instance_style() {
        assert_eq!(
            uuid_from_instance("RINCON_000E58A0000101400@Kitchen"),
            Some("RINCON_000E58A0000101400".into())
        );
        assert_eq!(
            uuid_from_instance("Sonos-000E58A00001"),
            Some("RINCON_000E58A0000101400".into())
        );
        assert_eq!(uuid_from_instance("something-else"), None);
    }

    #[test]
    fn records_in_separate_packets_and_in_any_order_complete_the_player() {
        let wire = build_response(
            "RINCON_000E58A0000101400@Kitchen",
            &["uuid=RINCON_000E58A0000101400", "hhid=TEST_HOUSE"],
            1443,
            [192, 0, 2, 22],
        );
        let response = parse_message(&wire).unwrap();
        // The A and TXT replies arrive first; a PTR without SRV/TXT/A
        // cannot yet supply an address for inventory's description fetch.
        let first = DnsMessage {
            records: vec![response.records[3].clone(), response.records[2].clone()],
            ..response.clone()
        };
        let second = DnsMessage {
            records: vec![response.records[0].clone(), response.records[1].clone()],
            ..response.clone()
        };
        assert!(sonos_adverts(&first).is_empty());
        assert_eq!(sonos_adverts(&second)[0].addr, None);
        let records: Vec<Record> = first
            .records
            .iter()
            .chain(&second.records)
            .cloned()
            .collect();
        let adverts = sonos_adverts_from_records(&records);
        assert_eq!(adverts.len(), 1);
        assert_eq!(adverts[0].addr, Some(Ipv4Addr::new(192, 0, 2, 22)));
        assert_eq!(adverts[0].household.as_deref(), Some("TEST_HOUSE"));
        assert_eq!(adverts[0].port, Some(1443));
    }

    #[test]
    fn dns_names_are_case_insensitive_and_goodbyes_are_ignored() {
        let wire = build_response(
            "Sonos-000E58A00001._sonos._tcp.local",
            &["vers=1"],
            1443,
            [192, 0, 2, 11],
        );
        let mut response = parse_message(&wire).unwrap();
        response.records[0].name.make_ascii_uppercase();
        response.records[1].name.make_ascii_uppercase();
        response.records[3].name.make_ascii_uppercase();
        let adverts = sonos_adverts(&response);
        assert_eq!(adverts[0].addr, Some(Ipv4Addr::new(192, 0, 2, 11)));
        response.records[0].ttl = 0;
        assert!(sonos_adverts(&response).is_empty());
        response.records[0].ttl = 4500;
        response.is_response = false;
        assert!(sonos_adverts(&response).is_empty());
    }

    #[test]
    fn goodbye_removes_an_earlier_alive_and_a_new_alive_rediscovers() {
        let wire = build_response("Sonos-000E58A00001", &["vers=1"], 1443, [192, 0, 2, 11]);
        let response = parse_message(&wire).unwrap();
        let mut records = response.records.clone();
        assert_eq!(sonos_adverts_from_records(&records).len(), 1);

        let mut goodbye = response.records[0].clone();
        goodbye.ttl = 0;
        records.push(goodbye);
        assert!(sonos_adverts_from_records(&records).is_empty());

        records.push(response.records[0].clone());
        let adverts = sonos_adverts_from_records(&records);
        assert_eq!(adverts.len(), 1);
        assert_eq!(adverts[0].addr, Some(Ipv4Addr::new(192, 0, 2, 11)));
    }

    #[test]
    fn rejects_truncated_messages() {
        assert!(parse_message(&[0u8; 5]).is_err());
        let mut good = build_response("Sonos-000E58A00001", &["vers=1"], 1443, [192, 0, 2, 11]);
        assert!(parse_message(&good).is_ok());
        good.truncate(good.len() - 3);
        assert!(parse_message(&good).is_err());
    }

    #[test]
    fn non_ascii_instance_names_never_panic_in_suffix_strip() {
        // Here the would-be suffix cut point lands inside the two-byte 'é'
        // (byte 16 of 33): comparing on bytes instead of slicing the str at
        // a computed offset keeps this from panicking, and the non-matching
        // suffix leaves the instance untouched.
        let inside = format!("{}é{}", "a".repeat(15), "b".repeat(16));
        let records = vec![Record {
            name: SONOS_SERVICE.to_string(),
            rr_type: TYPE_PTR,
            ttl: 4500,
            data: RecordData::Ptr(inside.clone()),
        }];
        let adverts = sonos_adverts_from_records(&records);
        assert_eq!(adverts.len(), 1);
        assert_eq!(adverts[0].instance, inside);

        // A non-ASCII room name that genuinely carries the suffix strips.
        let records = vec![Record {
            name: SONOS_SERVICE.to_string(),
            rr_type: TYPE_PTR,
            ttl: 4500,
            data: RecordData::Ptr("Küche._sonos._tcp.local".to_string()),
        }];
        assert_eq!(sonos_adverts_from_records(&records)[0].instance, "Küche");
    }

    /// A response header announcing `an` answers.
    fn response_header(an: u16) -> Vec<u8> {
        let mut b = vec![0, 0, 0x84, 0, 0, 0];
        b.extend_from_slice(&an.to_be_bytes());
        b.extend_from_slice(&[0, 0, 0, 0]);
        b
    }

    #[test]
    fn zero_rdlength_ptr_cannot_consume_the_next_record() {
        let mut b = response_header(2);
        // Answer 1: PTR with RDLENGTH 0 — a name is at least the root octet,
        // so this is malformed. A decoder that ignores RDLENGTH would read
        // the next record's bytes as the PTR target.
        b.push(0); // root name
        b.extend_from_slice(&TYPE_PTR.to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&0u16.to_be_bytes());
        // Answer 2: a valid A record the greedy read would swallow.
        b.push(0);
        b.extend_from_slice(&TYPE_A.to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&120u32.to_be_bytes());
        b.extend_from_slice(&4u16.to_be_bytes());
        b.extend_from_slice(&[192, 0, 2, 1]);
        assert!(parse_message(&b).is_err());
    }

    #[test]
    fn ptr_and_srv_names_must_end_exactly_at_rdlength() {
        // PTR rdata: the root name (1 octet) plus 2 octets of padding that
        // RDLENGTH wrongly claims.
        let mut b = response_header(1);
        b.push(0);
        b.extend_from_slice(&TYPE_PTR.to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&4500u32.to_be_bytes());
        b.extend_from_slice(&3u16.to_be_bytes());
        b.extend_from_slice(&[0, 0xAA, 0xBB]);
        assert!(parse_message(&b).is_err());

        // SRV rdata: 6 fixed octets plus a root-name target (1 octet), but
        // RDLENGTH claims one more.
        let mut b = response_header(1);
        b.push(0);
        b.extend_from_slice(&TYPE_SRV.to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&4500u32.to_be_bytes());
        b.extend_from_slice(&8u16.to_be_bytes());
        b.extend_from_slice(&[0, 0, 0, 0, 0x05, 0x67, 0, 0xAA]);
        assert!(parse_message(&b).is_err());
    }

    #[test]
    fn ptr_rdata_may_compress_into_the_datagram() {
        // The question holds the service name at offset 12; the PTR's target
        // is one label plus a compression pointer back to it, exactly filling
        // its RDLENGTH.
        let mut b = vec![0, 0, 0x84, 0, 0, 1, 0, 1, 0, 0, 0, 0];
        let qname_off = u16::try_from(b.len()).unwrap();
        for label in ["_sonos", "_tcp", "local"] {
            b.push(u8::try_from(label.len()).unwrap());
            b.extend_from_slice(label.as_bytes());
        }
        b.push(0);
        b.extend_from_slice(&TYPE_PTR.to_be_bytes());
        b.extend_from_slice(&0x8001u16.to_be_bytes());
        let ptr = 0xC000u16 | qname_off;
        b.extend_from_slice(&ptr.to_be_bytes()); // answer name
        b.extend_from_slice(&TYPE_PTR.to_be_bytes());
        b.extend_from_slice(&0x8001u16.to_be_bytes());
        b.extend_from_slice(&4500u32.to_be_bytes());
        b.extend_from_slice(&7u16.to_be_bytes()); // 1+4 label + 2 pointer
        b.push(4);
        b.extend_from_slice(b"Inst");
        b.extend_from_slice(&ptr.to_be_bytes());
        let msg = parse_message(&b).unwrap();
        assert_eq!(
            msg.records[0].data,
            RecordData::Ptr("Inst._sonos._tcp.local".to_string())
        );
    }

    #[test]
    fn txt_strings_must_not_cross_the_rdata_end() {
        // The string claims 5 octets; RDLENGTH leaves room for 1, and the
        // bytes a greedy read would swallow belong to the following A record.
        let mut b = response_header(2);
        b.push(0);
        b.extend_from_slice(&TYPE_TXT.to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&4500u32.to_be_bytes());
        b.extend_from_slice(&2u16.to_be_bytes());
        b.extend_from_slice(&[5, b'a']);
        b.push(0);
        b.extend_from_slice(&TYPE_A.to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&120u32.to_be_bytes());
        b.extend_from_slice(&4u16.to_be_bytes());
        b.extend_from_slice(&[192, 0, 2, 1]);
        assert!(parse_message(&b).is_err());
    }

    #[test]
    fn absurd_counts_in_a_tiny_packet_error_without_huge_allocation() {
        // A bare 12-byte header claiming ~196K records: preallocation is
        // clamped by what the remaining bytes could plausibly hold (zero
        // records here), and the parse errors on the missing record bytes.
        let mut b = vec![
            0, 0, 0x84, 0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
        ];
        assert!(parse_message(&b).is_err());
        b.extend_from_slice(&[0u8; 64]);
        assert!(parse_message(&b).is_err());
    }

    #[test]
    fn names_may_have_more_labels_than_the_pointer_hop_limit() {
        // 33 one-character labels: 67 wire octets, well within the 255-octet
        // name bound, but more label reads than the old 32-hop loop limit
        // (which counted labels as hops) allowed.
        let mut b = response_header(1);
        b.push(0);
        b.extend_from_slice(&TYPE_PTR.to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&4500u32.to_be_bytes());
        b.extend_from_slice(&67u16.to_be_bytes());
        for _ in 0..33 {
            b.push(1);
            b.push(b'a');
        }
        b.push(0);
        let msg = parse_message(&b).unwrap();
        let expected: String = (0..33).map(|_| "a").collect::<Vec<_>>().join(".");
        assert_eq!(msg.records[0].data, RecordData::Ptr(expected));
    }

    #[test]
    fn names_beyond_255_wire_octets_are_rejected() {
        // Four 63-octet labels: 4*64 + 1 = 257 wire octets.
        let mut b = response_header(1);
        b.push(0);
        b.extend_from_slice(&TYPE_PTR.to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&4500u32.to_be_bytes());
        b.extend_from_slice(&257u16.to_be_bytes());
        for _ in 0..4 {
            b.push(63);
            b.extend_from_slice(&[b'a'; 63]);
        }
        b.push(0);
        assert!(parse_message(&b).is_err());
    }

    #[test]
    fn a_compression_pointer_loop_is_rejected() {
        // The record name is a pointer to its own offset.
        let mut b = response_header(1);
        let self_off = u16::try_from(b.len()).unwrap();
        b.extend_from_slice(&(0xC000u16 | self_off).to_be_bytes());
        b.extend_from_slice(&TYPE_A.to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&120u32.to_be_bytes());
        b.extend_from_slice(&4u16.to_be_bytes());
        b.extend_from_slice(&[192, 0, 2, 1]);
        assert!(parse_message(&b).is_err());
    }

    #[test]
    fn an_a_goodbye_invalidates_the_address_without_removing_the_player() {
        let wire = build_response("Sonos-000E58A00001", &["vers=1"], 1443, [192, 0, 2, 11]);
        let response = parse_message(&wire).unwrap();
        let mut records = response.records.clone();
        // A newer alive A, then an A goodbye: latest wins, so the address is
        // gone — it must not fall back to either older A record.
        let mut newer_a = response.records[3].clone();
        newer_a.data = RecordData::A(Ipv4Addr::new(192, 0, 2, 12));
        records.push(newer_a);
        let mut goodbye_a = response.records[3].clone();
        goodbye_a.ttl = 0;
        records.push(goodbye_a);
        let adverts = sonos_adverts_from_records(&records);
        assert_eq!(adverts.len(), 1);
        assert_eq!(adverts[0].port, Some(1443));
        assert_eq!(adverts[0].addr, None);
        // A fresh A after the goodbye repopulates the address.
        let mut alive_a = response.records[3].clone();
        alive_a.data = RecordData::A(Ipv4Addr::new(192, 0, 2, 13));
        records.push(alive_a);
        assert_eq!(
            sonos_adverts_from_records(&records)[0].addr,
            Some(Ipv4Addr::new(192, 0, 2, 13))
        );
    }

    #[test]
    fn an_srv_goodbye_invalidates_port_and_address_until_a_fresh_srv() {
        let wire = build_response("Sonos-000E58A00001", &["vers=1"], 1443, [192, 0, 2, 11]);
        let response = parse_message(&wire).unwrap();
        let mut records = response.records.clone();
        let mut goodbye_srv = response.records[1].clone();
        goodbye_srv.ttl = 0;
        records.push(goodbye_srv);
        let adverts = sonos_adverts_from_records(&records);
        assert_eq!(adverts.len(), 1);
        assert_eq!(adverts[0].port, None);
        assert_eq!(adverts[0].addr, None);
        // A fresh SRV rediscovers both; the still-alive A resolves again.
        records.push(response.records[1].clone());
        let adverts = sonos_adverts_from_records(&records);
        assert_eq!(adverts[0].port, Some(1443));
        assert_eq!(adverts[0].addr, Some(Ipv4Addr::new(192, 0, 2, 11)));
    }

    #[test]
    fn a_txt_goodbye_invalidates_txt_values_without_falling_back() {
        // The TXT uuid deliberately differs from the instance-derived one, so
        // a fallback to the dead TXT would be visible.
        let wire = build_response(
            "Sonos-000E58A00001",
            &[
                "uuid=RINCON_FFFFFFFFFFFF01400",
                "hhid=Sonos_TEST",
                "bootseq=9",
            ],
            1443,
            [192, 0, 2, 11],
        );
        let response = parse_message(&wire).unwrap();
        let mut records = response.records.clone();
        let adverts = sonos_adverts_from_records(&records);
        assert_eq!(adverts[0].uuid.as_deref(), Some("RINCON_FFFFFFFFFFFF01400"));
        assert_eq!(adverts[0].household.as_deref(), Some("Sonos_TEST"));

        let mut goodbye_txt = response.records[2].clone();
        goodbye_txt.ttl = 0;
        records.push(goodbye_txt);
        let mut adverts = sonos_adverts_from_records(&records);
        assert_eq!(adverts.len(), 1);
        let ad = adverts.remove(0);
        assert_eq!(ad.household, None);
        assert_eq!(ad.boot_seq, None);
        assert_eq!(ad.location, None);
        // Only the instance-derived uuid remains; the dead TXT value is gone.
        assert_eq!(ad.uuid.as_deref(), Some("RINCON_000E58A0000101400"));
        // SRV and A are untouched.
        assert_eq!(ad.port, Some(1443));
        assert_eq!(ad.addr, Some(Ipv4Addr::new(192, 0, 2, 11)));

        // A fresh TXT repopulates the values.
        records.push(response.records[2].clone());
        let adverts = sonos_adverts_from_records(&records);
        assert_eq!(adverts[0].household.as_deref(), Some("Sonos_TEST"));
        assert_eq!(adverts[0].uuid.as_deref(), Some("RINCON_FFFFFFFFFFFF01400"));
    }
}
