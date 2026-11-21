//! The tunnels against real SSH servers, through [`Tunnels`] as the app
//! drives them. Skipped unless the variables name the servers, as
//! `user:password@host:port`:
//!
//! - `S3V_TEST_TUNNELS`: a server that allows forwarding (and
//!   `GatewayPorts clientspecified`), e.g. `dev:devpass123@127.0.0.1:2240`;
//! - `S3V_TEST_TUNNELS_TARGET`: an HTTP server only that server reaches,
//!   e.g. `web:80`;
//! - `S3V_TEST_TUNNELS_REFUSED` (optional): a server with forwarding
//!   disabled, e.g. `dev:devpass123@127.0.0.1:2241`;
//! - `S3V_TEST_TUNNELS_PUBLISHED` (optional): a port of the first server
//!   published on this computer, e.g. `2242`.
//!
//! Local ports 15000 to 15099 are used.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener as StdListener, TcpStream as StdStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::*;

struct Server {
    settings: Arc<SshSettings>,
}

fn ssh_server(variable: &str) -> Option<Server> {
    let value = std::env::var(variable).ok().filter(|value| !value.is_empty())?;
    let (credentials, address) = value.rsplit_once('@')?;
    let (user, password) = credentials.split_once(':')?;
    let (host, port) = address.rsplit_once(':')?;
    Some(Server {
        settings: Arc::new(SshSettings {
            extra: Default::default(),
            host: host.to_owned(),
            port: port.parse().ok()?,
            user: user.to_owned(),
            secret: password.to_owned(),
            key_path: None,
            known_hosts: None,
        }),
    })
}

/// The HTTP server behind the SSH server, as that server names it.
fn target() -> Option<(String, u16)> {
    let value = std::env::var("S3V_TEST_TUNNELS_TARGET").ok()?;
    let (host, port) = value.rsplit_once(':')?;
    Some((host.to_owned(), port.parse().ok()?))
}

/// The app's side of things: a worker and the tunnels running on it.
struct Bench {
    /// Runs the tunnels: kept for as long as they are.
    _worker: Worker,
    _events: std::sync::mpsc::Receiver<crate::worker::Event>,
    tunnels: Tunnels,
}

impl Bench {
    fn new(settings: &Arc<SshSettings>) -> Self {
        let (worker, events) = Worker::new(egui::Context::default());
        let mut tunnels = Tunnels::default();
        tunnels.open(&worker, "test", settings.clone(), String::new());
        Self {
            _worker: worker,
            _events: events,
            tunnels,
        }
    }

    fn start(&mut self, rule: &ForwardRule) {
        assert!(self.tunnels.start("test", rule.clone()));
    }

    fn host(&self) -> &Host {
        self.tunnels.host("test").expect("the host is open")
    }

