use bytes::BytesMut;
use libmobiletunnel::stream_multiplexer;
use libmobiletunnel::util::SwallowResultPrintErrExt as _;
use std::time::Duration;
use tokio;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

struct Server {
    _join_handle: JoinHandle<()>,
    pub port: u16,
}

impl Server {
    async fn run_one(
        mut l: TcpStream,
        ct: CancellationToken,
    ) -> Result<(), Box<dyn std::error::Error>> {
        loop {
            let mut b = BytesMut::with_capacity(8128);
            tokio::select! {
                _ = l.read_buf(&mut b) => {
                    l.write_all(&b[..]).await?;
                },
                _ = ct.cancelled() => {
                    break;
                }
            }
        }
        return Ok(());
    }

    async fn run(l: TcpListener, ct: CancellationToken) -> Result<(), Box<dyn std::error::Error>> {
        let mut handles = vec![];
        loop {
            tokio::select! {
                _ = ct.cancelled() => {
                    log::trace!("stopping server");
                    break;
                },
                t = l.accept() => {
                    let (stream, _) = t?;
                    let ct2 = ct.child_token();
                    let jh = tokio::spawn(async move {
                        Server::run_one(stream, ct2).await.swallow_or_print_err("server::run_one");
                        return ();
                    });
                    handles.push(jh);
                }
            }
        }
        return Ok(());
    }

    async fn new(ct: CancellationToken) -> Result<Server, Box<dyn std::error::Error>> {
        let l = TcpListener::bind("localhost:0").await?;
        let port = l.local_addr()?.port();
        log::info!("server listening on port {}", port);
        let jh = tokio::spawn(async move {
            let res = Self::run(l, ct).await;
            log::trace!("server finished");
            if let Err(e) = res {
                log::error!("Got error {}", e);
            }
            return ();
        });
        return Ok(Server {
            _join_handle: jh,
            port: port,
        });
    }
}

struct Harness {
    _server: Server,
    _interrupt: CancellationToken,
    client_port: u16,
    _tasks: Vec<JoinHandle<()>>,
}

impl Harness {
    fn shim(
        mut foo: mpsc::Receiver<Vec<u8>>,
    ) -> (mpsc::UnboundedReceiver<Vec<u8>>, JoinHandle<()>) {
        let (tx, ret) = mpsc::unbounded_channel();
        let jh = tokio::spawn(async move {
            loop {
                match foo.recv().await {
                    Some(x) => {
                        if let Err(_) = tx.send(x) {
                            break;
                        }
                    }
                    None => {
                        break;
                    }
                }
            }
        });
        return (ret, jh);
    }

    async fn new() -> Result<Harness, Box<dyn std::error::Error>> {
        let _ = env_logger::try_init();
        let ct = CancellationToken::new();
        let server = Server::new(ct.child_token()).await?;
        let (from_client_tx, from_client) = mpsc::channel(2);
        let (to_client, to_client_rx) = mpsc::channel(2);
        let (to_client_shim, to_client_jh) = Harness::shim(to_client_rx);
        let (from_client_shim, from_client_shim_jh) = Harness::shim(from_client);
        let mut clientm = stream_multiplexer::StreamMultiplexerClient::new(
            stream_multiplexer::StreamMultiplexerClientOptions {
                listen_port: 0,
                listen_host: "localhost".to_string(),
            },
            from_client_tx,
            to_client_shim,
        )
        .await?;
        let mut serverm = stream_multiplexer::StreamMultiplexerServer::new(
            stream_multiplexer::StreamMultiplexerServerOptions {
                target_port: server.port,
                target_address: "localhost".to_string(),
            },
            to_client,
            from_client_shim,
        )?;
        let client_port = clientm.port;
        let ct2 = ct.child_token();
        let run_client_jh = tokio::spawn(async move {
            let _a = clientm.run(ct2).await;
            return ();
        });
        let ct3 = ct.child_token();
        let run_server_jh = tokio::spawn(async move {
            serverm.run(ct3).await.swallow_or_print_err("foo");
            return ();
        });
        Ok(Harness {
            _server: server,
            _interrupt: ct,
            client_port: client_port,
            _tasks: vec![
                from_client_shim_jh,
                to_client_jh,
                run_client_jh,
                run_server_jh,
            ],
        })
    }
}

#[tokio::test()]
async fn basic_test() {
    let harness = Harness::new().await.unwrap();
    let mut c = TcpStream::connect(format!("localhost:{}", harness.client_port))
        .await
        .unwrap();
    c.write_all(b"hello").await.unwrap();
    let mut buffer = vec![0; 5];
    c.read_exact(&mut buffer).await.unwrap();
    assert!(b"hello".to_vec() == buffer);
}

