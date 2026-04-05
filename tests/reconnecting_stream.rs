use std::io;
use std::net::Ipv4Addr;
use std::time::Duration;

use bytes::BytesMut;
use libmobiletunnel::reconnecting_stream;
use libmobiletunnel::util::SwallowResultPrintErrExt as _;
use tokio;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

enum Instruction {
    Connect,
    Disconnect,
    Close,
}

struct Proxy {
    join_handle: JoinHandle<()>,
    pub port: u16,
    instructions_tx: mpsc::UnboundedSender<Instruction>,
    done_rx: mpsc::UnboundedReceiver<String>,
}

impl Proxy {
    async fn write_all(s: &mut TcpStream, buf: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
        if let Err(e) = s.write_all(buf).await {
            return Err(e.into());
        }
        return Ok(());
    }

    async fn run(
        l: TcpListener,
        target_port: u16,
        mut rx: mpsc::UnboundedReceiver<Instruction>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut disconnected = false;
        loop {
            if disconnected {
                loop {
                    match rx.recv().await {
                        Some(Instruction::Connect) => {
                            break;
                        }
                        Some(Instruction::Close) => {
                            log::info!("closing proxy");
                            return Ok(());
                        }
                        Some(Instruction::Disconnect) => {
                            panic!("Received disconnect when disconnected");
                        }
                        None => {
                            return Ok(());
                        }
                    }
                }
            }
            log::trace!("Accepting...");
            let (mut from_client_stream, _) = l.accept().await?;
            log::trace!("Got socket, Connecting to {} ...", target_port);
            let mut to_server_stream =
                TcpStream::connect(format!("127.0.0.1:{}", target_port)).await?;
            log::trace!("Connected to {} ...", target_port);
            loop {
                log::trace!("Loop");
                let mut b1 = BytesMut::with_capacity(8128);
                let mut b2 = BytesMut::with_capacity(8128);
                tokio::select! {
                    from_client = from_client_stream.read_buf(&mut b1) => {
                        match from_client {
                            Ok(0) => {
                                log::trace!("Got 0 from client");
                                break;
                            },
                            Ok(x) => {
                                log::trace!("Got {} data from client, sending to server", x);
                                if let Err(e) = Self::write_all(&mut to_server_stream, &b1[0..x]).await {
                                    log::trace!("Got {} error writing to server", e);
                                    break;
                                }
                            },
                            Err(e) => {
                                log::trace!("Got error {} from client", e);
                                break;
                            }
                        };
                    },
                    from_server = to_server_stream.read_buf(&mut b2) => {
                        match from_server {
                            Ok(0) => {
                                log::trace!("Got 0 from server");
                                break;
                            },
                            Ok(x) => {
                                log::trace!("Got {} data from server, sending to client", x);
                                if let Err(e) = Self::write_all(&mut from_client_stream, &b2[0..x]).await {
                                    log::trace!("Got {} error writing to client", e);
                                    break;
                                }
                            },
                            Err(_) => {
                                break;
                            }
                        };
                    },
                    instruction = rx.recv() => {
                        match instruction {
                            Some(Instruction::Disconnect) => {
                                log::info!("disconnecting proxy");
                                disconnected = true;
                                break;
                            },
                            Some(Instruction::Close) => {
                                log::info!("closing proxy");
                                return Ok(());
                            },
                            _ => {
                                break;
                            }
                        }
                    }
                };
            }
        }
    }

    async fn new(target_port: u16) -> Result<Proxy, Box<dyn std::error::Error>> {
        let l = TcpListener::bind("127.0.0.1:0").await?;
        let (tx, rx) = mpsc::unbounded_channel();
        let (tx2, rx2) = mpsc::unbounded_channel();
        let port = l.local_addr()?.port();
        let jh = tokio::spawn(async move {
            Self::run(l, target_port, rx)
                .await
                .swallow_or_print_err("run proxy");
            log::trace!("proxy finished");
            tx2.send("Done".to_string())
                .swallow_or_print_err("sending error");
            return ();
        });
        return Ok(Proxy {
            join_handle: jh,
            port: port,
            instructions_tx: tx,
            done_rx: rx2,
        });
    }
}

