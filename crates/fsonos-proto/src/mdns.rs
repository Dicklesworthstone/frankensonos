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
fn read_name(bytes: &[u8], mut pos: usize) -> Result<(String, usize), ProtoError> {
    let mut labels: Vec<String> = Vec::new();
    let mut end: Option<usize> = None;
    let mut hops = 0;
    loop {
        if hops > 32 {
            return Err(malformed("name compression loop"));
        }
        hops += 1;
        let &len = bytes.get(pos).ok_or_else(|| malformed("truncated name"))?;
        if len == 0 {
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
    let mut questions = Vec::with_capacity(qd);
    for _ in 0..qd {
        let (name, next) = read_name(bytes, pos)?;
        let qtype = u16_at(bytes, next)?;
        let _qclass = u16_at(bytes, next + 2)?;
        questions.push((name, qtype));
        pos = next + 4;
    }
    let mut records = Vec::with_capacity(an + ns + ar);
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
            TYPE_PTR => RecordData::Ptr(read_name(bytes, rstart)?.0),
            TYPE_SRV => {
                if rdlen < 7 {
                    return Err(malformed("truncated SRV"));
                }
                let port = u16_at(bytes, rstart + 4)?;
                let (target, _) = read_name(bytes, rstart + 6)?;
                RecordData::Srv { port, target }
            }
            TYPE_TXT => {
                let mut txts = Vec::new();
                let mut t = rstart;
                while t < rend {
                    let slen =
                        usize::from(*bytes.get(t).ok_or_else(|| malformed("truncated TXT"))?);
                    let s = bytes
                        .get(t + 1..t + 1 + slen)
                        .ok_or_else(|| malformed("truncated TXT string"))?;
                    txts.push(String::from_utf8_lossy(s).into_owned());
                    t += 1 + slen;
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

/// Extract Sonos player advertisements from a parsed message: every PTR to a
/// `_sonos._tcp` instance, enriched by the SRV/TXT/A records for the same
/// instance (typically in the additional section).
#[must_use]
pub fn sonos_adverts(msg: &DnsMessage) -> Vec<SonosAdvert> {
    let mut out: Vec<SonosAdvert> = Vec::new();
    for rec in &msg.records {
        let RecordData::Ptr(instance) = &rec.data else {
            continue;
        };
        if !rec.name.eq_ignore_ascii_case("_sonos._tcp.local") {
            continue;
        }
        let mut advert = SonosAdvert {
            instance: strip_service_domain(instance).to_string(),
            uuid: None,
            household: None,
            boot_seq: None,
            location: None,
            port: None,
            addr: None,
        };
        // The instance's own records are keyed by the full FQDN: SRV on the
        // instance name, TXT on the instance name, A on the SRV target.
        let mut target_host: Option<&str> = None;
        for r in &msg.records {
            match &r.data {
                RecordData::Srv { port, target } if r.name == *instance => {
                    advert.port = Some(*port);
                    target_host = Some(target);
                }
                RecordData::Txt(txts) if r.name == *instance => {
                    for t in txts {
                        if let Some((k, v)) = t.split_once('=') {
                            match k {
                                "uuid" => advert.uuid = Some(v.to_string()),
                                "hhid" => advert.household = Some(v.to_string()),
                                "bootseq" => advert.boot_seq = v.parse().ok(),
                                "location" => advert.location = Some(v.to_string()),
                                _ => {}
                            }
                        }
                    }
                }
                RecordData::A(addr) => {
                    if target_host.is_some_and(|h| r.name.eq_ignore_ascii_case(h)) {
                        advert.addr = Some(*addr);
                    }
                }
                _ => {}
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
fn strip_service_domain(name: &str) -> &str {
    const SUFFIX: &str = "._sonos._tcp.local";
    if name.len() > SUFFIX.len() && name[name.len() - SUFFIX.len()..].eq_ignore_ascii_case(SUFFIX) {
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
    fn build_response(instance: &str, txts: &[&str], port: u16, ip: [u8; 4]) -> Vec<u8> {
        let mut b = Vec::new();
        // header: id 0, response, 0 questions, 1 answer, 0 authority, 3 additional
        b.extend_from_slice(&[0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 3]);
        let svc = b"_sonos._tcp.local";
        let mut put_name = |b: &mut Vec<u8>, name: &str| {
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
        let mut put_ptr = |b: &mut Vec<u8>| {
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
    fn rejects_truncated_messages() {
        assert!(parse_message(&[0u8; 5]).is_err());
        let mut good = build_response("Sonos-000E58A00001", &["vers=1"], 1443, [192, 0, 2, 11]);
        assert!(parse_message(&good).is_ok());
        good.truncate(good.len() - 3);
        assert!(parse_message(&good).is_err());
    }
}
