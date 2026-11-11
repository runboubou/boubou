//! The zone this server answers for. `192-168-1-20.<id>.<zone>` points at 192.168.1.20, the
//! address it spells, so a phone on the same network reaches that computer directly. Only
//! private addresses are spelled: the names exist for local networks. `<id>` comes from the
//! hash of a computer's key, which is how certificates for its names are kept to it.

use hickory_proto::{
    op::{Message, MessageType, Metadata, OpCode, ResponseCode},
    rr::{
        Name, RData, Record, RecordType,
        rdata::{A, CAA, NS, SOA, TXT},
    },
};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, net::Ipv4Addr, sync::Mutex};

/// The id's 16 characters carry 80 bits of the key's hash.
const ID_BYTES: usize = 10;
/// Names that spell an address never change their answer.
const LONG: u32 = 24 * 60 * 60;
const SHORT: u32 = 5 * 60;
/// How long resolvers remember that a name has no answer.
const NEGATIVE: u32 = 60;

/// The id of the computer whose key has this DER-encoded SubjectPublicKeyInfo.
pub fn id(public_key: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let hash = Sha256::digest(public_key);
    let (mut id, mut buffer, mut bits) = (String::new(), 0u32, 0);
    for &byte in &hash[..ID_BYTES] {
        buffer = buffer << 8 | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            id.push(ALPHABET[(buffer >> bits) as usize & 31].into());
        }
        buffer &= (1 << bits) - 1;
    }
    id
}

/// How a challenge record is filed: lowercase, without the final dot.
fn key(record: &str) -> String {
    record.trim_end_matches('.').to_ascii_lowercase()
}

fn is_id(label: &str) -> bool {
    label.len() == ID_BYTES * 8 / 5
        && label
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || (b'2'..=b'7').contains(&byte))
}

/// The private address a label such as `192-168-1-20` spells, written the one way it is
/// printed.
fn spelled(label: &str) -> Option<Ipv4Addr> {
    let address: Ipv4Addr = label.replace('-', ".").parse().ok()?;
    (address.is_private() && address.to_string().replace('.', "-") == label).then_some(address)
}

pub struct Zone {
    /// Such as `lan.runboubou.com.`, in lowercase.
    pub apex: Name,
    /// This server's public address, where the apex points: the certificate service.
    address: Ipv4Addr,
    /// The name server the parent zone delegates this zone to.
    server: Name,
    /// Values of `_acme-challenge` TXT records, by lowercase name, while an order waits on them.
    challenges: Mutex<HashMap<String, Vec<String>>>,
}

enum Found {
    Records(Vec<RData>, u32),
    NoData,
    NoName,
}

impl Zone {
    pub fn new(apex: &str, address: Ipv4Addr, server: &str) -> anyhow::Result<Self> {
        let absolute = |name: &str| -> anyhow::Result<Name> {
            let mut name = Name::from_ascii(name)?.to_lowercase();
            name.set_fqdn(true);
            Ok(name)
        };
        Ok(Self {
            apex: absolute(apex)?,
            address,
            server: absolute(server)?,
            challenges: Mutex::default(),
        })
    }

    /// The apex without its final dot, as certificates and URLs write it.
    pub fn domain(&self) -> String {
        self.apex.to_ascii().trim_end_matches('.').to_owned()
    }

    /// Publishes `value` at `record` until [`Zone::forget`].
    pub fn prove(&self, record: &str, value: String) {
        let mut challenges = self.challenges.lock().unwrap();
        challenges.entry(key(record)).or_default().push(value);
    }

    pub fn forget(&self, record: &str, value: &str) {
        let mut challenges = self.challenges.lock().unwrap();
        let record = key(record);
        if let Some(values) = challenges.get_mut(&record) {
            values.retain(|kept| kept != value);
            if values.is_empty() {
                challenges.remove(&record);
            }
        }
    }

    /// The response to a DNS request, at most `limit` bytes long, or `None` for messages that
    /// get no answer.
    pub fn answer(&self, request: &[u8], limit: usize) -> Option<Vec<u8>> {
        let request = Message::from_vec(request).ok()?;
        if request.metadata.message_type != MessageType::Query {
            return None;
        }
        let mut response = Message::response(request.metadata.id, request.metadata.op_code);
        response.metadata = Metadata::response_from_request(&request.metadata);
        let code = match request.queries.as_slice() {
            [query] if request.metadata.op_code == OpCode::Query => {
                response.add_query(query.clone());
                self.respond(&query.name, query.query_type, &mut response)
            }
            [_] => ResponseCode::NotImp,
            _ => ResponseCode::FormErr,
        };
        response.metadata.response_code = code;
        let mut bytes = response.to_vec().ok()?;
        if bytes.len() > limit {
            response.answers.clear();
            response.authorities.clear();
            response.metadata.truncation = true;
            bytes = response.to_vec().ok()?;
        }
        Some(bytes)
    }

