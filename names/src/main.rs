//! `boubou-names`: the name server and certificate service behind the links Boubou prints
//! for phones. It answers DNS for one zone, where names spell private addresses, and gets
//! computers Let's Encrypt certificates for their names. Conversations never pass through it:
//! a phone looks a name up once and then talks to the computer on its own network.

mod issue;
mod zone;

use anyhow::{Context, Result};
use clap::Parser;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{
    Method, Request, Response, StatusCode,
    body::{Bytes, Incoming},
    header,
    service::service_fn,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use issue::{Issued, Issuer, Refusal};
use rcgen::PublicKeyData;
use rustls::{
    ServerConfig,
    pki_types::{CertificateDer, PrivatePkcs8KeyDer, pem::PemObject},
};
use std::{
    convert::Infallible,
    net::{IpAddr, Ipv4Addr},
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, UdpSocket},
};
use tokio_rustls::TlsAcceptor;
use zone::Zone;

/// How often the service's own certificate is checked for renewal, and retried after a
/// failure.
const CHECK: Duration = Duration::from_secs(10 * 60);

#[derive(Parser)]
#[command(version, about)]
struct Options {
    /// This server's public IPv4 address. The zone's own name points here.
    #[arg(long)]
    address: Ipv4Addr,
    /// The zone delegated to this server.
    #[arg(long, default_value = "lan.runboubou.com")]
    zone: String,
    /// The name the parent zone's NS record gives this server.
    #[arg(long, default_value = "names.runboubou.com")]
    nameserver: String,
    /// Where the ACME account and issued certificates are kept.
    #[arg(long, default_value = "/var/lib/boubou-names")]
    state: PathBuf,
    /// The ACME server's directory.
    #[arg(long, default_value = "https://acme-v02.api.letsencrypt.org/directory")]
    directory: String,
    /// The ACME server's root certificate in PEM, for test servers such as Pebble.
    #[arg(long)]
    root: Option<PathBuf>,
    /// The address to listen on, if not `--address`.
    #[arg(long)]
    listen: Option<IpAddr>,
    #[arg(long, default_value_t = 53)]
    dns_port: u16,
    #[arg(long, default_value_t = 443)]
    https_port: u16,
}

#[tokio::main]
async fn main() -> Result<()> {
    let options = Options::parse();
    let zone = Arc::new(Zone::new(
        &options.zone,
        options.address,
        &options.nameserver,
    )?);
    let listen = options.listen.unwrap_or(IpAddr::V4(options.address));
    // DNS first: proving the service's own name needs it.
    let udp = UdpSocket::bind((listen, options.dns_port))
        .await
        .context("Cannot listen for DNS over UDP")?;
    let tcp = TcpListener::bind((listen, options.dns_port))
        .await
        .context("Cannot listen for DNS over TCP")?;
    tokio::spawn(dns_udp(udp, zone.clone()));
    tokio::spawn(dns_tcp(tcp, zone.clone()));
    eprintln!("Answering DNS for {} on {listen}.", zone.domain());

    // Kept apart per ACME server, so test certificates never pass for real ones.
    let state = options.state.join(server_folder(&options.directory));
    let issuer = Arc::new(
        Issuer::open(
            zone.clone(),
            &state,
            &options.directory,
            options.root.as_deref(),
        )
        .await
        .context("Cannot reach the ACME server")?,
    );
    let own = state.join("own");
    let tls = loop {
        match own_certificate(&issuer, &own, &zone.domain()).await {
            Ok(tls) => break Arc::new(RwLock::new(tls)),
            Err(error) => eprintln!("No certificate for the service yet: {error:#}"),
        }
        tokio::time::sleep(CHECK).await;
    };
    let renewing = (issuer.clone(), tls.clone(), zone.domain());
    tokio::spawn(async move {
        let (issuer, tls, domain) = renewing;
        loop {
            tokio::time::sleep(CHECK).await;
            match own_certificate(&issuer, &own, &domain).await {
                Ok(fresh) => *tls.write().unwrap() = fresh,
                Err(error) => eprintln!("Cannot renew the service's certificate: {error:#}"),
            }
        }
    });
    let https = TcpListener::bind((listen, options.https_port))
        .await
        .context("Cannot listen for HTTPS")?;
    eprintln!("Issuing certificates at https://{}/.", zone.domain());
    serve_https(https, issuer, tls).await
}

