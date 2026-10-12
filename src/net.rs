// lode/net: the network seam a driver dials through — plain, proxied, or SSH.
//
// Every TCP driver asks the NetContext for the address to connect to. With no
// tunnel it returns the server's own host:port. With an SSH tunnel it stands up
// a LOCAL forwarder (a 127.0.0.1 listener) whose every accepted connection is
// bridged over an SSH `direct-tcpip` channel to the real DB endpoint — so the
// driver dials a local port and its whole DB conversation rides an ENCRYPTED SSH
// channel, with ZERO per-driver code. This is the encrypted path for engines
// with no native TLS (and a defence-in-depth option for those that have it).
//
// A required tunnel is NEVER silently bypassed: if the SSH session can't be
// established, connect fails rather than falling back to a direct link.

#[cfg(feature = "tunnel")]
use std::sync::Arc;

use crate::spec::{ProxySpec, TlsSpec, TunnelSpec};

/// What a dial really went through, as a sentence for the audit log and the
/// test stages. Assembled here because this is the only place that knows.
fn path_words(proxy: Option<&ProxySpec>, tunnelled: bool) -> String {
    match (proxy, tunnelled) {
        (None, false) => "plain TCP".into(),
        (None, true) => "an SSH tunnel".into(),
        (Some(p), false) => format!("a {} proxy at {}:{}", p.kind.label(), p.host, p.port),
        (Some(p), true) => format!(
            "a {} proxy at {}:{} and then an SSH tunnel",
            p.kind.label(),
            p.host,
            p.port
        ),
    }
}

/// Resolves the address a driver should dial, applying an SSH tunnel when one is
/// configured. Cheap to clone/pass by value; the established tunnel (if any) is
/// held behind a shared cell and moved out by the server to live for the whole
/// connection.
#[derive(Clone, Default)]
pub struct NetContext {
    tunnel: Option<TunnelSpec>,
    /// A proxy UNDER everything: the database's dial and the SSH server's dial
    /// both go through it. A connection may have both, and the order is the
    /// only one the two things can mean — a proxy is how this machine reaches
    /// anything at all, and a tunnel is what rides on a reachable SSH server.
    proxy: Option<ProxySpec>,
    #[cfg(feature = "tunnel")]
    established: Arc<tokio::sync::Mutex<Option<tunnel::TunnelGuard>>>,
    /// The local forwarder for a PROXIED, untunnelled connection.
    ///
    /// A proxy cannot be expressed as a host:port for a driver to dial, so the
    /// same answer the tunnel already uses applies: bind a loopback port and
    /// bridge each accepted connection through the proxy. Zero per-driver code,
    /// which is the whole point of this file.
    forwarded: Arc<tokio::sync::Mutex<Option<ProxyGuard>>>,
}

impl NetContext {
    pub fn new(tunnel: Option<TunnelSpec>) -> Self {
        Self::with_proxy(tunnel, None)
    }

