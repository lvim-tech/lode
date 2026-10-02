// lode/net: the network seam a driver dials through — plain or SSH.
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

use crate::spec::TunnelSpec;

/// Resolves the address a driver should dial, applying an SSH tunnel when one is
/// configured. Cheap to clone/pass by value; the established tunnel (if any) is
/// held behind a shared cell and moved out by the server to live for the whole
/// connection.
#[derive(Clone, Default)]
pub struct NetContext {
    tunnel: Option<TunnelSpec>,
    #[cfg(feature = "tunnel")]
    established: Arc<tokio::sync::Mutex<Option<tunnel::TunnelGuard>>>,
}

impl NetContext {
    pub fn new(tunnel: Option<TunnelSpec>) -> Self {
        Self {
            tunnel,
            #[cfg(feature = "tunnel")]
            established: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    /// The `host:port` a TCP driver should connect to for the server endpoint
    /// `(host, port)` — the endpoint itself, or a local forwarded port when an
    /// SSH tunnel is configured (established lazily on first use, then reused).
    pub async fn resolve(&self, host: &str, port: u16) -> anyhow::Result<String> {
        match &self.tunnel {
            None => Ok(format!("{host}:{port}")),
            #[cfg(feature = "tunnel")]
            Some(spec) => {
                let mut guard = self.established.lock().await;
                if guard.is_none() {
                    *guard = Some(tunnel::TunnelGuard::open(spec, host, port).await?);
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
                tokio::time::timeout(timeout, tokio::net::TcpStream::connect(&addr))
                    .await
                    .map_err(|_| {
                        anyhow::anyhow!("timed out dialling {addr} after {}s", timeout.as_secs())
                    })?
                    .map_err(|e| anyhow::anyhow!("cannot reach {addr}: {e}"))?;
                Ok(format!("{addr} reachable (plain TCP)"))
            }
            #[cfg(feature = "tunnel")]
            Some(spec) => {
                let mut guard = self.established.lock().await;
                if guard.is_none() {
                    *guard = Some(tunnel::TunnelGuard::open(spec, host, port).await?);
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
}

#[cfg(feature = "tunnel")]
mod tunnel {
    use std::sync::Arc;

    use russh::client::{self, Handler};
    use russh::keys::PublicKey;
    use tokio::net::TcpListener;

    use crate::spec::{TunnelAuth, TunnelSpec};

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
        pub async fn open(spec: &TunnelSpec, db_host: &str, db_port: u16) -> anyhow::Result<Self> {
            let config = Arc::new(client::Config::default());
            let mut session = client::connect(config, (spec.host.as_str(), spec.port), ClientHandler)
                .await
                .map_err(|e| anyhow::anyhow!("ssh connect to {}:{} failed: {e}", spec.host, spec.port))?;

            // Authenticate with the chosen method.
            let authed = match &spec.auth {
                TunnelAuth::Password { password } => {
                    let pw = password.resolve().await?;
                    session
                        .authenticate_password(&spec.user, pw)
                        .await
                        .map_err(|e| anyhow::anyhow!("ssh password auth failed: {e}"))?
                        .success()
                }
                TunnelAuth::Key { path, passphrase } => {
                    let pass = passphrase.resolve().await?;
                    let pass_opt = if pass.is_empty() { None } else { Some(pass.as_str()) };
                    let key = russh::keys::load_secret_key(path, pass_opt)
                        .map_err(|e| anyhow::anyhow!("cannot load ssh key '{path}': {e}"))?;
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
    use std::time::Duration;

    const QUICK: Duration = Duration::from_secs(3);

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
