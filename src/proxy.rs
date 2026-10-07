// lode/proxy: dialling through a SOCKS5 or HTTP CONNECT proxy.
//
// One function, `dial`, which hands back a connected `TcpStream` that speaks to
// the far endpoint. Everything above it — the SSH tunnel, every TCP driver —
// then works exactly as before, because what it gets back is an ordinary
// stream. That is the same trick the tunnel uses and for the same reason: a
// proxy with per-driver code would be fourteen places to get it wrong.
//
// The name is resolved BY THE PROXY, not here. Both protocols can carry a host
// name, and sending the name is the point: on the far side of a proxy the
// client's DNS is usually the wrong DNS, and an address resolved locally would
// be an address from the wrong network.
//
// Both handshakes are written out rather than taken from a crate. They are
// about eighty lines between them, they are the only two this needs, and the
// failure messages are the whole value of the thing — "the proxy refused:
// connection not allowed by ruleset" is what somebody can act on, and a crate
// that maps every refusal onto one error type takes exactly that away.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::spec::{ProxyKind, ProxySpec};

/// Connect to `host:port` THROUGH the proxy, and hand back the stream.
pub async fn dial(spec: &ProxySpec, host: &str, port: u16) -> anyhow::Result<TcpStream> {
    let at = format!("{}:{}", spec.host, spec.port);
    let mut s = TcpStream::connect(&at)
        .await
        .map_err(|e| anyhow::anyhow!("cannot reach the proxy at {at}: {e}"))?;
    // Nagle off: both handshakes are a few short writes that each need an
    // answer, and waiting for a full segment turns a millisecond into forty.
    let _ = s.set_nodelay(true);
    let (user, pass) = match &spec.auth {
        Some(a) => (a.user.clone(), a.password.resolve().await?),
        None => (String::new(), String::new()),
    };
    match spec.kind {
        ProxyKind::Socks5 => socks5(&mut s, host, port, &user, &pass).await?,
        ProxyKind::HttpConnect => http_connect(&mut s, host, port, &user, &pass).await?,
    }
    Ok(s)
}

// ── SOCKS5 (RFC 1928, and RFC 1929 for the username/password method) ──────────

const SOCKS5: u8 = 0x05;
const NO_AUTH: u8 = 0x00;
const USER_PASS: u8 = 0x02;
const NO_ACCEPTABLE: u8 = 0xFF;
const CMD_CONNECT: u8 = 0x01;
const ATYP_IPV4: u8 = 0x01;
const ATYP_DOMAIN: u8 = 0x03;
const ATYP_IPV6: u8 = 0x04;

/// What the proxy said when it refused, in its own words rather than ours.
fn socks5_reply(code: u8) -> &'static str {
    match code {
        0x00 => "ok",
        0x01 => "general SOCKS server failure",
        0x02 => "connection not allowed by ruleset",
        0x03 => "network unreachable",
        0x04 => "host unreachable",
        0x05 => "connection refused",
        0x06 => "TTL expired",
        0x07 => "command not supported",
        0x08 => "address type not supported",
        _ => "unknown reply code",
    }
}