    pub fn with_proxy(tunnel: Option<TunnelSpec>, proxy: Option<ProxySpec>) -> Self {
        Self {
            tunnel,
            proxy,
            #[cfg(feature = "tunnel")]
            established: Arc::new(tokio::sync::Mutex::new(None)),
            forwarded: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    pub fn proxy(&self) -> Option<&ProxySpec> {
        self.proxy.as_ref()
    }

    /// Open one TCP stream to `host:port`, through the proxy when there is one.
    ///
    /// Everything in this file that reaches the network goes through here, so
    /// "is there a proxy" is answered once rather than at each call site.
    pub async fn dial(&self, host: &str, port: u16) -> anyhow::Result<tokio::net::TcpStream> {
        match &self.proxy {
            Some(p) => crate::proxy::dial(p, host, port).await,
            None => tokio::net::TcpStream::connect(format!("{host}:{port}"))
                .await
                .map_err(|e| anyhow::anyhow!("cannot reach {host}:{port}: {e}")),
        }
    }

    /// The `host:port` a TCP driver should connect to for the server endpoint
    /// `(host, port)` — the endpoint itself, or a local forwarded port when an
    /// SSH tunnel is configured (established lazily on first use, then reused).
    pub async fn resolve(&self, host: &str, port: u16) -> anyhow::Result<String> {
        match &self.tunnel {
            // No tunnel, but a proxy: the driver still gets a plain address, it
            // is just a loopback one with the proxy behind it.
            None if self.proxy.is_some() => {
                let mut guard = self.forwarded.lock().await;
                if guard.is_none() {
                    *guard = Some(ProxyGuard::open(self.proxy.clone().unwrap(), host, port).await?);
                }
                Ok(guard.as_ref().unwrap().local_addr().to_string())
            }
            None => Ok(format!("{host}:{port}")),
            #[cfg(feature = "tunnel")]
            Some(spec) => {
                let mut guard = self.established.lock().await;
                if guard.is_none() {
                    *guard = Some(tunnel::TunnelGuard::open(spec, self.proxy.as_ref(), host, port).await?);
                }
                Ok(guard.as_ref().unwrap().local_addr().to_string())
            }
            #[cfg(not(feature = "tunnel"))]
            Some(_) => Err(anyhow::anyhow!(
                "this daemon build has no SSH-tunnel support (build with the 'tunnel' feature)"
            )),
        }
    }

    /// Prove the DB endpoint ANSWERS — not merely that there is somewhere to
    /// dial. This is what `conn.test`'s endpoint stage asks, and the difference
    /// is the whole value of the stage.
    ///
    /// Without a tunnel the two are the same thing: dialling the server's own
    /// host:port either connects or does not.
    ///
    /// WITH a tunnel they are emphatically not. `resolve` returns the address of
    /// the LOCAL listener this process just bound, and `TcpStream::connect` to it
    /// succeeds the instant that listener exists — the far side has not been
    /// asked anything yet, because the `direct-tcpip` channel is opened per
    /// accepted connection, inside the forwarding task, where its failure is
    /// invisible to the caller. A green light meant "the SSH session came up",
    /// and a MongoDB connection pointed at 127.0.0.1:9042 with nothing listening
    /// reported `reachable through the SSH tunnel` in 251 ms.
    ///
    /// So the tunnelled probe does not go near the local listener. It opens a
    /// `direct-tcpip` channel on the session itself and reports what the SSH
    /// SERVER said: the server performs the connection to `host:port` and
    /// answers with a channel, or refuses with a reason. That answer is about the
    /// endpoint, which is what was asked.
    pub async fn probe(&self, host: &str, port: u16, timeout: std::time::Duration) -> anyhow::Result<String> {
        match &self.tunnel {
            None => {
                let addr = format!("{host}:{port}");
                // Through a proxy this is a REAL end-to-end proof and not a
                // local one: the proxy answers the CONNECT only when it has
                // reached the far side, so a refusal here is the far side's.
                tokio::time::timeout(timeout, self.dial(host, port))
                    .await
                    .map_err(|_| {
                        anyhow::anyhow!("timed out dialling {addr} after {}s", timeout.as_secs())
                    })??;
                Ok(format!(
                    "{addr} reachable through {}",
                    path_words(self.proxy.as_ref(), false)
                ))
            }
            #[cfg(feature = "tunnel")]
            Some(spec) => {
                let mut guard = self.established.lock().await;
                if guard.is_none() {
                    *guard = Some(tunnel::TunnelGuard::open(spec, self.proxy.as_ref(), host, port).await?);
                }
                let g = guard.as_ref().unwrap();
                tokio::time::timeout(timeout, g.probe())
                    .await
                    .map_err(|_| {
                        anyhow::anyhow!(
                            "the SSH server did not answer a forward request to {host}:{port} within {}s",
                            timeout.as_secs()
                        )
                    })??;
                Ok(format!(
                    "{host}:{port} reachable THROUGH the SSH tunnel — the SSH server opened a \
                     direct-tcpip channel to it (the remote end answered, not just the tunnel)"
                ))
            }
            #[cfg(not(feature = "tunnel"))]
            Some(_) => Err(anyhow::anyhow!(
                "this engine build has no SSH-tunnel support (build with the 'tunnel' feature)"
            )),
        }
    }

    /// Whether this context carries an SSH tunnel (its link is encrypted end to
    /// end regardless of the driver's native TLS).
    pub fn tunneled(&self) -> bool {
        self.tunnel.is_some()
    }

    /// Whether the address `resolve` hands back STANDS IN for the server: a
    /// loopback port this engine listens on, because the link travels through
    /// an SSH tunnel or a proxy.
    pub fn forwarded(&self) -> bool {
        self.tunnel.is_some() || self.proxy.is_some()
    }

    /// Refuse a posture that cannot be kept, instead of failing in the
    /// handshake.
    ///
    /// `verify_full` checks the name ON the certificate against the address
    /// dialled — and through a tunnel or a proxy that address is a loopback
    /// port of this machine's own, which no server certificate names. The
    /// handshake then dies with "NotValidForName", a message that says nothing
    /// about the tunnel that caused it, and the honest answer is `verify_ca`:
    /// the chain is still verified (see `crate::tls`), and the channel to that
    /// port is the SSH channel, authenticated by the server's host key.
    ///
    /// A driver whose library can be told the name SEPARATELY from the address
    /// does not call this — postgres has `hostaddr`, and the Redis driver does
    /// its own handshake. Both verify the real name through a tunnel.
    pub fn refuse_unverifiable_name(&self, tls: &TlsSpec) -> anyhow::Result<()> {
        if tls.verifies_hostname() && self.forwarded() {
            anyhow::bail!(
                "verify_full cannot be kept through {}: the certificate names the server, and what this \
                 driver dials is a local port of this machine — use verify_ca, which still verifies the \
                 certificate chain against the CA",
                if self.tunnel.is_some() { "an SSH tunnel" } else { "a proxy" }
            );
        }
        Ok(())
    }

    /// How a dial on this context actually travels, in words.
    pub fn path(&self) -> String {
        path_words(self.proxy.as_ref(), self.tunnel.is_some())
    }
}

/// A loopback listener that bridges every accepted connection through a proxy
/// to one endpoint.
///
/// The same shape as the SSH forwarder above, and for the same reason: a driver
/// knows how to dial a `host:port` and nothing else, so anything that is not a
/// `host:port` is turned into one here rather than in fourteen drivers.
pub struct ProxyGuard {
    local_addr: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ProxyGuard {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl ProxyGuard {
    pub fn local_addr(&self) -> &str {
        &self.local_addr
    }

    pub async fn open(spec: ProxySpec, host: &str, port: u16) -> anyhow::Result<ProxyGuard> {
        // Prove the proxy works BEFORE handing a driver a port that looks fine.
        // The first dial is the proof and is then thrown away: a listener that
        // accepts and fails on every connection is exactly the shape of bug the
        // tunnel's endpoint stage was written to stop telling.
        crate::proxy::dial(&spec, host, port).await?;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| anyhow::anyhow!("cannot bind a local port for the proxy: {e}"))?;
        let local_addr = listener.local_addr()?.to_string();
        let host = host.to_string();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut inbound, _)) = listener.accept().await else { break };
                let spec = spec.clone();
                let host = host.clone();
                tokio::spawn(async move {
                    let Ok(mut out) = crate::proxy::dial(&spec, &host, port).await else {
                        // Dropping `inbound` closes the driver's socket, so it
                        // reports a connection failure rather than hanging. The
                        // proxy's reason is lost HERE, which is why `probe`
                        // dials the proxy itself rather than inferring anything
                        // from this path.
                        return;
                    };
                    let _ = tokio::io::copy_bidirectional(&mut inbound, &mut out).await;
                });
            }
        });
        Ok(ProxyGuard { local_addr, task })
    }
}