    fn respond(&self, name: &Name, kind: RecordType, response: &mut Message) -> ResponseCode {
        let Some(found) = self.find(name, kind) else {
            return ResponseCode::Refused;
        };
        response.metadata.authoritative = true;
        match found {
            Found::Records(records, ttl) => {
                for data in records {
                    response.add_answer(Record::from_rdata(name.clone(), ttl, data));
                }
                ResponseCode::NoError
            }
            Found::NoData => {
                response.add_authority(self.soa());
                ResponseCode::NoError
            }
            Found::NoName => {
                response.add_authority(self.soa());
                ResponseCode::NXDomain
            }
        }
    }

    /// `None` for names outside the zone, which this server does not answer for.
    fn find(&self, name: &Name, kind: RecordType) -> Option<Found> {
        let name = name.to_lowercase();
        if !self.apex.zone_of(&name) {
            return None;
        }
        let depth = usize::from(name.num_labels() - self.apex.num_labels());
        let labels: Vec<_> = name
            .iter()
            .take(depth)
            .map(|label| String::from_utf8_lossy(label).into_owned())
            .collect();
        let labels: Vec<_> = labels.iter().map(String::as_str).collect();
        let records = |data: Vec<RData>, ttl| {
            Some(if data.is_empty() {
                Found::NoData
            } else {
                Found::Records(data, ttl)
            })
        };
        match labels.as_slice() {
            [] => match kind {
                RecordType::A => records(vec![RData::A(A(self.address))], SHORT),
                RecordType::NS => records(vec![RData::NS(NS(self.server.clone()))], LONG),
                RecordType::SOA => records(vec![self.soa().data], LONG),
                // Only Let's Encrypt may issue certificates for these names.
                RecordType::CAA => records(
                    vec![RData::CAA(CAA::new_issue(
                        false,
                        Name::from_ascii("letsencrypt.org").ok(),
                        Vec::new(),
                    ))],
                    LONG,
                ),
                _ => Some(Found::NoData),
            },
            ["_acme-challenge", rest @ ..]
                if rest.is_empty() || matches!(rest, [id] if is_id(id)) =>
            {
                let challenges = self.challenges.lock().unwrap();
                let values = challenges
                    .get(&key(&name.to_ascii()))
                    .cloned()
                    .unwrap_or_default();
                let data = match kind {
                    RecordType::TXT => values
                        .into_iter()
                        .map(|value| RData::TXT(TXT::new(vec![value])))
                        .collect(),
                    _ => Vec::new(),
                };
                records(data, 0)
            }
            [id] if is_id(id) => Some(Found::NoData),
            [address, id] if is_id(id) => match spelled(address) {
                Some(address) if kind == RecordType::A => records(vec![RData::A(A(address))], LONG),
                Some(_) => Some(Found::NoData),
                None => Some(Found::NoName),
            },
            _ => Some(Found::NoName),
        }
    }

