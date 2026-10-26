//! Remote daemon — a `claudebase run` session on one machine whose daemon runs
//! on another (docs/design/remote-daemon.md).
//!
//! Server side: an optional TLS-over-TCP listener carrying the same framing as
//! the local socket. The certificate is self-signed, generated here, and pinned
//! by the client by its SHA-256 — no CA, no domain. The first frame must be
//! `auth {token}`; one token per client machine, stored hashed in `chat.db`,
//! revocable. A remote connection may call the session surface only
//! ([`remote_may_call`]); administration stays with the machine's own user.
//!
//! Client side: named profiles in `~/.claude/claudebase/remotes.toml`, and the
//! [`Endpoint`] every connection to "the daemon" resolves through —
//! `CLAUDEBASE_DAEMON` (a profile, exported by `claudebase run --daemon`), then
//! the default profile, then the local socket.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::daemon::ipc::{read_frame, write_frame};

/// Which daemon a process talks to. Exported by `claudebase run --daemon`.
pub const DAEMON_ENV: &str = "CLAUDEBASE_DAEMON";
/// `CLAUDEBASE_DAEMON=local` forces the local socket over a default profile.
pub const LOCAL: &str = "local";
/// URL scheme of a connection string.
const SCHEME: &str = "claudebase://";
/// `daemon_state` key holding the listener's bind address.
const BIND_KEY: &str = "remote.bind";
/// Default port: next to the HTTP callback's 8585.
pub const DEFAULT_PORT: u16 = 8586;
/// A client that has not authenticated by then is dropped.
const AUTH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub type BoxRead = Box<dyn AsyncRead + Send + Unpin>;
pub type BoxWrite = Box<dyn AsyncWrite + Send + Unpin>;

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

fn claudebase_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .unwrap_or_else(|| std::ffi::OsString::from("."));
    PathBuf::from(home).join(".claude").join("claudebase")
}

/// The server's certificate and key.
fn tls_dir() -> PathBuf {
    claudebase_dir().join("remote")
}

/// The client's profiles.
pub fn profiles_path() -> PathBuf {
    claudebase_dir().join("remotes.toml")
}

fn write_private(path: &Path, body: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
    }
    // Written beside and renamed, so a reader never sees half a key.
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    {
        #[cfg(unix)]
        let mut f = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)
        }
        .with_context(|| format!("create {}", tmp.display()))?;
        #[cfg(not(unix))]
        let mut f = std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        std::io::Write::write_all(&mut f, body)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

// ---------------------------------------------------------------------------
// Server: certificate
// ---------------------------------------------------------------------------

/// Generate the listener's certificate unless there is one. Returns its pin.
pub fn ensure_certificate() -> Result<String> {
    let (cert, key) = (tls_dir().join("cert.pem"), tls_dir().join("key.pem"));
    if !cert.exists() || !key.exists() {
        let host = hostname();
        let ck = rcgen::generate_simple_self_signed(vec!["claudebase".to_string(), host])
            .context("generate the TLS certificate")?;
        write_private(&key, ck.key_pair.serialize_pem().as_bytes())?;
        write_private(&cert, ck.cert.pem().as_bytes())?;
    }
    certificate_pin()
}

/// `sha256:<hex>` of the listener's certificate, as clients pin it.
pub fn certificate_pin() -> Result<String> {
    use rustls_pki_types::pem::PemObject;
    let path = tls_dir().join("cert.pem");
    let der = rustls_pki_types::CertificateDer::from_pem_file(&path)
        .map_err(|e| anyhow!("read {}: {e}", path.display()))?;
    Ok(format!("sha256:{}", sha256_hex(der.as_ref())))
}

fn server_tls_config() -> Result<Arc<rustls::ServerConfig>> {
    use rustls_pki_types::pem::PemObject;
    let (cert, key) = (tls_dir().join("cert.pem"), tls_dir().join("key.pem"));
    let cert = rustls_pki_types::CertificateDer::from_pem_file(&cert)
        .map_err(|e| anyhow!("read {}: {e}", cert.display()))?;
    let key = rustls_pki_types::PrivateKeyDer::from_pem_file(&key)
        .map_err(|e| anyhow!("read {}: {e}", key.display()))?;
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .context("load the TLS certificate")?;
    Ok(Arc::new(config))
}

pub fn hostname() -> String {
    crate::daemon::agent_registry::this_host()
}

// ---------------------------------------------------------------------------
// Server: configuration and tokens (chat.db, like the callback endpoint)
// ---------------------------------------------------------------------------