#[cfg(feature = "tunnel")]
mod tunnel {
    use std::sync::Arc;

    use russh::client::{self, Handler};
    use russh::keys::PublicKey;
    use tokio::net::TcpListener;

    use crate::spec::{ProxySpec, TunnelAuth, TunnelSpec};

    /// A running SSH local-forward. Holds the SSH session + the accept task; on
    /// drop the task is aborted and the session closes.
    pub struct TunnelGuard {
        local_addr: String,
        /// The endpoint this forward was stood up for, kept so the guard can be
        /// asked about the FAR side rather than only about its own listener.
        host: String,
        port: u16,
        task: tokio::task::JoinHandle<()>,
        session: Arc<client::Handle<ClientHandler>>,
    }

    impl Drop for TunnelGuard {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl TunnelGuard {
        pub fn local_addr(&self) -> &str {
            &self.local_addr
        }

        /// Ask the SSH SERVER to connect to the forwarded endpoint, and close the
        /// channel again. Succeeds only when the server actually reached it: an
        /// SSH server that cannot connect answers CHANNEL_OPEN_FAILURE with a
        /// reason, which arrives here as an error carrying that reason.
        ///
        /// This deliberately does NOT go through the local listener. Connecting
        /// to that proves only that this process bound a socket.
        pub async fn probe(&self) -> anyhow::Result<()> {
            let channel = self
                .session
                .channel_open_direct_tcpip(self.host.clone(), self.port as u32, "127.0.0.1", 0)
                .await
                .map_err(|e| {
                    anyhow::anyhow!(
                        "the SSH server could not reach {}:{} — {e}",
                        self.host,
                        self.port
                    )
                })?;
            let _ = channel.close().await;
            Ok(())
        }