async fn test_one(idx: usize, port: u16) -> Result<bool, ()> {
    let mut c = TcpStream::connect(format!("localhost:{}", port))
        .await
        .map_err(|_| ())?;
    log::trace!("{}: writing...", idx);
    c.write_all(b"hello").await.map_err(|_| ())?;
    log::trace!("{}: written...", idx);
    let mut buffer = vec![0; 5];
    c.read_exact(&mut buffer).await.map_err(|_| ())?;
    log::trace!("{}: read...", idx);
    return Ok(b"hello".to_vec() == buffer);
}

#[tokio::test()]
async fn large_data_test() {
    let harness = Harness::new().await.unwrap();
    let mut c = TcpStream::connect(format!("localhost:{}", harness.client_port))
        .await
        .unwrap();

    // 64KB deterministic payload - large enough to span multiple TCP reads/frames
    let payload: Vec<u8> = (0..65536).map(|i| (i % 251) as u8).collect();
    c.write_all(&payload).await.unwrap();

    let mut buffer = vec![0u8; payload.len()];
    c.read_exact(&mut buffer).await.unwrap();
    assert_eq!(buffer, payload);
}

#[tokio::test()]
async fn many_test() {
    let harness = Harness::new().await.unwrap();
    let client_port = harness.client_port;
    let jhs: Vec<JoinHandle<bool>> = (0..20)
        .into_iter()
        .map(|idx| tokio::spawn(async move { test_one(idx, client_port).await.unwrap_or(false) }))
        .collect();
    for j in jhs {
        assert!(j.await.unwrap());
    }
}

/// Server where the first connection floods data (never reads),
/// and subsequent connections echo normally.
struct BackpressureServer {
    _join_handle: JoinHandle<()>,
    pub port: u16,
}

impl BackpressureServer {
    async fn new(ct: CancellationToken) -> Result<BackpressureServer, Box<dyn std::error::Error>> {
        let l = TcpListener::bind("localhost:0").await?;
        let port = l.local_addr()?.port();
        let jh = tokio::spawn(async move {
            let mut first = true;
            loop {
                tokio::select! {
                    _ = ct.cancelled() => break,
                    t = l.accept() => {
                        match t {
                            Ok((mut stream, _)) => {
                                let ct2 = ct.child_token();
                                if first {
                                    first = false;
                                    // First connection: send lots of data, never read.
                                    // Data flows server→client, creating backpressure
                                    // on the CLIENT's per-stream channel when the test
                                    // client doesn't read.
                                    tokio::spawn(async move {
                                        let data = vec![0u8; 8 * 1024 * 1024];
                                        let _ = stream.write_all(&data).await;
                                        ct2.cancelled().await;
                                    });
                                } else {
                                    tokio::spawn(async move {
                                        Server::run_one(stream, ct2).await.swallow_or_print_err("echo");
                                    });
                                }
                            }
                            Err(_) => break,
                        }
                    }
                }
            }
        });
        Ok(BackpressureServer {
            _join_handle: jh,
            port,
        })
    }
}

/// Test that a connection with TCP backpressure does not stall other connections.
///
/// The target server floods 8MB into connection 1 (server→client direction).
/// The test client never reads from connection 1, so the client multiplexer's
/// write_all blocks, filling the per-stream channel. With try_send, the client
/// multiplexer drops connection 1 and stays responsive for connection 2.
///
/// The flood travels server→client via from_stream_rx (cap 16), NOT through
/// from_channel_rx which handles connection 2's CONNECT+DATA. This avoids
/// pipeline congestion that would make the test timing-dependent.
#[tokio::test()]
async fn backpressure_no_stall_test() {
    let _ = env_logger::try_init();
    let ct = CancellationToken::new();

    let server = BackpressureServer::new(ct.child_token()).await.unwrap();

    let (from_client_tx, from_client) = mpsc::channel(2);
    let (to_client, to_client_rx) = mpsc::channel(2);
    let (to_client_shim, _to_client_jh) = Harness::shim(to_client_rx);
    let (from_client_shim, _from_client_shim_jh) = Harness::shim(from_client);
    let mut clientm = stream_multiplexer::StreamMultiplexerClient::new(
        stream_multiplexer::StreamMultiplexerClientOptions {
            listen_port: 0,
            listen_host: "localhost".to_string(),
        },
        from_client_tx,
        to_client_shim,
    )
    .await
    .unwrap();
    let mut serverm = stream_multiplexer::StreamMultiplexerServer::new(
        stream_multiplexer::StreamMultiplexerServerOptions {
            target_port: server.port,
            target_address: "localhost".to_string(),
        },
        to_client,
        from_client_shim,
    )
    .unwrap();
    let client_port = clientm.port;
    let ct2 = ct.child_token();
    tokio::spawn(async move { let _ = clientm.run(ct2).await; });
    let ct3 = ct.child_token();
    tokio::spawn(async move { serverm.run(ct3).await.swallow_or_print_err("mux"); });

    // Connection 1: open but never read. The BackpressureServer sends 8MB
    // into this connection. Data flows: server target → server mux →
    // pipeline → client mux → per-stream channel → write_all to test
    // client TCP. Since we never read, TCP buffers fill, write_all blocks,
    // channel fills, try_send drops the connection.
    let port1 = client_port;
    tokio::spawn(async move {
        if let Ok(c) = TcpStream::connect(format!("localhost:{}", port1)).await {
            tokio::time::sleep(Duration::from_secs(30)).await;
            drop(c);
        }
    });

    // Wait for the flood to propagate and fill the per-stream channel.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Connection 2: echo through the same multiplexer. Connection 2's
    // CONNECT+DATA travel client→server via from_channel_rx, which is NOT
    // congested (the flood goes server→client). Without try_send, this
    // hangs because the client multiplexer is blocked on send().await.
    let port2 = client_port;
    let result = tokio::time::timeout(Duration::from_secs(5), async move {
        let mut c = TcpStream::connect(format!("localhost:{}", port2)).await.unwrap();
        c.write_all(b"hello").await.unwrap();
        let mut buf = vec![0u8; 5];
        c.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");
    })
    .await;

    assert!(
        result.is_ok(),
        "Connection 2 stalled — backpressure on connection 1 blocked the multiplexer"
    );
    ct.cancel();
}

