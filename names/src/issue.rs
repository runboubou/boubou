//! Certificates for computers' names. A computer sends a signing request for
//! `*.<id>.<zone>`, where `<id>` is the hash of the key that signed it, and gets Let's
//! Encrypt's certificate back. Its private key never leaves it, and no other key can have a
//! certificate for its names.

use crate::zone::{self, Zone};
use anyhow::{Context, Result, bail, ensure};
use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier, NewAccount,
    NewOrder, OrderStatus, RetryPolicy,
};
use std::{
    collections::{HashMap, HashSet},
    net::IpAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Semaphore;
use x509_parser::{
    certification_request::X509CertificationRequest, extensions::GeneralName,
    extensions::ParsedExtension, pem::Pem, prelude::FromDer,
};

/// New names one network address may have certified a day. Let's Encrypt certifies 50 new
/// names a week under one domain, and this keeps one network from taking them all.
const NEW_PER_DAY: usize = 10;
/// Orders with Let's Encrypt in progress at once.
const ORDERS: usize = 8;
const DAY: Duration = Duration::from_secs(24 * 60 * 60);

pub enum Refusal {
    /// The request is malformed or asks for names its key may not have.
    Invalid(String),
    /// An order for the same names is already in progress.
    Busy,
    /// This network has had enough new names certified today.
    Limited,
    /// Let's Encrypt did not issue.
    Failed(anyhow::Error),
}

/// A certificate chain in PEM and when to replace it, in seconds since 1970.
pub struct Issued {
    pub chain: String,
    pub renew: u64,
    pub expires: u64,
}

impl Issued {
    /// The chain's times, if its first certificate is for `public_key`.
    pub fn read(chain: String, public_key: &[u8]) -> Result<Self> {
        let (renew, expires) = {
            let pem = Pem::iter_from_buffer(chain.as_bytes())
                .next()
                .context("No certificate")??;
            let leaf = pem.parse_x509()?;
            ensure!(leaf.public_key().raw == public_key, "Another key");
            let validity = leaf.validity();
            let (from, until) = (
                validity.not_before.timestamp(),
                validity.not_after.timestamp(),
            );
            // Renewed after two thirds of its life, as Let's Encrypt advises.
            let renew = from + (until - from) * 2 / 3;
            (u64::try_from(renew)?, u64::try_from(until)?)
        };
        Ok(Self {
            chain,
            renew,
            expires,
        })
    }

    pub fn current(&self) -> bool {
        now() < self.renew
    }

    pub fn json(&self) -> String {
        serde_json::json!({
            "certificate": self.chain,
            "renew": self.renew,
            "expires": self.expires,
        })
        .to_string()
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// What a valid signing request asks for.
#[derive(Debug, PartialEq)]
pub struct Request {
    pub id: String,
    /// `*.<id>.<zone>`
    pub name: String,
    pub public_key: Vec<u8>,
}

/// Checks that `csr` is signed by its own key and asks for exactly the names that key's id
/// may have under `domain`.
pub fn requested(domain: &str, csr: &[u8]) -> Result<Request, String> {
    let (rest, request) =
        X509CertificationRequest::from_der(csr).map_err(|_| "Not a signing request")?;
    if !rest.is_empty() {
        return Err("Not a signing request".into());
    }
    request
        .verify_signature()
        .map_err(|_| "The request's signature does not match its key")?;
    let public_key = request.certification_request_info.subject_pki.raw.to_vec();
    let id = zone::id(&public_key);
    let name = format!("*.{id}.{domain}");
    let mut names = Vec::new();
    for extension in request.requested_extensions().into_iter().flatten() {
        if let ParsedExtension::SubjectAlternativeName(alternatives) = extension {
            for alternative in &alternatives.general_names {
                names.push(match alternative {
                    GeneralName::DNSName(dns) => dns.to_ascii_lowercase(),
                    _ => String::new(),
                });
            }
        }
    }
    let subject = &request.certification_request_info.subject;
    let common = subject.iter_common_name().all(|common| {
        common
            .as_str()
            .is_ok_and(|common| common.eq_ignore_ascii_case(&name))
    });
    if names != [name.as_str()] || !common {
        return Err(format!("This key may ask for {name} only"));
    }
    Ok(Request {
        id,
        name,
        public_key,
    })
}

pub struct Issuer {
    zone: Arc<Zone>,
    account: Account,
    /// Certificates issued, by id, kept so a computer asking again gets the same one.
    folder: PathBuf,
    /// When each network address last had new names certified.
    recent: Mutex<HashMap<IpAddr, Vec<Instant>>>,
    /// Ids with an order in progress.
    pending: Mutex<HashSet<String>>,
    orders: Semaphore,
}

impl Issuer {
    /// Uses the account kept in `state`, or opens one with the ACME server at `directory`.
    /// `root` is the server's own root certificate, for test servers.
    pub async fn open(
        zone: Arc<Zone>,
        state: &Path,
        directory: &str,
        root: Option<&Path>,
    ) -> Result<Self> {
        std::fs::create_dir_all(state.join("certificates"))?;
        let builder = || match root {
            Some(root) => Account::builder_with_root(root),
            None => Account::builder(),
        };
        let stored = state.join("account.json");
        let account = match std::fs::read(&stored) {
            Ok(credentials) => {
                let credentials: AccountCredentials = serde_json::from_slice(&credentials)?;
                builder()?.from_credentials(credentials).await?
            }
            Err(_) => {
                let new = NewAccount {
                    contact: &[],
                    terms_of_service_agreed: true,
                    only_return_existing: false,
                };
                let (account, credentials) =
                    builder()?.create(&new, directory.to_owned(), None).await?;
                write_private(&stored, serde_json::to_string(&credentials)?.as_bytes())?;
                account
            }
        };
        Ok(Self {
            zone,
            account,
            folder: state.join("certificates"),
            recent: Mutex::default(),
            pending: Mutex::default(),
            orders: Semaphore::new(ORDERS),
        })
    }

    /// A certificate for the names `csr` asks for, sent from network address `from`.
    pub async fn certificate(&self, csr: &[u8], from: IpAddr) -> Result<Issued, Refusal> {
        let request = requested(&self.zone.domain(), csr).map_err(Refusal::Invalid)?;
        let kept = self.folder.join(format!("{}.pem", request.id));
        let previous = std::fs::read_to_string(&kept)
            .ok()
            .and_then(|chain| Issued::read(chain, &request.public_key).ok());
        if let Some(previous) = previous.filter(Issued::current) {
            return Ok(previous);
        }
        if !self.pending.lock().unwrap().insert(request.id.clone()) {
            return Err(Refusal::Busy);
        }
        let _pending = Pending(&self.pending, &request.id);
        // Renewals do not count against Let's Encrypt's limit on new names.
        if !kept.exists() && !self.admit(from) {
            return Err(Refusal::Limited);
        }
        let _order = self.orders.acquire().await.map_err(|_| Refusal::Busy)?;
        let issue = async {
            let chain = self.order(&request.name, csr).await?;
            let issued = Issued::read(chain, &request.public_key)?;
            write_private(&kept, issued.chain.as_bytes())?;
            eprintln!("Certified {}.", request.name);
            Ok(issued)
        };
        issue.await.map_err(Refusal::Failed)
    }

    /// Counts a new name for `from`, unless it has had its share today.
    fn admit(&self, from: IpAddr) -> bool {
        let mut recent = self.recent.lock().unwrap();
        recent.retain(|_, times| {
            times.retain(|time| time.elapsed() < DAY);
            !times.is_empty()
        });
        let times = recent.entry(from).or_default();
        let admitted = times.len() < NEW_PER_DAY;
        if admitted {
            times.push(Instant::now());
        }
        admitted
    }

    /// Orders a certificate for `name` from Let's Encrypt, proving control of it through
    /// this server's own zone, and returns its chain in PEM.
    pub async fn order(&self, name: &str, csr: &[u8]) -> Result<String> {
        let identifiers = [Identifier::Dns(name.to_owned())];
        let mut order = self.account.new_order(&NewOrder::new(&identifiers)).await?;
        let mut published = Vec::new();
        let result = async {
            let mut authorizations = order.authorizations();
            while let Some(authorization) = authorizations.next().await {
                let mut authorization = authorization?;
                match authorization.status {
                    AuthorizationStatus::Valid => continue,
                    AuthorizationStatus::Pending => {}
                    status => bail!("The authorization is {status:?}"),
                }
                let mut challenge = authorization
                    .challenge(ChallengeType::Dns01)
                    .context("No DNS challenge")?;
                let Identifier::Dns(proven) = challenge.identifier().identifier else {
                    bail!("Not a DNS name");
                };
                let record = format!("_acme-challenge.{proven}");
                let value = challenge.key_authorization().dns_value();
                self.zone.prove(&record, value.clone());
                published.push((record, value));
                challenge.set_ready().await?;
            }
            let patience = RetryPolicy::new().timeout(Duration::from_secs(90));
            let status = order.poll_ready(&patience).await?;
            ensure!(status == OrderStatus::Ready, "The order is {status:?}");
            order.finalize_csr(csr).await?;
            Ok(order.poll_certificate(&patience).await?)
        }
        .await;
        for (record, value) in published {
            self.zone.forget(&record, &value);
        }
        result
    }
}

/// Clears an id's order in progress when dropped.
struct Pending<'a>(&'a Mutex<HashSet<String>>, &'a str);

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        self.0.lock().unwrap().remove(self.1);
    }
}

/// Writes `bytes` to `path` through a file only the owner can read, renamed into place.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let temporary = path.with_extension("partial");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::PublicKeyData;

    fn csr(names: &[&str], key: &rcgen::KeyPair) -> Vec<u8> {
        let mut params = rcgen::CertificateParams::new(
            names
                .iter()
                .map(|name| name.to_string())
                .collect::<Vec<_>>(),
        )
        .unwrap();
        params.distinguished_name = rcgen::DistinguishedName::new();
        params.serialize_request(key).unwrap().der().to_vec()
    }

    #[test]
    fn a_key_may_ask_for_its_own_names_only() {
        let key = rcgen::KeyPair::generate().unwrap();
        let id = zone::id(&key.subject_public_key_info());
        let own = format!("*.{id}.lan.example.com");
        let request = requested("lan.example.com", &csr(&[&own], &key)).unwrap();
        assert_eq!(request.id, id);
        assert_eq!(request.name, own);
        assert_eq!(request.public_key, key.subject_public_key_info());

        let other = rcgen::KeyPair::generate().unwrap();
        let other = format!(
            "*.{}.lan.example.com",
            zone::id(&other.subject_public_key_info())
        );
        for names in [
            vec![other.as_str()],
            vec![own.as_str(), "lan.example.com"],
            vec![&own[2..]],
            vec!["*.lan.example.com"],
            vec![],
        ] {
            assert!(
                requested("lan.example.com", &csr(&names, &key)).is_err(),
                "{names:?}"
            );
        }
        // Under another zone, the same names are not this zone's to give.
        assert!(requested("lan.example.org", &csr(&[&own], &key)).is_err());
    }

    #[test]
    fn requests_must_be_signed_by_their_own_key() {
        let key = rcgen::KeyPair::generate().unwrap();
        let name = format!(
            "*.{}.lan.example.com",
            zone::id(&key.subject_public_key_info())
        );
        let mut request = csr(&[&name], &key);
        let last = request.len() - 1;
        request[last] ^= 1;
        assert!(requested("lan.example.com", &request).is_err());
        assert!(requested("lan.example.com", b"not der").is_err());
    }

    #[test]
    fn a_kept_chain_is_used_until_two_thirds_of_its_life() {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(vec!["*.x.lan.example.com".into()]).unwrap();
        let start = SystemTime::now() - DAY * 10;
        params.not_before = start.into();
        params.not_after = (start + DAY * 90).into();
        let chain = params.self_signed(&key).unwrap().pem();
        let issued = Issued::read(chain.clone(), &key.subject_public_key_info()).unwrap();
        let start = start.duration_since(UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(issued.renew, start + 60 * DAY.as_secs());
        assert_eq!(issued.expires, start + 90 * DAY.as_secs());
        assert!(issued.current());
        let json: serde_json::Value = serde_json::from_str(&issued.json()).unwrap();
        assert_eq!(json["certificate"], chain.as_str());
        assert_eq!(json["renew"], issued.renew);
        // A chain kept for another key is not handed out.
        let other = rcgen::KeyPair::generate().unwrap();
        assert!(Issued::read(chain, &other.subject_public_key_info()).is_err());
    }
}