fn ensure_tokens_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS remote_tokens (
           name         TEXT PRIMARY KEY,
           sha256       TEXT NOT NULL UNIQUE,
           created_at   INTEGER NOT NULL,
           last_used_at INTEGER
         );",
    )
    .context("remote: ensure remote_tokens")?;
    Ok(())
}

pub fn configured_bind(conn: &Connection) -> Result<Option<String>> {
    conn.query_row("SELECT value FROM daemon_state WHERE key = ?1", params![BIND_KEY], |r| r.get(0))
        .optional()
        .context("remote: read bind")
}

pub fn set_bind(conn: &Connection, bind: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO daemon_state (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![BIND_KEY, bind],
    )
    .context("remote: store bind")?;
    Ok(())
}

pub fn clear_bind(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM daemon_state WHERE key = ?1", params![BIND_KEY])
        .context("remote: clear bind")?;
    Ok(())
}

/// Issue a token for client `name`, replacing any earlier one of that name.
/// Only the hash is stored: the token is shown once.
pub fn issue_token(conn: &Connection, name: &str) -> Result<String> {
    check_client_name(name)?;
    ensure_tokens_table(conn)?;
    let token = format!(
        "cbr_{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    conn.execute(
        "INSERT INTO remote_tokens (name, sha256, created_at) VALUES (?1, ?2, ?3) \
         ON CONFLICT(name) DO UPDATE SET sha256 = excluded.sha256, created_at = excluded.created_at, \
         last_used_at = NULL",
        params![name, sha256_hex(token.as_bytes()), chrono::Utc::now().timestamp()],
    )?;
    Ok(token)
}

pub fn revoke_token(conn: &Connection, name: &str) -> Result<bool> {
    ensure_tokens_table(conn)?;
    Ok(conn.execute("DELETE FROM remote_tokens WHERE name = ?1", params![name])? > 0)
}

#[derive(Debug, Serialize)]
pub struct TokenInfo {
    pub name: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

pub fn list_tokens(conn: &Connection) -> Result<Vec<TokenInfo>> {
    ensure_tokens_table(conn)?;
    let mut stmt = conn.prepare("SELECT name, created_at, last_used_at FROM remote_tokens ORDER BY name")?;
    let rows = stmt.query_map([], |r| Ok(TokenInfo { name: r.get(0)?, created_at: r.get(1)?, last_used_at: r.get(2)? }))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The client a token belongs to, recording the use.
fn authenticate(conn: &Connection, token: &str) -> Result<Option<String>> {
    ensure_tokens_table(conn)?;
    let name: Option<String> = conn
        .query_row(
            "SELECT name FROM remote_tokens WHERE sha256 = ?1",
            params![sha256_hex(token.as_bytes())],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(n) = &name {
        conn.execute(
            "UPDATE remote_tokens SET last_used_at = ?1 WHERE name = ?2",
            params![chrono::Utc::now().timestamp(), n],
        )?;
    }
    Ok(name)
}

fn check_client_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("a client name is 1-64 characters of [a-zA-Z0-9_-]");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Server: what a remote connection may call
// ---------------------------------------------------------------------------

/// Who is on the other end of a connection.
#[derive(Debug, Clone)]
pub enum Peer {
    /// The local socket: this machine's user.
    Local,
    /// The TLS listener: the client the token was issued to.
    Remote { client: String },
}

/// The session surface. Everything a `claudebase run` session and the CLI
/// calls made inside it need — and nothing that administers the daemon
/// (access lists, bots, callback and remote tokens all live in commands that
/// open `chat.db` locally, so they are not reachable over any connection).
/// Unknown tools are refused: a tool added later is local-only until it is
/// listed here on purpose.
pub fn remote_may_call(tool: &str) -> bool {
    matches!(
        tool,
        "claudebase_daemon_status"
            | "chat_post"
            | "chat_reply"
            | "chat_subscribe"
            | "chat_list"
            | "chat_list_threads"
            | "chat_ask"
            | "chat_list_pending_asks"
            | "agent_register"
            | "agent_rename"
            | "agent_bind_session"
            | "agent_describe"
            | "agent_send"
            | "agent_set_dnd"
            | "agent_unregister"
            | "agent_list_alive"
    ) || tool.starts_with("journal_")
        || tool.starts_with("session_")
}

// ---------------------------------------------------------------------------
// Server: listener
// ---------------------------------------------------------------------------

/// Accept TLS connections on `bind`, authenticate each, and hand it to
/// `handle` as an ordinary connection marked [`Peer::Remote`]. Spawned only
/// when a bind is configured; a failure to start is logged, and the local
/// socket keeps working.
pub async fn serve<F, Fut>(bind: String, handle: F)
where
    F: Fn(BoxRead, BoxWrite, Peer) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let config = match server_tls_config() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "remote: no usable TLS certificate — run `claudebase daemon remote enable`");
            return;
        }
    };
    let listener = match tokio::net::TcpListener::bind(&bind).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(%bind, error = %e, "remote: cannot bind");
            return;
        }
    };
    tracing::info!(%bind, "remote: listening (TLS)");
    let acceptor = tokio_rustls::TlsAcceptor::from(config);
    let handle = Arc::new(handle);
    loop {
        let (tcp, addr) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!(error = %e, "remote: accept error (continuing)");
                continue;
            }
        };
        let (acceptor, handle) = (acceptor.clone(), handle.clone());
        tokio::spawn(async move {
            let accepted = tokio::time::timeout(AUTH_TIMEOUT, async {
                let tls = acceptor.accept(tcp).await.context("TLS handshake")?;
                let (mut read, mut write) = tokio::io::split(tls);
                let client = server_auth(&mut read, &mut write).await?;
                anyhow::Ok((read, write, client))
            })
            .await;
            match accepted {
                Ok(Ok((read, write, client))) => {
                    tracing::info!(%addr, %client, "remote: client connected");
                    handle(Box::new(read), Box::new(write), Peer::Remote { client: client.clone() }).await;
                    tracing::info!(%addr, %client, "remote: client disconnected");
                }
                Ok(Err(e)) => tracing::warn!(%addr, error = %format!("{e:#}"), "remote: connection refused"),
                Err(_) => tracing::warn!(%addr, "remote: connection refused (no authentication in time)"),
            }
        });
    }
}