    fn soa(&self) -> Record {
        let contact = Name::from_ascii("hostmaster")
            .and_then(|name| name.append_domain(&self.apex))
            .unwrap_or_else(|_| self.apex.clone());
        let soa = SOA::new(self.server.clone(), contact, 1, 3600, 600, 86400, NEGATIVE);
        Record::from_rdata(self.apex.clone(), NEGATIVE, RData::SOA(soa))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hickory_proto::op::Query;

    fn zone() -> Zone {
        Zone::new(
            "lan.example.com",
            Ipv4Addr::new(203, 0, 113, 7),
            "names.example.com",
        )
        .unwrap()
    }

    fn ask(zone: &Zone, name: &str, kind: RecordType) -> Message {
        let mut request = Message::query();
        request.metadata.id = 4242;
        request.metadata.recursion_desired = true;
        request.add_query(Query::query(Name::from_ascii(name).unwrap(), kind));
        let response = zone.answer(&request.to_vec().unwrap(), 512).unwrap();
        let response = Message::from_vec(&response).unwrap();
        assert_eq!(response.metadata.id, 4242);
        assert_eq!(response.metadata.message_type, MessageType::Response);
        response
    }

    fn data(response: &Message) -> Vec<String> {
        response
            .answers
            .iter()
            .map(|record| record.data.to_string())
            .collect()
    }

    const ID: &str = "abcdefgh234567ab";

    #[test]
    fn ids_are_sixteen_letters_and_digits_of_the_key_hash() {
        // The same vector is checked by Boubou, which names itself the same way.
        assert_eq!(id(b"boubou"), "vucitfkndwtzfjqm");
        assert!(is_id(&id(b"another key")));
        assert!(!is_id("ABCDEFGH234567AB") && !is_id("abcdefgh2345678a") && !is_id("abc"));
    }

    #[test]
    fn names_spell_private_addresses_only() {
        let zone = zone();
        let name = format!("192-168-1-20.{ID}.lan.example.com.");
        let response = ask(&zone, &name, RecordType::A);
        assert!(response.metadata.authoritative);
        assert_eq!(response.metadata.response_code, ResponseCode::NoError);
        assert_eq!(data(&response), ["192.168.1.20"]);
        assert_eq!(response.answers[0].ttl, LONG);
        // Resolvers vary the case of names they ask for and expect it back as sent.
        let mixed = format!("10-0-0-5.{}.LaN.eXample.com.", ID.to_uppercase());
        let response = ask(&zone, &mixed, RecordType::A);
        assert_eq!(data(&response), ["10.0.0.5"]);
        assert_eq!(response.answers[0].name.to_ascii(), mixed);
        assert_eq!(response.queries[0].name.to_ascii(), mixed);

        for missing in [
            format!("8-8-8-8.{ID}.lan.example.com."),
            format!("192-168-001-20.{ID}.lan.example.com."),
            format!("192-168-1.{ID}.lan.example.com."),
            "192-168-1-20.short.lan.example.com.".into(),
            format!("a.192-168-1-20.{ID}.lan.example.com."),
        ] {
            let response = ask(&zone, &missing, RecordType::A);
            assert_eq!(
                response.metadata.response_code,
                ResponseCode::NXDomain,
                "{missing}"
            );
            assert_eq!(response.authorities[0].record_type(), RecordType::SOA);
        }
        // The name exists, with an address of another family only.
        let response = ask(&zone, &name, RecordType::AAAA);
        assert_eq!(response.metadata.response_code, ResponseCode::NoError);
        assert!(response.answers.is_empty());
        assert_eq!(response.authorities[0].record_type(), RecordType::SOA);
    }

    #[test]
    fn the_apex_is_the_service_and_lets_only_lets_encrypt_issue() {
        let zone = zone();
        let response = ask(&zone, "lan.example.com.", RecordType::A);
        assert_eq!(data(&response), ["203.0.113.7"]);
        let response = ask(&zone, "lan.example.com.", RecordType::NS);
        assert_eq!(data(&response), ["names.example.com."]);
        let response = ask(&zone, "lan.example.com.", RecordType::CAA);
        assert_eq!(data(&response), ["0 issue \"letsencrypt.org\""]);
        // A computer's own name defers to the apex.
        let response = ask(&zone, &format!("{ID}.lan.example.com."), RecordType::CAA);
        assert_eq!(response.metadata.response_code, ResponseCode::NoError);
        assert!(response.answers.is_empty());
    }

    #[test]
    fn challenges_are_published_while_an_order_waits() {
        let zone = zone();
        let record = format!("_acme-challenge.{ID}.lan.example.com");
        let ask_txt = || ask(&zone, &format!("{record}."), RecordType::TXT);
        assert!(ask_txt().answers.is_empty());
        zone.prove(&record, "first".into());
        zone.prove(&record.to_uppercase(), "second".into());
        let response = ask_txt();
        assert_eq!(data(&response), ["first", "second"]);
        assert_eq!(response.answers[0].ttl, 0);
        zone.forget(&record, "first");
        assert_eq!(data(&ask_txt()), ["second"]);
        zone.forget(&record, "second");
        assert!(ask_txt().answers.is_empty());
        // The service's own name is proven the same way.
        zone.prove("_acme-challenge.lan.example.com", "own".into());
        assert_eq!(
            data(&ask(
                &zone,
                "_acme-challenge.lan.example.com.",
                RecordType::TXT
            )),
            ["own"]
        );
    }

    #[test]
    fn other_zones_and_odd_requests_are_refused() {
        let zone = zone();
        let response = ask(&zone, "example.org.", RecordType::A);
        assert_eq!(response.metadata.response_code, ResponseCode::Refused);
        assert!(!response.metadata.authoritative);
        assert!(zone.answer(b"\x00\x01", 512).is_none());
        // Answers never pass for questions.
        let mut reply = Message::response(1, OpCode::Query);
        reply.add_query(Query::query(
            Name::from_ascii("lan.example.com.").unwrap(),
            RecordType::A,
        ));
        assert!(zone.answer(&reply.to_vec().unwrap(), 512).is_none());
    }

    #[test]
    fn answers_too_long_for_udp_are_truncated() {
        let zone = zone();
        let record = format!("_acme-challenge.{ID}.lan.example.com");
        for value in 0..20 {
            zone.prove(&record, format!("{value:0>43}"));
        }
        let mut request = Message::query();
        request.add_query(Query::query(
            Name::from_ascii(format!("{record}.")).unwrap(),
            RecordType::TXT,
        ));
        let bytes = request.to_vec().unwrap();
        let short = Message::from_vec(&zone.answer(&bytes, 512).unwrap()).unwrap();
        assert!(short.metadata.truncation && short.answers.is_empty());
        let long = Message::from_vec(&zone.answer(&bytes, 65535).unwrap()).unwrap();
        assert_eq!(long.answers.len(), 20);
    }
}