        /// Establish the SSH session, authenticate, and start forwarding a fresh
        /// local port to `(db_host, db_port)` over the tunnel.
        pub async fn open(
            spec: &TunnelSpec,
            proxy: Option<&ProxySpec>,
            db_host: &str,
            db_port: u16,
        ) -> anyhow::Result<Self> {
            let config = Arc::new(client::Config::default());
            // The SSH SERVER is dialled through the proxy too, when there is
            // one. A proxy that fronted the database but not the bastion would
            // be a proxy that is sometimes bypassed, and "sometimes bypassed"
            // is the one thing a network rule must never be.
            let mut session = match proxy {
                Some(p) => {
                    let stream = crate::proxy::dial(p, &spec.host, spec.port).await.map_err(|e| {
                        anyhow::anyhow!(
                            "reaching the SSH server {}:{} through the {} proxy failed: {e}",
                            spec.host,
                            spec.port,
                            p.kind.label()
                        )
                    })?;
                    client::connect_stream(config, stream, ClientHandler)
                        .await
                        .map_err(|e| {
                            anyhow::anyhow!("ssh handshake with {}:{} failed: {e}", spec.host, spec.port)
                        })?
                }
                None => client::connect(config, (spec.host.as_str(), spec.port), ClientHandler)
                    .await
                    .map_err(|e| anyhow::anyhow!("ssh connect to {}:{} failed: {e}", spec.host, spec.port))?,
            };

            // Authenticate with the chosen method.
            let authed = match &spec.auth {
                TunnelAuth::Password { password } => {
                    let pw = password.resolve().await?;
                    // The resolved value is alive only here, so this is where a
                    // failure that quotes it can be cleaned. It dies at the end
                    // of this arm, as it always did.
                    crate::audit::scrubbing(
                        &pw,
                        session
                            .authenticate_password(&spec.user, pw.clone())
                            .await
                            .map_err(|e| anyhow::anyhow!("ssh password auth failed: {e}")),
                    )?
                    .success()
                }
                TunnelAuth::Key { path, passphrase } => {
                    let pass = passphrase.resolve().await?;
                    let pass_opt = if pass.is_empty() { None } else { Some(pass.as_str()) };
                    // Both the passphrase and the path come out of this message.
                    // The path because the engine cannot tell a path from a
                    // passphrase typed where a path goes — which is the mistake
                    // that put a credential in somebody's audit log.
                    let key = crate::audit::scrubbing(
                        &pass,
                        russh::keys::load_secret_key(path, pass_opt)
                            .map_err(|e| anyhow::anyhow!("cannot load ssh key '{path}': {e}")),
                    )?;
                    let with_hash = russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key), None);
                    session
                        .authenticate_publickey(&spec.user, with_hash)
                        .await
                        .map_err(|e| anyhow::anyhow!("ssh key auth failed: {e}"))?
                        .success()
                }
                TunnelAuth::Agent => {
                    let mut agent = russh::keys::agent::client::AgentClient::connect_env()
                        .await
                        .map_err(|e| anyhow::anyhow!("ssh agent unavailable: {e}"))?;
                    let identities = agent
                        .request_identities()
                        .await
                        .map_err(|e| anyhow::anyhow!("ssh agent has no identities: {e}"))?;
                    let mut ok = false;
                    for id in identities {
                        let res = session
                            .authenticate_publickey_with(&spec.user, id, None, &mut agent)
                            .await;
                        if let Ok(r) = res {
                            if r.success() {
                                ok = true;
                                break;
                            }
                        }
                    }
                    ok
                }
            };
            if !authed {
                return Err(anyhow::anyhow!("ssh authentication was rejected"));
            }

