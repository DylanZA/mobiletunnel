use std::io;
use std::net::Ipv4Addr;

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
        let _ = env_logger::try_init();
        let server_listener = TcpListener::bind("127.0.0.1:0").await?;
        let server_port = server_listener.local_addr().unwrap().port();
        let proxy = Proxy::new(server_port).await?;
        log::trace!("Setting up streams...");
        let (c, cs) = reconnecting_stream::StreamState::default();
        let (s, ss) = reconnecting_stream::StreamState::default();
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
        harness
            .client
            .sender
            .send("FC".as_bytes().to_vec())
            .await
            .unwrap();
        harness
            .server
            .sender
            .send("FS".as_bytes().to_vec())
            .await
            .unwrap();
        assert!(harness.client_recv(2).await.unwrap() == "FS".as_bytes());
        assert!(harness.server_recv(2).await.unwrap() == "FC".as_bytes());
        harness
            .stop()
            .await
            .swallow_or_print_err("shutting down test harness");
    }
}

#[tokio::test()]
async fn test_with_disconnect() {
    let mut harness = Harness::new().await.unwrap();
    harness
        .client
        .sender
        .send("FC".as_bytes().to_vec())
        .await
        .unwrap();
    harness
        .server
        .sender
        .send("FS".as_bytes().to_vec())
        .await
        .unwrap();
    harness.send_disconnect().unwrap();
    harness.send_connect().unwrap();
    harness
        .client
        .sender
        .send("FC".as_bytes().to_vec())
        .await
        .unwrap();
    harness
        .server
        .sender
        .send("FS".as_bytes().to_vec())
        .await
        .unwrap();
    assert!(harness.client_recv(4).await.unwrap() == "FSFS".as_bytes());
    assert!(harness.server_recv(4).await.unwrap() == "FCFC".as_bytes());
    harness
        .stop()
        .await
        .swallow_or_print_err("shutting down test harness");
}