/// Read the `auth` frame and answer it. Nothing else is read before it.
async fn server_auth<R, W>(read: &mut R, write: &mut W) -> Result<String>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let frame: Value = serde_json::from_slice(&read_frame(read).await?).context("auth frame is not JSON")?;
    let id = frame.get("id").cloned().unwrap_or(Value::Null);
    let token = (frame.get("method").and_then(Value::as_str) == Some("auth"))
        .then(|| frame.pointer("/params/token").and_then(Value::as_str))
        .flatten()
        .unwrap_or("")
        .to_owned();
    let client = tokio::task::spawn_blocking(move || -> Result<Option<String>> {
        let conn = crate::daemon::chat::open_chat_db()?;
        authenticate(&conn, &token)
    })
    .await??;
    let Some(client) = client else {
        let reply = json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32001, "message": "unknown or revoked token"}});
        let _ = write_frame(write, &serde_json::to_vec(&reply)?).await;
        bail!("unknown or revoked token");
    };
    let reply = json!({"jsonrpc": "2.0", "id": id, "result": {"client": client, "daemon_host": hostname()}});
    write_frame(write, &serde_json::to_vec(&reply)?).await?;
    Ok(client)
}

// ---------------------------------------------------------------------------
// Client: connection strings and profiles
// ---------------------------------------------------------------------------

/// A daemon on another machine, as `claudebase://host:port?pin=sha256:…&token=…`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remote {
    pub host: String,
    pub port: u16,
    /// `sha256:<hex>` of the server certificate.
    pub pin: String,
    pub token: String,
}

impl Remote {
    pub fn parse(s: &str) -> Result<Remote> {
        let rest = s.trim().strip_prefix(SCHEME).ok_or_else(|| anyhow!("a daemon URL starts with {SCHEME}"))?;
        let (authority, query) = rest.split_once('?').ok_or_else(|| anyhow!("the URL has no ?pin=…&token=…"))?;
        let authority = authority.trim_end_matches('/');
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) if !h.ends_with(']') || h.starts_with('[') => {
                (h, p.parse::<u16>().with_context(|| format!("`{p}` is not a port"))?)
            }
            _ => (authority, DEFAULT_PORT),
        };
        let host = host.trim_start_matches('[').trim_end_matches(']').to_owned();
        if host.is_empty() {
            bail!("the URL names no host");
        }
        let mut pin = None;
        let mut token = None;
        for pair in query.split('&') {
            match pair.split_once('=') {
                Some(("pin", v)) => pin = Some(v.to_owned()),
                Some(("token", v)) => token = Some(v.to_owned()),
                _ => {}
            }
        }
        let pin = pin.ok_or_else(|| anyhow!("the URL has no pin=sha256:…"))?;
        if !pin.strip_prefix("sha256:").is_some_and(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit())) {
            bail!("pin must be sha256:<64 hex digits>");
        }
        let token = token.filter(|t| !t.is_empty()).ok_or_else(|| anyhow!("the URL has no token=…"))?;
        Ok(Remote { host, port, pin: pin.to_ascii_lowercase(), token })
    }

    pub fn url(&self) -> String {
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        format!("{SCHEME}{host}:{}?pin={}&token={}", self.port, self.pin, self.token)
    }

    /// Where it is, without the token — for listings and logs.
    pub fn display(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Profiles {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(default)]
    pub remotes: BTreeMap<String, Remote>,
}