async fn socks5(
    s: &mut TcpStream,
    host: &str,
    port: u16,
    user: &str,
    pass: &str,
) -> anyhow::Result<()> {
    // Offer only what we can actually do. Offering username/password with no
    // credentials would be choosing a method we would then fail.
    let methods: &[u8] = if user.is_empty() { &[NO_AUTH] } else { &[NO_AUTH, USER_PASS] };
    let mut greet = vec![SOCKS5, methods.len() as u8];
    greet.extend_from_slice(methods);
    s.write_all(&greet).await?;

    let mut reply = [0u8; 2];
    s.read_exact(&mut reply).await.map_err(|e| {
        anyhow::anyhow!("the proxy closed before answering the SOCKS5 greeting: {e}")
    })?;
    if reply[0] != SOCKS5 {
        anyhow::bail!(
            "that is not a SOCKS5 proxy — it answered version {} (if it is an HTTP proxy, choose HTTP CONNECT)",
            reply[0]
        );
    }
    match reply[1] {
        NO_AUTH => {}
        USER_PASS => {
            if user.is_empty() {
                anyhow::bail!("the proxy asked for a username and password, and none is configured");
            }
            if user.len() > 255 || pass.len() > 255 {
                anyhow::bail!("SOCKS5 allows at most 255 bytes of username and of password");
            }
            let mut m = vec![0x01u8, user.len() as u8];
            m.extend_from_slice(user.as_bytes());
            m.push(pass.len() as u8);
            m.extend_from_slice(pass.as_bytes());
            s.write_all(&m).await?;
            let mut r = [0u8; 2];
            s.read_exact(&mut r).await?;
            if r[1] != 0 {
                anyhow::bail!("the proxy rejected the username and password");
            }
        }
        NO_ACCEPTABLE => anyhow::bail!(
            "the proxy accepts none of the authentication methods this offered{}",
            if user.is_empty() { " (no credentials are configured — it may want some)" } else { "" }
        ),
        other => anyhow::bail!("the proxy chose SOCKS5 authentication method {other}, which this does not speak"),
    }

    // The request. The HOST NAME goes over the wire, not an address: the proxy's
    // DNS is the one on the right network.
    if host.len() > 255 {
        anyhow::bail!("a host name for SOCKS5 may be at most 255 bytes");
    }
    let mut req = vec![SOCKS5, CMD_CONNECT, 0x00, ATYP_DOMAIN, host.len() as u8];
    req.extend_from_slice(host.as_bytes());
    req.extend_from_slice(&port.to_be_bytes());
    s.write_all(&req).await?;

    let mut head = [0u8; 4];
    s.read_exact(&mut head)
        .await
        .map_err(|e| anyhow::anyhow!("the proxy closed before answering the CONNECT: {e}"))?;
    if head[1] != 0 {
        anyhow::bail!(
            "the proxy refused to connect to {host}:{port} — {}",
            socks5_reply(head[1])
        );
    }
    // The bound address comes back and has to be read off the socket, or it
    // would be the first bytes the driver sees.
    let skip = match head[3] {
        ATYP_IPV4 => 4,
        ATYP_IPV6 => 16,
        ATYP_DOMAIN => {
            let mut n = [0u8; 1];
            s.read_exact(&mut n).await?;
            n[0] as usize
        }
        other => anyhow::bail!("the proxy answered with address type {other}, which this does not understand"),
    };
    let mut rest = vec![0u8; skip + 2];
    s.read_exact(&mut rest).await?;
    Ok(())
}

// ── HTTP CONNECT ──────────────────────────────────────────────────────────────

