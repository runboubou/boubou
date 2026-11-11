//! Names on the local network that browsers already trust, so a phone opens the page without
//! a warning. The names service at `lan.runboubou.com` points
//! `192-168-1-20.<id>.lan.runboubou.com` at 192.168.1.20 and has Let's Encrypt certify
//! `*.<id>.lan.runboubou.com` for the key whose hash is `<id>`. That key never leaves this
//! computer, and conversations never pass through the service: only the signing request and
//! the certificate do. The service's code is in `names/`.

use super::{hold, private};
use crate::progress;
use anyhow::{Context, Result, ensure};
use rcgen::{KeyPair, PublicKeyData};
use rustls::{
    pki_types::{CertificateDer, PrivatePkcs8KeyDer, pem::PemObject},
    sign::CertifiedKey,
};
use sha2::{Digest, Sha256};
use std::{
    fmt, fs,
    io::Write,
    net::Ipv4Addr,
    path::{Path, PathBuf},
    sync::{Arc, RwLock, mpsc},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub const ZONE: &str = "lan.runboubou.com";
pub const SERVICE: &str = "https://lan.runboubou.com/v1/certificate";
/// How long a start waits for its first certificate before it offers the address alone.
/// Let's Encrypt usually issues within seconds.
const FIRST: Duration = Duration::from_secs(30);
/// How often a running door checks whether its certificate is due, and retries after a failure.
const RECHECK: Duration = Duration::from_secs(6 * 60 * 60);
const RETRY: Duration = Duration::from_secs(15 * 60);

/// The id the names service gives the key with this DER-encoded SubjectPublicKeyInfo: 80 bits
/// of its hash in lowercase base 32.
pub(super) fn id(public_key: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let hash = Sha256::digest(public_key);
    let (mut id, mut buffer, mut bits) = (String::new(), 0u32, 0);
    for &byte in &hash[..10] {
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

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// The service's answer, kept as it came.
#[derive(serde::Deserialize)]
struct Answer {
    certificate: String,
    /// When to ask for the next one, in seconds since 1970.
    renew: u64,
    expires: u64,
}

struct Certified {
    key: Arc<CertifiedKey>,
    renew: u64,
    expires: u64,
}

/// This computer's names and, once the service has certified them, their certificate.
pub struct Names {
    id: String,
    key: KeyPair,
    folder: PathBuf,
    certified: RwLock<Option<Certified>>,
}

impl fmt::Debug for Names {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Names")
            .field("id", &self.id)
            .finish()
    }
}

impl Names {
    /// Loads this computer's key, made once and kept so its names and links stay the same,
    /// and the certificate kept with it.
    fn open(cache: &Path) -> Result<Self> {
        let folder = cache.join("serve").join("name");
        let _held = hold(&folder)?;
        let stored = folder.join("key.der");
        if !stored.exists() {
            private(&stored)?.write_all(&KeyPair::generate()?.serialize_der())?;
        }
        let key = KeyPair::try_from(fs::read(&stored)?.as_slice()).with_context(|| {
            format!(
                "The key in {} is damaged. Delete that file and retry.",
                stored.display()
            )
        })?;
        let names = Self {
            id: id(&key.subject_public_key_info()),
            key,
            folder,
            certified: RwLock::new(None),
        };
        if let Ok(kept) = fs::read(names.folder.join("certificate.json")) {
            *names.certified.write().unwrap() = names.certify(&kept).ok();
        }
        Ok(names)
    }

    /// The name that points at `address`.
    pub fn name(&self, address: Ipv4Addr) -> String {
        format!(
            "{}.{}.{ZONE}",
            address.to_string().replace('.', "-"),
            self.id
        )
    }

    /// The certificate for this computer's names, while it lasts.
    pub fn certificate(&self) -> Option<Arc<CertifiedKey>> {
        let certified = self.certified.read().unwrap();
        let certified = certified
            .as_ref()
            .filter(|certified| now() < certified.expires);
        certified.map(|certified| certified.key.clone())
    }

    /// The certificate for `name` that a browser asked for by name, if it is one of ours.
    pub fn certificate_for(&self, name: &str) -> Option<Arc<CertifiedKey>> {
        let suffix = format!(".{}.{ZONE}", self.id);
        let label = name.to_ascii_lowercase();
        let label = label.strip_suffix(&suffix)?;
        (!label.is_empty() && !label.contains('.'))
            .then(|| self.certificate())
            .flatten()
    }

    fn due(&self) -> bool {
        let certified = self.certified.read().unwrap();
        certified
            .as_ref()
            .is_none_or(|certified| now() >= certified.renew)
    }

    /// Asks `service` for a new certificate and keeps it.
    fn renew(&self, service: &str) -> Result<()> {
        let mut request = rcgen::CertificateParams::new([format!("*.{}.{ZONE}", self.id)])?;
        request.distinguished_name = rcgen::DistinguishedName::new();
        let request = request.serialize_request(&self.key)?;
        let response = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            // Let's Encrypt takes seconds to check the service's records and issue.
            .timeout(Duration::from_secs(120))
            .build()?
            .post(service)
            .header(reqwest::header::CONTENT_TYPE, "application/pkcs10")
            .body(request.der().to_vec())
            .send()
            .map_err(reqwest::Error::without_url)?;
        let status = response.status();
        let answer = response.bytes()?;
        ensure!(
            status.is_success(),
            "{status}: {}",
            String::from_utf8_lossy(&answer).trim()
        );
        let certified = self.certify(&answer)?;
        {
            let _held = hold(&self.folder)?;
            let partial = self.folder.join("certificate.partial");
            private(&partial)?.write_all(&answer)?;
            fs::rename(partial, self.folder.join("certificate.json"))?;
        }
        *self.certified.write().unwrap() = Some(certified);
        Ok(())
    }

    /// The service's answer as a certificate for this computer's key.
    fn certify(&self, answer: &[u8]) -> Result<Certified> {
        let answer: Answer = serde_json::from_slice(answer).context("Not a certificate")?;
        let chain = CertificateDer::pem_slice_iter(answer.certificate.as_bytes())
            .collect::<Result<Vec<_>, _>>()?;
        ensure!(!chain.is_empty(), "Not a certificate");
        let key = PrivatePkcs8KeyDer::from(self.key.serialize_der());
        let provider = rustls::crypto::ring::default_provider();
        // Checks that the certificate is for this computer's key.
        let key = CertifiedKey::from_der(chain, key.into(), &provider)?;
        Ok(Certified {
            key: Arc::new(key),
            renew: answer.renew,
            expires: answer.expires,
        })
    }
}

/// This computer's names, with a certificate from `service` if one arrives in time. A thread
/// keeps it renewed for as long as the door is open.
pub fn start(cache: &Path, service: &str) -> Result<Arc<Names>> {
    let names = Arc::new(Names::open(cache)?);
    let (finished, first) = mpsc::channel();
    let renewing = (names.clone(), service.to_owned());
    thread::Builder::new()
        .name("boubou-names".into())
        .spawn(move || {
            let (names, service) = renewing;
            loop {
                let wait = if names.due() {
                    let renewed = names.renew(&service);
                    let wait = if renewed.is_ok() { RECHECK } else { RETRY };
                    // Only the start waits for an answer.
                    let _ = finished.send(renewed);
                    wait
                } else {
                    RECHECK
                };
                thread::sleep(wait);
            }
        })?;
    if names.certificate().is_none() {
        let _progress = progress::Progress::new("Getting a certificate browsers trust", None);
        let failed = match first.recv_timeout(FIRST) {
            Ok(renewed) => renewed.err(),
            Err(_) => Some(anyhow::anyhow!("{ZONE} did not answer in time")),
        };
        if let Some(error) = failed {
            eprintln!("No certificate browsers trust yet: {error:#}");
        }
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_match_the_names_service() {
        // The same vector is checked by the names service.
        assert_eq!(id(b"boubou"), "vucitfkndwtzfjqm");
    }
}