impl Profiles {
    pub fn load() -> Result<Profiles> {
        let path = profiles_path();
        match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("{} is not valid", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Profiles::default()),
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }

    pub fn save(&self) -> Result<()> {
        write_private(&profiles_path(), toml::to_string_pretty(self)?.as_bytes())
    }
}

pub fn check_profile_name(name: &str) -> Result<()> {
    if name == LOCAL {
        bail!("`{LOCAL}` means this machine's own daemon; pick another name");
    }
    check_client_name(name).map_err(|_| anyhow!("a profile name is 1-64 characters of [a-zA-Z0-9_-]"))
}

// ---------------------------------------------------------------------------
// Client: which daemon, and connecting to it
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Endpoint {
    Local,
    Remote { profile: String, remote: Remote },
}

impl Endpoint {
    /// `CLAUDEBASE_DAEMON`, else the default profile, else local. A name that
    /// does not resolve is an error, never a silent fall back to the local
    /// daemon: a session that lands on the wrong daemon talks to the wrong
    /// agents.
    pub fn resolve() -> Result<Endpoint> {
        let from_env = std::env::var(DAEMON_ENV).ok().filter(|s| !s.trim().is_empty());
        Endpoint::named(from_env.as_deref())
    }

    /// `name` as given to `--daemon`, or `None` for the default.
    pub fn named(name: Option<&str>) -> Result<Endpoint> {
        let profiles = Profiles::load()?;
        let name = match name.map(str::trim) {
            Some(LOCAL) => return Ok(Endpoint::Local),
            Some(n) => n.to_owned(),
            None => match profiles.default.clone() {
                Some(d) => d,
                None => return Ok(Endpoint::Local),
            },
        };
        let remote = profiles.remotes.get(&name).cloned().ok_or_else(|| {
            anyhow!("no daemon profile `{name}` (claudebase daemon remote list; add one with claudebase daemon remote add)")
        })?;
        Ok(Endpoint::Remote { profile: name, remote })
    }

    pub fn is_local(&self) -> bool {
        matches!(self, Endpoint::Local)
    }

    /// For messages: `local` or `prod (prod.example:8586)`.
    pub fn describe(&self) -> String {
        match self {
            Endpoint::Local => LOCAL.to_string(),
            Endpoint::Remote { profile, remote } => format!("{profile} ({})", remote.display()),
        }
    }

    /// The value `claudebase run` exports so the session's CLI calls follow it.
    pub fn env_value(&self) -> String {
        match self {
            Endpoint::Local => LOCAL.to_string(),
            Endpoint::Remote { profile, .. } => profile.clone(),
        }
    }
}

/// Open a connection to `endpoint`. Remote: TCP, TLS with the pinned
/// certificate, then the `auth` frame — returned only once the daemon has
/// accepted the token.
pub async fn connect(endpoint: &Endpoint) -> Result<(BoxRead, BoxWrite)> {
    match endpoint {
        Endpoint::Local => {
            use interprocess::local_socket::tokio::prelude::*;
            use interprocess::local_socket::tokio::Stream;
            use interprocess::local_socket::{GenericFilePath, ToFsName};
            let socket = crate::daemon::server::socket_path()?;
            let name = socket.clone().to_fs_name::<GenericFilePath>().context("socket name")?;
            let stream = Stream::connect(name).await.with_context(|| format!("connect {}", socket.display()))?;
            let (r, w) = tokio::io::split(stream);
            Ok((Box::new(r), Box::new(w)))
        }
        Endpoint::Remote { profile, remote } => connect_remote(remote)
            .await
            .with_context(|| format!("daemon `{profile}` at {}", remote.display())),
    }
}