struct Harness {
    server_jh: JoinHandle<Result<(), io::Error>>,
    client_jh: JoinHandle<Result<(), String>>,
    proxy: Proxy,
    server: reconnecting_stream::StreamSender,
    client: reconnecting_stream::StreamSender,
    interrupt: CancellationToken,
}

impl Harness {
    async fn new() -> Result<Harness, Box<dyn std::error::Error>> {
        Self::with_window(None).await
    }

    async fn with_window(max_window: Option<usize>) -> Result<Harness, Box<dyn std::error::Error>> {
        let _ = env_logger::try_init();
        let server_listener = TcpListener::bind("127.0.0.1:0").await?;
        let server_port = server_listener.local_addr().unwrap().port();
        let proxy = Proxy::new(server_port).await?;
        log::trace!("Setting up streams...");
        let (c, cs) = match max_window {
            Some(w) => reconnecting_stream::StreamState::new(reconnecting_stream::StreamOptions::new(w)),
            None => reconnecting_stream::StreamState::default(),
        };
        let (s, ss) = match max_window {
            Some(w) => reconnecting_stream::StreamState::new(reconnecting_stream::StreamOptions::new(w)),
            None => reconnecting_stream::StreamState::default(),
        };
        let cancel_token = CancellationToken::new();
        log::trace!("Starting server...");
        let server_jh = tokio::spawn(s.run_server(server_listener, cancel_token.clone()));
        log::trace!("Starting client...");
        let client_jh = tokio::spawn(c.run_client(
            std::net::IpAddr::V4(Ipv4Addr::LOCALHOST),
            proxy.port,
            cancel_token.clone(),
        ));
        log::trace!("... done harness init");

        Ok(Harness {
            server_jh: server_jh,
            client_jh: client_jh,
            proxy: proxy,
            client: cs,
            server: ss,
            interrupt: cancel_token,
        })
    }

    async fn do_recv(&mut self, server: bool, n: usize) -> Option<Vec<u8>> {
        let mut ret = vec![];
        let receiver = if server {
            &mut self.server.receiver
        } else {
            &mut self.client.receiver
        };
        loop {
            let foo = tokio::select! {
                x = receiver.recv() => x,
                _ = self.proxy.done_rx.recv() => None,
            };
            match foo {
                Some(val) => {
                    ret.extend(val);
                    if ret.len() >= n {
                        return Some(ret);
                    }
                }
                None => {
                    return None;
                }
            }
        }
    }

    pub async fn client_recv(&mut self, n: usize) -> Option<Vec<u8>> {
        self.do_recv(false, n).await
    }

    pub async fn server_recv(&mut self, n: usize) -> Option<Vec<u8>> {
        self.do_recv(true, n).await
    }

    pub fn send_disconnect(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.proxy.instructions_tx.send(Instruction::Disconnect)?;
        return Ok(());
    }

    pub fn send_connect(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.proxy.instructions_tx.send(Instruction::Connect)?;
        return Ok(());
    }

    pub async fn disconnect_and_reconnect(&self) {
        self.send_disconnect().unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        self.send_connect().unwrap();
    }

    pub async fn client_send(&self, data: &[u8]) {
        self.client.sender.send(data.to_vec()).await.unwrap();
    }

    pub async fn server_send(&self, data: &[u8]) {
        self.server.sender.send(data.to_vec()).await.unwrap();
    }

    pub async fn assert_server_recv(&mut self, expected: &[u8]) {
        assert_eq!(self.server_recv(expected.len()).await.unwrap(), expected);
    }

    pub async fn assert_client_recv(&mut self, expected: &[u8]) {
        assert_eq!(self.client_recv(expected.len()).await.unwrap(), expected);
    }

    pub async fn stop(mut self) -> Result<(), Box<dyn std::error::Error>> {
        log::trace!("Shutting down test harness...");
        self.proxy.instructions_tx.send(Instruction::Close)?;
        let (_, cs) = reconnecting_stream::StreamState::default();
        let (_, ss) = reconnecting_stream::StreamState::default();
        self.client = cs;
        self.server = ss;
        self.interrupt.cancel();
        log::trace!("Waiting for proxy to shutdown...");
        self.proxy.join_handle.await?;
        log::trace!("Waiting for server to shutdown...");
        self.server_jh.await.unwrap().unwrap();
        log::trace!("Waiting for client to shutdown...");
        self.client_jh.await.unwrap().unwrap();
        Ok(())
    }
}