#[tokio::test()]
async fn test_stream_disconnect_and_new_stream() {
    let harness = Harness::new().await.unwrap();

    // Connection 1: send and receive
    let mut c1 = TcpStream::connect(format!("localhost:{}", harness.client_port))
        .await
        .unwrap();
    c1.write_all(b"first").await.unwrap();
    let mut buf = vec![0u8; 5];
    c1.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"first");
    drop(c1); // close connection 1

    // Small delay for cleanup
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Connection 2: should work on a fresh stream
    let mut c2 = TcpStream::connect(format!("localhost:{}", harness.client_port))
        .await
        .unwrap();
    c2.write_all(b"second").await.unwrap();
    let mut buf2 = vec![0u8; 6];
    c2.read_exact(&mut buf2).await.unwrap();
    assert_eq!(&buf2, b"second");
}

#[tokio::test()]
async fn test_many_sequential_connections() {
    let harness = Harness::new().await.unwrap();

    for i in 0u32..50 {
        let msg = format!("msg{:03}", i);
        let mut c = TcpStream::connect(format!("localhost:{}", harness.client_port))
            .await
            .unwrap();
        c.write_all(msg.as_bytes()).await.unwrap();
        let mut buf = vec![0u8; msg.len()];
        c.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf, msg.as_bytes());
    }
}

#[tokio::test()]
async fn test_concurrent_large_and_small() {
    let harness = Harness::new().await.unwrap();
    let port = harness.client_port;

    // Large transfer
    let large_task = tokio::spawn(async move {
        let mut c = TcpStream::connect(format!("localhost:{}", port)).await.unwrap();
        let payload: Vec<u8> = (0..65536).map(|i| (i % 251) as u8).collect();
        c.write_all(&payload).await.unwrap();
        let mut buf = vec![0u8; payload.len()];
        c.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf, payload);
    });

    // Small transfer (concurrent)
    let port2 = harness.client_port;
    let small_task = tokio::spawn(async move {
        let mut c = TcpStream::connect(format!("localhost:{}", port2)).await.unwrap();
        c.write_all(b"hi").await.unwrap();
        let mut buf = vec![0u8; 2];
        c.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hi");
    });

    let result = tokio::time::timeout(Duration::from_secs(5), async {
        large_task.await.unwrap();
        small_task.await.unwrap();
    })
    .await;
    assert!(result.is_ok(), "Concurrent transfers timed out");
}

#[tokio::test()]
async fn test_connection_close_midstream() {
    let harness = Harness::new().await.unwrap();

    // Open a connection and start writing, then close abruptly
    {
        let mut c = TcpStream::connect(format!("localhost:{}", harness.client_port))
            .await
            .unwrap();
        c.write_all(&vec![0u8; 1024]).await.unwrap();
        // Drop without reading — abrupt close
    }

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Subsequent connection should still work
    let mut c2 = TcpStream::connect(format!("localhost:{}", harness.client_port))
        .await
        .unwrap();
    c2.write_all(b"ok").await.unwrap();
    let mut buf = vec![0u8; 2];
    c2.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"ok");
}