/// The folder for one ACME server's account and certificates: its host and port.
fn server_folder(directory: &str) -> String {
    let rest = directory
        .split_once("://")
        .map_or(directory, |(_, rest)| rest);
    rest.split('/').next().unwrap_or_default().replace(':', "_")
}

/// TLS for the service's own name, from the certificate kept in `folder` or a new one.
async fn own_certificate(
    issuer: &Issuer,
    folder: &Path,
    domain: &str,
) -> Result<Arc<ServerConfig>> {
    std::fs::create_dir_all(folder)?;
    let (key_path, chain_path) = (folder.join("key.der"), folder.join("chain.pem"));
    let key = match std::fs::read(&key_path) {
        Ok(der) => rcgen::KeyPair::try_from(der.as_slice())?,
        Err(_) => {
            let key = rcgen::KeyPair::generate()?;
            issue::write_private(&key_path, &key.serialize_der())?;
            key
        }
    };
    let public_key = key.subject_public_key_info();
    let kept = std::fs::read_to_string(&chain_path)
        .ok()
        .and_then(|chain| Issued::read(chain, &public_key).ok());
    let issued = renewed(kept, || async {
        let mut params = rcgen::CertificateParams::new(vec![domain.to_owned()])?;
        params.distinguished_name = rcgen::DistinguishedName::new();
        let csr = params.serialize_request(&key)?;
        let chain = issuer.order(domain, csr.der()).await?;
        let issued = Issued::read(chain, &public_key)?;
        issue::write_private(&chain_path, issued.chain.as_bytes())?;
        eprintln!("Certified {domain}.");
        Ok(issued)
    })
    .await?;
    let chain =
        CertificateDer::pem_slice_iter(issued.chain.as_bytes()).collect::<Result<Vec<_>, _>>()?;
    let key = PrivatePkcs8KeyDer::from(key.serialize_der());
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(chain, key.into())?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// `kept` while it is current, and otherwise a `renew`ed chain. When renewing fails, `kept`
/// serves until it expires, so an outage at Let's Encrypt never takes the service down early.
async fn renewed<F: Future<Output = Result<Issued>>>(
    kept: Option<Issued>,
    renew: impl FnOnce() -> F,
) -> Result<Issued> {
    match kept {
        Some(kept) if kept.current() => Ok(kept),
        kept => match renew().await {
            Ok(fresh) => Ok(fresh),
            Err(error) => match kept.filter(Issued::valid) {
                Some(kept) => {
                    eprintln!("Cannot renew the service's certificate yet: {error:#}");
                    Ok(kept)
                }
                None => Err(error),
            },
        },
    }
}

async fn dns_udp(socket: UdpSocket, zone: Arc<Zone>) {
    let mut buffer = [0; 4096];
    loop {
        let Ok((read, peer)) = socket.recv_from(&mut buffer).await else {
            continue;
        };
        // No EDNS in answers, so they fit the classic 512 bytes.
        if let Some(response) = zone.answer(&buffer[..read], 512) {
            let _ = socket.send_to(&response, peer).await;
        }
    }
}

async fn dns_tcp(listener: TcpListener, zone: Arc<Zone>) {
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            continue;
        };
        let zone = zone.clone();
        tokio::spawn(async move {
            let exchange = async {
                loop {
                    let mut request = vec![0; usize::from(stream.read_u16().await?)];
                    stream.read_exact(&mut request).await?;
                    let Some(response) = zone.answer(&request, usize::from(u16::MAX)) else {
                        return Ok(());
                    };
                    stream.write_u16(response.len() as u16).await?;
                    stream.write_all(&response).await?;
                }
            };
            let _: Result<std::io::Result<()>, _> =
                tokio::time::timeout(Duration::from_secs(10), exchange).await;
        });
    }
}