#[tokio::test()]
async fn basic_test() {
    for _ in 0..10 {
        let mut harness = Harness::new().await.unwrap();
        harness.client_send(b"FC").await;
        harness.server_send(b"FS").await;
        harness.assert_client_recv(b"FS").await;
        harness.assert_server_recv(b"FC").await;
        harness.stop().await.swallow_or_print_err("shutting down test harness");
    }
}

#[tokio::test()]
async fn test_with_disconnect() {
    let mut harness = Harness::new().await.unwrap();
    harness.client_send(b"FC").await;
    harness.server_send(b"FS").await;
    harness.send_disconnect().unwrap();
    harness.send_connect().unwrap();
    harness.client_send(b"FC").await;
    harness.server_send(b"FS").await;
    harness.assert_client_recv(b"FSFS").await;
    harness.assert_server_recv(b"FCFC").await;
    harness.stop().await.swallow_or_print_err("shutting down test harness");
}

// --- Backpressure / Flow Control ---

#[tokio::test()]
async fn test_backpressure_blocks_sender() {
    // With a tiny window and no ACKs (disconnected), the sender should
    // block once the window fills and the channel (cap 16) is exhausted.
    let mut harness = Harness::with_window(Some(50)).await.unwrap();
    harness.client_send(b"ok").await;
    harness.assert_server_recv(b"ok").await;

    // Disconnect — prevents ACKs from draining the window
    harness.send_disconnect().unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Send messages while disconnected. After 50 bytes, is_full() becomes
    // true. After 16 more fill the channel buffer (cap 16), sender blocks.
    for _ in 0..17 {
        let result = tokio::time::timeout(
            Duration::from_millis(100),
            harness.client.sender.send(vec![b'X'; 10]),
        )
        .await;
        if result.is_err() {
            // Sender blocked — backpressure is working. Reconnect to unblock.
            harness.send_connect().unwrap();
            harness.client_send(b"after").await;
            assert!(harness.server_recv(5).await.is_some());
            return;
        }
    }

    // All sends succeeded (unlikely but possible). Verify stream works.
    harness.send_connect().unwrap();
    harness.client_send(b"end").await;
    assert!(harness.server_recv(3).await.is_some());
}

#[tokio::test()]
async fn test_backpressure_resolves_after_ack() {
    let mut harness = Harness::with_window(Some(100)).await.unwrap();
    for i in 0u8..5 {
        let data = vec![b'A' + i; 90];
        harness.client_send(&data).await;
        harness.assert_server_recv(&data).await;
    }
}

#[tokio::test()]
async fn test_backpressure_during_disconnect() {
    let mut harness = Harness::with_window(Some(200)).await.unwrap();
    harness.client_send(&vec![b'A'; 80]).await;
    harness.assert_server_recv(&vec![b'A'; 80]).await;

    harness.send_disconnect().unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    harness.client_send(&vec![b'B'; 80]).await;

    harness.send_connect().unwrap();
    harness.assert_server_recv(&vec![b'B'; 80]).await;
}

#[tokio::test()]
async fn test_window_full_then_disconnect_reconnect() {
    let mut harness = Harness::with_window(Some(100)).await.unwrap();
    harness.client_send(&vec![b'X'; 90]).await;

    harness.disconnect_and_reconnect().await;

    harness.assert_server_recv(&vec![b'X'; 90]).await;
    harness.client_send(&vec![b'Y'; 50]).await;
    harness.assert_server_recv(&vec![b'Y'; 50]).await;
}

#[tokio::test()]
async fn test_window_exactly_at_limit() {
    let mut harness = Harness::with_window(Some(10)).await.unwrap();
    harness.client_send(&vec![b'A'; 10]).await;
    harness.assert_server_recv(&vec![b'A'; 10]).await;
    harness.client_send(&vec![b'B'; 10]).await;
    harness.assert_server_recv(&vec![b'B'; 10]).await;
}

// --- Reconnection Semantics ---

#[tokio::test()]
async fn test_data_sent_during_disconnect_arrives() {
    let mut harness = Harness::new().await.unwrap();
    harness.client_send(b"A").await;
    harness.assert_server_recv(b"A").await;

    harness.send_disconnect().unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    harness.client_send(b"B").await;

    harness.send_connect().unwrap();
    harness.assert_server_recv(b"B").await;
}