async fn connect_remote(remote: &Remote) -> Result<(BoxRead, BoxWrite)> {
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCert::new(&remote.pin)))
        .with_no_client_auth();
    let tcp = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::net::TcpStream::connect((remote.host.as_str(), remote.port)),
    )
    .await
    .map_err(|_| anyhow!("no answer within 10 s"))?
    .context("TCP connect")?;
    let _ = tcp.set_nodelay(true);
    // The certificate is pinned, not checked against a name; any name will do,
    // and the host itself is the honest one when it parses as one.
    let name = rustls_pki_types::ServerName::try_from(remote.host.clone())
        .unwrap_or_else(|_| rustls_pki_types::ServerName::try_from("claudebase").expect("static name"));
    let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(name, tcp)
        .await
        .context("TLS handshake (a pin mismatch means this is not the daemon the token was issued by)")?;
    let (mut read, mut write) = tokio::io::split(tls);
    let auth = json!({"jsonrpc": "2.0", "id": 0, "method": "auth", "params": {"token": remote.token}});
    write_frame(&mut write, &serde_json::to_vec(&auth)?).await?;
    let reply: Value = serde_json::from_slice(&read_frame(&mut read).await.context("read the auth answer")?)?;
    if let Some(err) = reply.get("error") {
        bail!("refused: {}", err.get("message").and_then(Value::as_str).unwrap_or("unknown error"));
    }
    Ok((Box::new(read), Box::new(write)))
}

/// Accepts exactly the certificate whose SHA-256 is the pin. Signatures are
/// still verified, so the server must hold that certificate's key.
#[derive(Debug)]
struct PinnedCert {
    pin: String,
    algorithms: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl PinnedCert {
    fn new(pin: &str) -> PinnedCert {
        PinnedCert {
            pin: pin.to_ascii_lowercase(),
            algorithms: rustls::crypto::ring::default_provider().signature_verification_algorithms,
        }
    }
}

impl rustls::client::danger::ServerCertVerifier for PinnedCert {
    fn verify_server_cert(
        &self,
        end_entity: &rustls_pki_types::CertificateDer<'_>,
        _intermediates: &[rustls_pki_types::CertificateDer<'_>],
        _server_name: &rustls_pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls_pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if format!("sha256:{}", sha256_hex(end_entity.as_ref())) == self.pin {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("certificate does not match the pinned sha256".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls_pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls_pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PIN: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn connection_strings_round_trip_and_reject_what_is_missing() {
        let r = Remote::parse(&format!("claudebase://prod.example:9000?pin={PIN}&token=cbr_x")).unwrap();
        assert_eq!((r.host.as_str(), r.port, r.token.as_str()), ("prod.example", 9000, "cbr_x"));
        assert_eq!(Remote::parse(&r.url()).unwrap(), r);
        let d = Remote::parse(&format!("claudebase://10.0.0.5?pin={PIN}&token=t")).unwrap();
        assert_eq!(d.port, DEFAULT_PORT);
        let v6 = Remote::parse(&format!("claudebase://[::1]:7?pin={PIN}&token=t")).unwrap();
        assert_eq!((v6.host.as_str(), v6.port), ("::1", 7));
        assert_eq!(Remote::parse(&v6.url()).unwrap(), v6);
        for bad in [
            format!("https://prod?pin={PIN}&token=t"),
            "claudebase://prod?token=t".to_string(),
            format!("claudebase://prod?pin={PIN}"),
            "claudebase://prod?pin=sha256:abc&token=t".to_string(),
            format!("claudebase://prod:notaport?pin={PIN}&token=t"),
        ] {
            assert!(Remote::parse(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_remote_connection_reaches_the_session_surface_only() {
        for ok in ["agent_send", "agent_register", "chat_subscribe", "journal_add", "journal_import", "chat_reply"] {
            assert!(remote_may_call(ok), "{ok}");
        }
        for no in ["agent_reap", "telegram_addbot", "callback_rotate", "remote_issue", "", "anything_new"] {
            assert!(!remote_may_call(no), "{no}");
        }
    }

    #[test]
    fn tokens_are_stored_hashed_revocable_and_named() {
        let conn = Connection::open_in_memory().unwrap();
        let t = issue_token(&conn, "laptop").unwrap();
        assert!(t.starts_with("cbr_") && t.len() > 60);
        let stored: String = conn.query_row("SELECT sha256 FROM remote_tokens", [], |r| r.get(0)).unwrap();
        assert_ne!(stored, t);
        assert_eq!(authenticate(&conn, &t).unwrap().as_deref(), Some("laptop"));
        assert_eq!(authenticate(&conn, "cbr_wrong").unwrap(), None);
        // Re-issuing under the same name retires the old token.
        let t2 = issue_token(&conn, "laptop").unwrap();
        assert_eq!(authenticate(&conn, &t).unwrap(), None);
        assert!(revoke_token(&conn, "laptop").unwrap());
        assert_eq!(authenticate(&conn, &t2).unwrap(), None);
        assert!(issue_token(&conn, "bad name").is_err());
    }
}