async fn serve_https(
    listener: TcpListener,
    issuer: Arc<Issuer>,
    tls: Arc<RwLock<Arc<ServerConfig>>>,
) -> Result<()> {
    loop {
        let Ok((stream, peer)) = listener.accept().await else {
            continue;
        };
        let acceptor = TlsAcceptor::from(tls.read().unwrap().clone());
        let issuer = issuer.clone();
        tokio::spawn(async move {
            let handshake = tokio::time::timeout(Duration::from_secs(10), acceptor.accept(stream));
            let Ok(Ok(stream)) = handshake.await else {
                return;
            };
            let service = service_fn(move |request| answer(issuer.clone(), peer.ip(), request));
            let connection = hyper::server::conn::http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(Duration::from_secs(10))
                .keep_alive(false)
                .serve_connection(TokioIo::new(stream), service);
            // Issuing takes Let's Encrypt seconds; a stalled connection is let go.
            let _ = tokio::time::timeout(Duration::from_secs(300), connection).await;
        });
    }
}

async fn answer(
    issuer: Arc<Issuer>,
    from: IpAddr,
    request: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let reply = |status: StatusCode, kind: &str, body: String, retry: Option<u32>| {
        let mut response = Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, kind)
            .header(header::CACHE_CONTROL, "no-store");
        if let Some(seconds) = retry {
            response = response.header(header::RETRY_AFTER, seconds);
        }
        response.body(Full::new(Bytes::from(body))).unwrap()
    };
    let text = |status, body: &str, retry| {
        reply(
            status,
            "text/plain; charset=utf-8",
            format!("{body}\n"),
            retry,
        )
    };
    Ok(match (request.method(), request.uri().path()) {
        (&Method::POST, "/v1/certificate") => {
            let Ok(body) = Limited::new(request.into_body(), 16 * 1024).collect().await else {
                return Ok(text(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "The request is too large.",
                    None,
                ));
            };
            match issuer.certificate(&body.to_bytes(), from).await {
                Ok(issued) => reply(StatusCode::OK, "application/json", issued.json(), None),
                Err(Refusal::Invalid(why)) => text(StatusCode::BAD_REQUEST, &why, None),
                Err(Refusal::Busy) => text(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "A certificate for these names is being issued. Retry shortly.",
                    Some(30),
                ),
                Err(Refusal::Limited) => text(
                    StatusCode::TOO_MANY_REQUESTS,
                    "This network has had enough new names certified today.",
                    Some(60 * 60),
                ),
                Err(Refusal::Failed(error)) => {
                    eprintln!("Not issued: {error:#}");
                    text(
                        StatusCode::BAD_GATEWAY,
                        "Let's Encrypt did not issue a certificate. Retry later.",
                        Some(10 * 60),
                    )
                }
            }
        }
        (&Method::GET, "/") => text(
            StatusCode::OK,
            "Names and certificates for Boubou on local networks: https://github.com/runboubou/boubou",
            None,
        ),
        _ => text(StatusCode::NOT_FOUND, "Not found.", None),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_acme_server_keeps_its_own_folder() {
        assert_eq!(
            server_folder("https://acme-v02.api.letsencrypt.org/directory"),
            "acme-v02.api.letsencrypt.org"
        );
        assert_eq!(
            server_folder("https://localhost:14000/dir"),
            "localhost_14000"
        );
    }

    #[tokio::test]
    async fn a_valid_certificate_serves_until_it_expires_when_renewing_fails() {
        let now = issue::now();
        let issued = |renew, expires| Issued {
            chain: String::new(),
            renew,
            expires,
        };
        let failing = || async { Err(anyhow::anyhow!("Let's Encrypt is down")) };
        let fresh = || async { Ok(issued(now + 200, now + 300)) };
        // Current: no renewal is attempted.
        let current = renewed(Some(issued(now + 100, now + 200)), failing).await;
        assert_eq!(current.unwrap().renew, now + 100);
        // Due: renewed when that works, kept when it does not.
        let due = || Some(issued(now - 10, now + 100));
        assert_eq!(renewed(due(), fresh).await.unwrap().renew, now + 200);
        assert_eq!(renewed(due(), failing).await.unwrap().renew, now - 10);
        // Expired or missing: only a renewal will do.
        assert!(
            renewed(Some(issued(now - 20, now - 10)), failing)
                .await
                .is_err()
        );
        assert!(renewed(None, failing).await.is_err());
        assert_eq!(renewed(None, fresh).await.unwrap().renew, now + 200);
    }
}