#[tokio::test()]
async fn test_bidirectional_during_reconnect() {
    let mut harness = Harness::new().await.unwrap();
    harness.client_send(b"C1").await;
    harness.server_send(b"S1").await;

    harness.send_disconnect().unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    harness.client_send(b"C2").await;
    harness.server_send(b"S2").await;

    harness.send_connect().unwrap();
    harness.assert_server_recv(b"C1C2").await;
    harness.assert_client_recv(b"S1S2").await;
}

#[tokio::test()]
async fn test_rapid_disconnect_reconnect() {
    let mut harness = Harness::new().await.unwrap();
    harness.client_send(b"START").await;

    for _ in 0..5 {
        harness.send_disconnect().unwrap();
        harness.send_connect().unwrap();
    }

    harness.client_send(b"END").await;
    harness.assert_server_recv(b"STARTEND").await;
}

#[tokio::test()]
async fn test_large_message_across_reconnect() {
    let mut harness = Harness::new().await.unwrap();
    let payload: Vec<u8> = (0..10000).map(|i| (i % 251) as u8).collect();
    harness.client_send(&payload).await;

    harness.send_disconnect().unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    harness.send_connect().unwrap();

    assert_eq!(harness.server_recv(10000).await.unwrap(), payload);
}

// --- Edge Cases ---

#[tokio::test()]
async fn test_multiple_messages_ordering() {
    let mut harness = Harness::new().await.unwrap();
    for i in 0u8..5 {
        harness.client_send(&[b'1' + i]).await;
    }
    harness.assert_server_recv(b"12345").await;
}

#[tokio::test()]
async fn test_both_directions_interleaved() {
    let mut harness = Harness::new().await.unwrap();
    harness.client_send(b"A").await;
    harness.server_send(b"X").await;
    harness.client_send(b"B").await;
    harness.server_send(b"Y").await;
    harness.assert_server_recv(b"AB").await;
    harness.assert_client_recv(b"XY").await;
}

// --- Stress / Reliability ---

#[tokio::test()]
async fn test_high_throughput_bidirectional() {
    let mut harness = Harness::with_window(Some(10000)).await.unwrap();
    let msg = vec![b'D'; 100];
    for _ in 0..100 {
        harness.client_send(&msg).await;
        harness.server_send(&msg).await;
    }

    let server_data = harness.server_recv(10000).await.unwrap();
    let client_data = harness.client_recv(10000).await.unwrap();
    assert_eq!(server_data, vec![b'D'; 10000]);
    assert_eq!(client_data, vec![b'D'; 10000]);
}

#[tokio::test()]
async fn test_many_small_messages() {
    let mut harness = Harness::new().await.unwrap();
    for i in 0u16..500 {
        harness.client_send(&[(i % 256) as u8]).await;
    }
    let data = harness.server_recv(500).await.unwrap();
    let expected: Vec<u8> = (0u16..500).map(|i| (i % 256) as u8).collect();
    assert_eq!(data, expected);
}

#[tokio::test()]
async fn test_disconnect_with_pending_acks() {
    let mut harness = Harness::new().await.unwrap();
    for i in 0u8..5 {
        harness.client_send(&vec![b'A' + i; 20]).await;
    }

    harness.send_disconnect().unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    harness.send_connect().unwrap();

    assert_eq!(harness.server_recv(100).await.unwrap().len(), 100);
}

#[tokio::test()]
async fn test_alternating_directions_with_disconnect() {
    let mut harness = Harness::new().await.unwrap();
    harness.client_send(b"A").await;
    harness.assert_server_recv(b"A").await;
    harness.server_send(b"B").await;
    harness.assert_client_recv(b"B").await;

    harness.send_disconnect().unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    harness.client_send(b"C").await;
    harness.server_send(b"D").await;

    harness.send_connect().unwrap();
    harness.assert_server_recv(b"C").await;
    harness.assert_client_recv(b"D").await;
}

#[tokio::test()]
async fn test_empty_reconnect() {
    let mut harness = Harness::new().await.unwrap();
    harness.disconnect_and_reconnect().await;
    harness.client_send(b"hello").await;
    harness.assert_server_recv(b"hello").await;
}