    /// Waits for the rule to be listening, or to have given up.
    fn settled(&self, rule: &ForwardRule) -> RuleState {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let state = self.host().with_state(|state| {
                state
                    .rules
                    .iter()
                    .find(|run| run.rule.id == rule.id)
                    .map(|run| run.state.clone())
            });
            match state {
                Some(state @ (RuleState::Active(_) | RuleState::Failed(_))) => return state,
                _ if Instant::now() > deadline => panic!("the tunnel never settled: {state:?}, {:?}", self.host().link()),
                _ => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }

    fn active(&self, rule: &ForwardRule) {
        let state = self.settled(rule);
        assert!(matches!(state, RuleState::Active(_)), "{state:?}");
    }

    /// The error of the last connection that could not go through.
    fn refusal(&self, rule: &ForwardRule) -> Option<String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let error = self.host().with_state(|state| {
                state
                    .rules
                    .iter()
                    .find(|run| run.rule.id == rule.id)
                    .and_then(|run| run.last_error.as_ref().map(|(error, _)| error.clone()))
            });
            if error.is_some() || Instant::now() > deadline {
                return error;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn stats(&self, rule: &ForwardRule) -> (u64, u64, u64) {
        self.host().with_state(|state| {
            let run = state.rules.iter().find(|run| run.rule.id == rule.id).expect("running");
            let stats = &run.stats;
            (
                stats.total.load(Ordering::Relaxed),
                stats.sent.load(Ordering::Relaxed),
                stats.received.load(Ordering::Relaxed),
            )
        })
    }

    fn wait_link(&self, wanted: impl Fn(&Link) -> bool, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(40);
        while !wanted(&self.host().link()) {
            assert!(Instant::now() < deadline, "{what}: still {:?}", self.host().link());
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn local(port: u16, host: &str, to: u16) -> ForwardRule {
    ForwardRule {
        bind_port: port,
        dest_host: host.to_owned(),
        dest_port: to,
        ..ForwardRule::default()
    }
}

fn get(stream: &mut StdStream) -> String {
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    stream
        .write_all(b"GET / HTTP/1.0\r\nHost: web\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut answer = String::new();
    let _ = stream.read_to_string(&mut answer);
    answer
}

fn http(port: u16) -> String {
    match StdStream::connect(("127.0.0.1", port)) {
        Ok(mut stream) => get(&mut stream),
        Err(error) => format!("connexion impossible : {error}"),
    }
}

/// Asks the proxy for `host:port` with SOCKS5 and returns its reply code
/// and the stream, ready for the request.
fn socks5(proxy: u16, host: &str, port: u16) -> (u8, StdStream) {
    let mut stream = StdStream::connect(("127.0.0.1", proxy)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    stream.write_all(&[5, 1, 0]).unwrap();
    let mut greeting = [0u8; 2];
    stream.read_exact(&mut greeting).unwrap();
    assert_eq!(greeting, [5, 0]);
    let mut request = vec![5, 1, 0, 3, host.len() as u8];
    request.extend_from_slice(host.as_bytes());
    request.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&request).unwrap();
    let mut reply = [0u8; 10];
    stream.read_exact(&mut reply).unwrap();
    (reply[1], stream)
}

/// A tiny HTTP server on this computer, answering `marker` to anything.
fn serve_here(port: u16, marker: &'static str) {
    let listener = StdListener::bind(("127.0.0.1", port)).unwrap();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request);
            let body = format!("HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n{marker}", marker.len());
            let _ = stream.write_all(body.as_bytes());
        }
    });
}

#[test]
fn local_forward_reaches_a_host_only_the_server_sees() {
    let (Some(server), Some((host, port))) = (ssh_server("S3V_TEST_TUNNELS"), target()) else {
        return;
    };
    let mut bench = Bench::new(&server.settings);
    let rule = local(15001, &host, port);
    bench.start(&rule);
    bench.active(&rule);

    for _ in 0..3 {
        assert!(http(15001).contains("tunnels-marker"), "the page behind the server");
    }
    let (total, sent, received) = bench.stats(&rule);
    assert_eq!(total, 3);
    assert!(sent > 100 && received > 100, "{sent} sent, {received} received");

    // Restarted, it takes its port again once the old listener let go.
    for _ in 0..3 {
        bench.start(&rule);
        bench.active(&rule);
        assert!(http(15001).contains("tunnels-marker"), "works after a restart");
    }

    bench.tunnels.stop("test", &rule.id);
    assert!(bench.tunnels.host("test").is_none(), "its last tunnel closes the connection");
    std::thread::sleep(Duration::from_millis(300));
    assert!(StdListener::bind(("127.0.0.1", 15001)).is_ok(), "the port is free again");
}

#[test]
fn dynamic_forward_is_a_socks_proxy() {
    let (Some(server), Some((host, port))) = (ssh_server("S3V_TEST_TUNNELS"), target()) else {
        return;
    };
    let mut bench = Bench::new(&server.settings);
    let rule = ForwardRule {
        kind: ForwardKind::Dynamic,
        bind_port: 15002,
        ..ForwardRule::default()
    };
    bench.start(&rule);
    bench.active(&rule);

    // The name is resolved by the server: this computer does not know it.
    let (code, mut stream) = socks5(15002, &host, port);
    assert_eq!(code, 0);
    assert!(get(&mut stream).contains("tunnels-marker"));

    // SOCKS4a, the same way.
    let mut stream = StdStream::connect(("127.0.0.1", 15002)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut request = vec![4, 1];
    request.extend_from_slice(&port.to_be_bytes());
    request.extend_from_slice(&[0, 0, 0, 1, 0]);
    request.extend_from_slice(host.as_bytes());
    request.push(0);
    stream.write_all(&request).unwrap();
    let mut reply = [0u8; 8];
    stream.read_exact(&mut reply).unwrap();
    assert_eq!(reply[1], 0x5a);
    assert!(get(&mut stream).contains("tunnels-marker"));

    // Nothing listens there: the client is told, and so is the user.
    let (code, _) = socks5(15002, &host, port + 1);
    assert_eq!(code, socks::Refusal::HostUnreachable as u8);
    let error = bench.refusal(&rule).expect("the failure is reported");
    assert!(error.contains("injoignable depuis le serveur"), "{error}");
}

#[test]
fn remote_forward_brings_connections_back_here() {
    let Some(server) = ssh_server("S3V_TEST_TUNNELS") else {
        return;
    };
    serve_here(15003, "remote-marker");
    let mut bench = Bench::new(&server.settings);
    let rule = ForwardRule {
        kind: ForwardKind::Remote,
        bind_port: 18003,
        dest_host: "127.0.0.1".to_owned(),
        dest_port: 15003,
        ..ForwardRule::default()
    };
    bench.start(&rule);
    bench.active(&rule);

    // Seen from the server: another SSH connection opens its port 18003.
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let answer = runtime.block_on(async {
        let handle = ssh::connect(&server.settings).await.expect("second connection");
        let channel = handle
            .channel_open_direct_tcpip("127.0.0.1", 18003, "127.0.0.1", 1)
            .await
            .expect("the server listens on 18003");
        let mut stream = channel.into_stream();
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        stream.write_all(b"GET / HTTP/1.0\r\n\r\n").await.unwrap();
        let mut answer = String::new();
        let _ = tokio::time::timeout(Duration::from_secs(10), stream.read_to_string(&mut answer)).await;
        answer
    });
    assert!(answer.contains("remote-marker"), "{answer}");
    assert_eq!(bench.stats(&rule).0, 1);

    // Bound to every address of the server, through a port it publishes.
    if let Some(published) = std::env::var("S3V_TEST_TUNNELS_PUBLISHED").ok().and_then(|port| port.parse().ok()) {
        let open = ForwardRule {
            kind: ForwardKind::Remote,
            bind_host: "0.0.0.0".to_owned(),
            bind_port: published,
            dest_host: "127.0.0.1".to_owned(),
            dest_port: 15003,
            ..ForwardRule::default()
        };
        bench.start(&open);
        bench.active(&open);
        assert!(http(published).contains("remote-marker"));
    }

    // Nothing listens here any more on that port: the server is refused.
    let dead = ForwardRule {
        kind: ForwardKind::Remote,
        bind_port: 18004,
        dest_host: "127.0.0.1".to_owned(),
        dest_port: 15099,
        ..ForwardRule::default()
    };
    bench.start(&dead);
    bench.active(&dead);
    runtime.block_on(async {
        let handle = ssh::connect(&server.settings).await.expect("second connection");
        let channel = handle.channel_open_direct_tcpip("127.0.0.1", 18004, "127.0.0.1", 1).await;
        if let Ok(channel) = channel {
            let mut stream = channel.into_stream();
            use tokio::io::AsyncReadExt;
            let mut answer = Vec::new();
            let _ = tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut answer)).await;
            assert!(answer.is_empty(), "nothing comes back");
        }
    });
    let error = bench.refusal(&dead).expect("the failure is reported");
    assert!(error.contains("Rien n'écoute sur 127.0.0.1:15099"), "{error}");

    // The server's port is released with the tunnel.
    bench.tunnels.stop("test", &rule.id);
    std::thread::sleep(Duration::from_millis(500));
    let still = runtime.block_on(async {
        let handle = ssh::connect(&server.settings).await.expect("second connection");
        handle.channel_open_direct_tcpip("127.0.0.1", 18003, "127.0.0.1", 1).await.is_ok()
    });
    assert!(!still, "the server no longer listens on 18003");
}

#[test]
fn errors_are_explained() {
    let (Some(server), Some((host, port))) = (ssh_server("S3V_TEST_TUNNELS"), target()) else {
        return;
    };
    let mut bench = Bench::new(&server.settings);

    let _taken = StdListener::bind(("127.0.0.1", 15005)).unwrap();
    let busy = local(15005, &host, port);
    bench.start(&busy);
    match bench.settled(&busy) {
        RuleState::Failed(error) => assert!(error.contains("déjà utilisé"), "{error}"),
        state => panic!("{state:?}"),
    }

    let nowhere = local(15006, &host, port + 1);
    bench.start(&nowhere);
    bench.active(&nowhere);
    let answer = http(15006);
    assert!(!answer.contains("tunnels-marker"));
    let error = bench.refusal(&nowhere).expect("the failure is reported");
    assert!(error.contains(&format!("{host}:{} est injoignable", port + 1)), "{error}");

    let incomplete = ForwardRule {
        bind_port: 15007,
        ..ForwardRule::default()
    };
    bench.start(&incomplete);
    assert_eq!(bench.settled(&incomplete), RuleState::Failed("Destination manquante".to_owned()));

    let Some(refusing) = ssh_server("S3V_TEST_TUNNELS_REFUSED") else {
        return;
    };
    let mut bench = Bench::new(&refusing.settings);
    let refused = local(15008, &host, port);
    bench.start(&refused);
    bench.active(&refused);
    http(15008);
    let error = bench.refusal(&refused).expect("the failure is reported");
    assert!(error.contains("refuse les redirections"), "{error}");

    let remote = ForwardRule {
        kind: ForwardKind::Remote,
        bind_port: 18008,
        dest_host: "127.0.0.1".to_owned(),
        dest_port: 15099,
        ..ForwardRule::default()
    };
    bench.start(&remote);
    match bench.settled(&remote) {
        RuleState::Failed(error) => assert!(error.contains("refusé d'ouvrir le port 127.0.0.1:18008"), "{error}"),
        state => panic!("{state:?}"),
    }
}

#[test]
fn a_server_that_cannot_be_reached_is_not_retried_alone() {
    let Some(server) = ssh_server("S3V_TEST_TUNNELS") else {
        return;
    };
    let wrong = Arc::new(SshSettings {
        extra: Default::default(),
        secret: "not-the-password".to_owned(),
        host: server.settings.host.clone(),
        port: server.settings.port,
        user: server.settings.user.clone(),
        key_path: None,
        known_hosts: None,
    });
    let mut bench = Bench::new(&wrong);
    let rule = local(15009, "web", 80);
    bench.start(&rule);
    bench.wait_link(|link| matches!(link, Link::Failed(_)), "the failure is shown");
    let Link::Failed(error) = bench.host().link() else { unreachable!() };
    assert!(error.contains("uthentification"), "{error}");
    std::thread::sleep(Duration::from_secs(3));
    assert!(matches!(bench.host().link(), Link::Failed(_)), "no retry on its own");

    // Asked for with the right password, it connects.
    bench.tunnels.reconnect("test", Some(server.settings.clone()));
    bench.wait_link(|link| *link == Link::Connected, "reconnected when asked");
    bench.active(&rule);
}

/// Stands between the tunnels and the server, and can cut every
/// connection it carries, as a network failure would.
struct Relay {
    port: u16,
    carried: Arc<Mutex<Vec<StdStream>>>,
}

impl Relay {
    fn new(port: u16, to: (String, u16)) -> Self {
        let listener = StdListener::bind(("127.0.0.1", port)).unwrap();
        let carried: Arc<Mutex<Vec<StdStream>>> = Arc::default();
        let kept = carried.clone();
        std::thread::spawn(move || {
            for client in listener.incoming().flatten() {
                let Ok(upstream) = StdStream::connect((to.0.as_str(), to.1)) else {
                    continue;
                };
                let pipe = |mut from: StdStream, mut to: StdStream| {
                    std::thread::spawn(move || {
                        let _ = std::io::copy(&mut from, &mut to);
                        let _ = to.shutdown(Shutdown::Both);
                    });
                };
                kept.lock().unwrap().extend([client.try_clone().unwrap(), upstream.try_clone().unwrap()]);
                pipe(client.try_clone().unwrap(), upstream.try_clone().unwrap());
                pipe(upstream, client);
            }
        });
        Self { port, carried }
    }

    fn cut(&self) {
        for stream in self.carried.lock().unwrap().drain(..) {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

#[test]
fn a_dropped_link_connects_again() {
    let (Some(server), Some((host, port))) = (ssh_server("S3V_TEST_TUNNELS"), target()) else {
        return;
    };
    let relay = Relay::new(15010, (server.settings.host.clone(), server.settings.port));
    let through = Arc::new(SshSettings {
        extra: Default::default(),
        host: "127.0.0.1".to_owned(),
        port: relay.port,
        user: server.settings.user.clone(),
        secret: server.settings.secret.clone(),
        key_path: None,
        known_hosts: None,
    });
    let mut bench = Bench::new(&through);
    let rule = local(15011, &host, port);
    bench.start(&rule);
    bench.active(&rule);
    assert!(http(15011).contains("tunnels-marker"));

    relay.cut();
    bench.wait_link(|link| matches!(link, Link::Lost { retry: Some(_), .. }), "the drop is shown");
    bench.wait_link(|link| *link == Link::Connected, "connected again");
    bench.active(&rule);
    assert!(http(15011).contains("tunnels-marker"), "the tunnel works again");
    // The counters span the reconnection.
    assert_eq!(bench.stats(&rule).0, 2);
}