async fn http_connect(
    s: &mut TcpStream,
    host: &str,
    port: u16,
    user: &str,
    pass: &str,
) -> anyhow::Result<()> {
    use base64::Engine as _;
    let target = format!("{host}:{port}");
    let mut req = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n");
    if !user.is_empty() {
        let token = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
        req.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
    }
    req.push_str("Proxy-Connection: Keep-Alive\r\n\r\n");
    s.write_all(req.as_bytes()).await?;

    // Read exactly up to the blank line and NO FURTHER. Anything after it
    // belongs to the tunnelled conversation, and swallowing one byte of it
    // corrupts whatever the driver says first.
    let mut head = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        let n = s.read(&mut byte).await?;
        if n == 0 {
            anyhow::bail!("the proxy closed before finishing its answer to CONNECT");
        }
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
        if head.len() > 8192 {
            anyhow::bail!("the proxy's answer to CONNECT had no end of headers in 8 KiB");
        }
    }
    let text = String::from_utf8_lossy(&head);
    let status = text.lines().next().unwrap_or("").to_string();
    let code = status.split_whitespace().nth(1).and_then(|c| c.parse::<u16>().ok());
    match code {
        Some(c) if (200..300).contains(&c) => Ok(()),
        Some(407) => Err(anyhow::anyhow!(
            "the proxy wants authentication{}",
            if user.is_empty() { " and none is configured" } else { ", and rejected what was configured" }
        )),
        // The status line verbatim: "403 Forbidden" and "502 Bad Gateway" are
        // different problems, and summarising them loses which.
        Some(_) => Err(anyhow::anyhow!("the proxy refused to connect to {target} — {status}")),
        None => Err(anyhow::anyhow!("the proxy answered something that is not HTTP: {status:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::ProxyAuth;
    use crate::secret::Secret;
    use tokio::net::TcpListener;

    fn spec(kind: ProxyKind, port: u16, user: Option<&str>) -> ProxySpec {
        ProxySpec {
            kind,
            host: "127.0.0.1".into(),
            port,
            auth: user.map(|u| ProxyAuth {
                user: u.into(),
                password: Secret::new("pw"),
            }),
        }
    }

    /// A SOCKS5 proxy that says yes, records what it was asked for, and then
    /// echoes. Real sockets on loopback: the handshake is a wire format, and a
    /// test that mocked the socket would be testing the mock.
    async fn fake_socks5(want_auth: bool, reply_code: u8) -> (u16, tokio::sync::oneshot::Receiver<String>) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut head = [0u8; 2];
            s.read_exact(&mut head).await.unwrap();
            let mut methods = vec![0u8; head[1] as usize];
            s.read_exact(&mut methods).await.unwrap();
            if want_auth {
                s.write_all(&[SOCKS5, USER_PASS]).await.unwrap();
                let mut n = [0u8; 2];
                s.read_exact(&mut n).await.unwrap();
                let mut user = vec![0u8; n[1] as usize];
                s.read_exact(&mut user).await.unwrap();
                let mut pl = [0u8; 1];
                s.read_exact(&mut pl).await.unwrap();
                let mut pass = vec![0u8; pl[0] as usize];
                s.read_exact(&mut pass).await.unwrap();
                s.write_all(&[0x01, 0x00]).await.unwrap();
            } else {
                s.write_all(&[SOCKS5, NO_AUTH]).await.unwrap();
            }
            let mut req = [0u8; 5];
            s.read_exact(&mut req).await.unwrap();
            let mut host = vec![0u8; req[4] as usize];
            s.read_exact(&mut host).await.unwrap();
            let mut p = [0u8; 2];
            s.read_exact(&mut p).await.unwrap();
            let asked = format!(
                "{}:{}",
                String::from_utf8_lossy(&host),
                u16::from_be_bytes(p)
            );
            s.write_all(&[SOCKS5, reply_code, 0x00, ATYP_IPV4, 0, 0, 0, 0, 0, 0])
                .await
                .unwrap();
            let _ = tx.send(asked);
            if reply_code == 0 {
                let mut buf = [0u8; 16];
                if let Ok(n) = s.read(&mut buf).await {
                    let _ = s.write_all(&buf[..n]).await;
                }
            }
        });
        (port, rx)
    }

    /// The host NAME goes over the wire. On the far side of a proxy the
    /// client's DNS is usually the wrong DNS, so an address resolved here would
    /// be an address from the wrong network.
    #[tokio::test]
    async fn socks5_sends_the_host_name_and_hands_back_a_usable_stream() {
        let (port, asked) = fake_socks5(false, 0x00).await;
        let mut s = dial(&spec(ProxyKind::Socks5, port, None), "db.internal", 5432)
            .await
            .unwrap();
        assert_eq!(asked.await.unwrap(), "db.internal:5432");
        // And the stream is the far conversation, with no handshake left on it.
        s.write_all(b"hello").await.unwrap();
        let mut back = [0u8; 5];
        s.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"hello");
    }

    #[tokio::test]
    async fn socks5_does_the_username_password_round_when_asked() {
        let (port, asked) = fake_socks5(true, 0x00).await;
        dial(&spec(ProxyKind::Socks5, port, Some("alice")), "db.internal", 5432)
            .await
            .unwrap();
        assert_eq!(asked.await.unwrap(), "db.internal:5432");
    }

    /// The refusal reaches the caller IN THE PROXY'S OWN TERMS. "Connection not
    /// allowed by ruleset" and "host unreachable" are different problems and
    /// send somebody to different places.
    #[tokio::test]
    async fn a_socks5_refusal_keeps_its_reason() {
        for (code, word) in [(0x02u8, "ruleset"), (0x04, "host unreachable"), (0x05, "refused")] {
            let (port, _asked) = fake_socks5(false, code).await;
            let e = dial(&spec(ProxyKind::Socks5, port, None), "db.internal", 5432)
                .await
                .unwrap_err()
                .to_string();
            assert!(e.contains(word), "code {code:#x} gave: {e}");
            assert!(e.contains("db.internal:5432"), "{e}");
        }
    }

    /// Pointing SOCKS5 at an HTTP proxy is a common mistake, and the message
    /// should say which one it is rather than "protocol error".
    #[tokio::test]
    async fn a_proxy_that_is_not_socks5_says_so_and_suggests_the_other_one() {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut b = [0u8; 3];
            let _ = s.read_exact(&mut b).await;
            let _ = s.write_all(b"HT").await;
        });
        let e = dial(&spec(ProxyKind::Socks5, port, None), "db", 5432)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("not a SOCKS5 proxy"), "{e}");
        assert!(e.contains("HTTP CONNECT"), "{e}");
    }

    /// An HTTP CONNECT proxy, with the thing that is easy to get wrong: the
    /// bytes after the blank line belong to the far conversation.
    async fn fake_http(status: &'static str, trailing: &'static [u8]) -> (u16, tokio::sync::oneshot::Receiver<String>) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut head = Vec::new();
            let mut b = [0u8; 1];
            while s.read_exact(&mut b).await.is_ok() {
                head.push(b[0]);
                if head.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            let _ = tx.send(String::from_utf8_lossy(&head).to_string());
            let _ = s.write_all(status.as_bytes()).await;
            let _ = s.write_all(trailing).await;
        });
        (port, rx)
    }

    #[tokio::test]
    async fn http_connect_asks_for_the_endpoint_and_leaves_the_body_alone() {
        let (port, req) = fake_http("HTTP/1.1 200 Connection established\r\n\r\n", b"SERVER-HELLO").await;
        let mut s = dial(&spec(ProxyKind::HttpConnect, port, None), "db.internal", 27017)
            .await
            .unwrap();
        let text = req.await.unwrap();
        assert!(text.starts_with("CONNECT db.internal:27017 HTTP/1.1\r\n"), "{text}");
        assert!(text.contains("Host: db.internal:27017"), "{text}");
        // The first bytes the driver reads must be the SERVER's, not a leftover
        // scrap of the proxy's headers.
        let mut buf = [0u8; 12];
        s.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"SERVER-HELLO");
    }

    #[tokio::test]
    async fn http_connect_sends_basic_credentials_when_there_are_some() {
        let (port, req) = fake_http("HTTP/1.1 200 OK\r\n\r\n", b"").await;
        dial(&spec(ProxyKind::HttpConnect, port, Some("alice")), "db", 5432)
            .await
            .unwrap();
        let text = req.await.unwrap();
        // base64("alice:pw")
        assert!(text.contains("Proxy-Authorization: Basic YWxpY2U6cHc="), "{text}");
    }

    #[tokio::test]
    async fn an_http_refusal_keeps_its_status_line() {
        let (port, _r) = fake_http("HTTP/1.1 403 Forbidden\r\n\r\n", b"").await;
        let e = dial(&spec(ProxyKind::HttpConnect, port, None), "db", 5432)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("403 Forbidden"), "{e}");
    }

    /// 407 is its own case because the fix differs depending on whether any
    /// credentials were configured at all.
    #[tokio::test]
    async fn a_407_says_whether_anything_was_even_offered() {
        let (port, _r) = fake_http("HTTP/1.1 407 Proxy Authentication Required\r\n\r\n", b"").await;
        let e = dial(&spec(ProxyKind::HttpConnect, port, None), "db", 5432)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("none is configured"), "{e}");

        let (port, _r) = fake_http("HTTP/1.1 407 Proxy Authentication Required\r\n\r\n", b"").await;
        let e = dial(&spec(ProxyKind::HttpConnect, port, Some("alice")), "db", 5432)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("rejected what was configured"), "{e}");
    }

    /// A proxy that is not there names the proxy, not the database: the two
    /// failures look identical from a driver and send somebody to the wrong
    /// machine.
    #[tokio::test]
    async fn an_absent_proxy_names_the_proxy() {
        // Bind and drop, so the port is almost certainly free.
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        let e = dial(&spec(ProxyKind::Socks5, port, None), "db.internal", 5432)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("cannot reach the proxy at 127.0.0.1"), "{e}");
        assert!(!e.contains("db.internal"), "the database is not what failed: {e}");
    }
}
