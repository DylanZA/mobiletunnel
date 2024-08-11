use std::io;
use std::net::Ipv4Addr;

use bytes::BytesMut;
use futures::stream;
use libmobiletunnel::stream_multiplexer;
use libmobiletunnel::util::SwallowResultPrintErrExt as _;
use tokio;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

struct server {
    join_handle: JoinHandle<()>,
    pub port: u16,
}

impl server {
    async fn write_all(s: &mut TcpStream, buf: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
        if let Err(e) = s.write_all(buf).await {
            return Err(e.into());
        }
        return Ok(());
    }

    async fn run_one(
        mut l: TcpStream,
        ct: CancellationToken,
    ) -> Result<(), Box<dyn std::error::Error>> {
        loop {
            let mut b = BytesMut::with_capacity(8128);
            tokio::select! {
                r = l.read_buf(&mut b) => {
                    l.write(&b[..]).await?;
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
                        server::run_one(stream, ct2).await.swallow_or_print_err("server::run_one");
                        return ();
                    });
                    handles.push(jh);
                }
            }
        }
        return Ok(());
    }

    async fn new(ct: CancellationToken) -> Result<server, Box<dyn std::error::Error>> {
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
        return Ok(server {
            join_handle: jh,
            port: port,
        });
    }
}

struct harness {
    server: server,
    interrupt: CancellationToken,
    client_port: u16,
    tasks: Vec<JoinHandle<()>>,
}

impl harness {
    fn shim(
        mut foo: mpsc::Receiver<Vec<u8>>,
    ) -> (mpsc::UnboundedReceiver<Vec<u8>>, JoinHandle<()>) {
        let (tx, ret) = mpsc::unbounded_channel();
        let jh = tokio::spawn(async move {
            loop {
                match foo.recv().await {
                    Some(x) => {
                        if let Err(e) = tx.send(x) {
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

    async fn new() -> Result<harness, Box<dyn std::error::Error>> {
        let _ = env_logger::try_init();
        let ct = CancellationToken::new();
        let server = server::new(ct.child_token()).await?;
        let (from_client_tx, from_client) = mpsc::channel(2);
        let (to_client, to_client_rx) = mpsc::channel(2);
        let (to_client_shim, to_client_jh) = harness::shim(to_client_rx);
        let (from_client_shim, from_client_shim_jh) = harness::shim(from_client);
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
        Ok(harness {
            server: server,
            interrupt: ct,
            client_port: client_port,
            tasks: vec![
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
    let mut harness = harness::new().await.unwrap();
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
    return Ok((b"hello".to_vec() == buffer));
}

#[tokio::test()]
async fn many_test() {
    let mut harness = harness::new().await.unwrap();
    let client_port = harness.client_port;
    let jhs: Vec<JoinHandle<bool>> = (0..20)
        .into_iter()
        .map(|idx| tokio::spawn(async move { test_one(idx, client_port).await.unwrap_or(false) }))
        .collect();
    for j in jhs {
        assert!(j.await.unwrap());
    }
}