            let session = Arc::new(session);
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .map_err(|e| anyhow::anyhow!("cannot bind local forward port: {e}"))?;
            let local_addr = listener.local_addr()?.to_string();

            let host = db_host.to_string();
            let sess = session.clone();
            let task = tokio::spawn(async move {
                loop {
                    let (mut inbound, _peer) = match listener.accept().await {
                        Ok(v) => v,
                        Err(_) => break,
                    };
                    let sess = sess.clone();
                    let host = host.clone();
                    tokio::spawn(async move {
                        // Each local connection gets its own direct-tcpip channel.
                        let channel = match sess
                            .channel_open_direct_tcpip(host, db_port as u32, "127.0.0.1", 0)
                            .await
                        {
                            Ok(c) => c,
                            // The far side refused. Returning drops `inbound`,
                            // which closes the driver's socket, so the driver
                            // reports a connection failure rather than hanging —
                            // but the SSH server's REASON is lost here, which is
                            // exactly why the endpoint stage asks the session
                            // directly (see `NetContext::probe`) instead of
                            // inferring anything from this path.
                            Err(_) => return,
                        };
                        let mut stream = channel.into_stream();
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut stream).await;
                    });
                }
            });

            Ok(TunnelGuard {
                local_addr,
                host: db_host.to_string(),
                port: db_port,
                task,
                session,
            })
        }
    }

    /// The SSH client handler. Server-key acceptance is permissive for now (the
    /// channel is still fully encrypted); strict known_hosts verification is a
    /// follow-up (noted in findings.md).
    pub struct ClientHandler;

    impl Handler for ClientHandler {
        type Error = russh::Error;

        async fn check_server_key(&mut self, _server_public_key: &PublicKey) -> Result<bool, Self::Error> {
            Ok(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::TlsMode;
    use std::time::Duration;

    const QUICK: Duration = Duration::from_secs(3);

    fn a_tunnel() -> TunnelSpec {
        serde_json::from_value(serde_json::json!({
            "host": "bastion.example",
            "user": "me",
            "auth": { "kind": "agent" }
        }))
        .expect("a tunnel")
    }

    fn a_proxy() -> ProxySpec {
        serde_json::from_value(serde_json::json!({
            "kind": "socks5",
            "host": "127.0.0.1",
            "port": 1080
        }))
        .expect("a proxy")
    }

    fn posture(mode: TlsMode) -> TlsSpec {
        TlsSpec {
            mode,
            ..Default::default()
        }
    }

    /// The message somebody reads instead of "NotValidForName".
    #[test]
    fn verify_full_is_refused_where_the_name_cannot_be_checked() {
        for (what, net) in [
            ("tunnel", NetContext::new(Some(a_tunnel()))),
            ("proxy", NetContext::with_proxy(None, Some(a_proxy()))),
        ] {
            let said = net.refuse_unverifiable_name(&posture(TlsMode::VerifyFull));
            let e = said.expect_err("verify_full cannot be kept through a local port").to_string();
            assert!(e.contains("verify_ca"), "{what}: the message has to say what to do: {e}");
        }
    }

    /// And every other posture goes through, including the strict one that
    /// does not depend on the name.
    #[test]
    fn every_other_posture_travels_through_a_tunnel() {
        let net = NetContext::new(Some(a_tunnel()));
        for mode in [
            TlsMode::Disable,
            TlsMode::Prefer,
            TlsMode::Require,
            TlsMode::VerifyCa,
        ] {
            assert!(
                net.refuse_unverifiable_name(&posture(mode)).is_ok(),
                "{} is not about the name",
                mode.label()
            );
        }
    }

    #[test]
    fn a_direct_link_verifies_the_name_as_it_always_did() {
        let net = NetContext::new(None);
        assert!(!net.forwarded());
        assert!(net.refuse_unverifiable_name(&posture(TlsMode::VerifyFull)).is_ok());
    }

    /// Without a tunnel the probe is a real dial, and it has to say no when
    /// there is nothing there. Real sockets on loopback; no network.
    #[tokio::test]
    async fn an_untunnelled_probe_tells_the_truth() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let net = NetContext::new(None);

        let ok = net.probe("127.0.0.1", addr.port(), QUICK).await;
        assert!(ok.is_ok(), "a bound port must be reachable: {ok:?}");
        assert!(ok.unwrap().contains("plain TCP"));

        drop(listener);

        // A port with nothing on it. The port just freed is the obvious
        // candidate and it is NOT reliable: these tests run in parallel and any
        // of them may be handed that number by the kernel in the gap. So the
        // claim is retried on a fresh port, and only a run where every attempt
        // found something listening would fail — which is not a flaky test, it
        // is a machine where this cannot be asked.
        let mut last = String::new();
        let mut proved = false;
        for _ in 0..8 {
            let probe_port = {
                let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let p = l.local_addr().unwrap().port();
                drop(l);
                p
            };
            match net.probe("127.0.0.1", probe_port, QUICK).await {
                Ok(_) => continue, // someone took it; ask about another one
                Err(e) => {
                    last = e.to_string();
                    assert!(
                        last.contains(&probe_port.to_string()),
                        "the error must name the port: {last}"
                    );
                    proved = true;
                    break;
                }
            }
        }
        assert!(proved, "no free port stayed free long enough to probe; last: {last}");
    }

    /// The probe of a tunnelled spec must never be answered by the LOCAL
    /// forwarder. This is the shape of the bug: `resolve` hands back a listener
    /// this process bound, so anything that dials it succeeds regardless of the
    /// far side. Here the SSH host does not exist, so the probe must fail while
    /// trying to reach the SSH server — never by reporting the endpoint fine.
    #[cfg(feature = "tunnel")]
    #[tokio::test]
    async fn a_tunnelled_probe_is_not_answered_by_the_local_forwarder() {
        use crate::spec::{TunnelAuth, TunnelSpec};
        let net = NetContext::new(Some(TunnelSpec {
            host: "ssh.invalid".into(),
            port: 22,
            user: "nobody".into(),
            auth: TunnelAuth::Agent,
        }));
        let res = net.probe("127.0.0.1", 9042, QUICK).await;
        assert!(res.is_err(), "an unreachable SSH host cannot yield a reachable endpoint");
        let msg = res.unwrap_err().to_string();
        assert!(
            !msg.contains("reachable"),
            "the failure must not be worded as a success: {msg}"
        );
    }
}
