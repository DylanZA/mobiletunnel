/*
This file is part of MobileTunnel.
MobileTunnel is free software: you can redistribute it and/or modify it
under the terms of the GNU General Public License as published by the
Free Software Foundation, either version 3 of the License, or
(at your option) any later version.

MobileTunnel is distributed in the hope that it will be useful,
 but WITHOUT ANY WARRANTY; without even the implied warranty of
 MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.

 See the GNU General Public License for more details.
You should have received a copy of the GNU General Public License
along with MobileTunnel. If not, see <https://www.gnu.org/licenses/>.

Copyright 2024 Dylan Yudaken
*/

use std::collections::HashMap;
use std::io;
use std::str;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct OngoingStreamId(u64);

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct ConnectMsg {
    stream_id: OngoingStreamId,
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct DisconnectMsg {
    stream_id: OngoingStreamId,
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct DataMsg {
    stream_id: OngoingStreamId,
    to_send: Vec<u8>,
}
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct ErrorMsg {
    stream_id: OngoingStreamId,
    message: String,
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub enum ChannelMessage {
    Connect(ConnectMsg),
    Data(DataMsg),
    Disconnect(DisconnectMsg),
    Error(ErrorMsg),
}

const CONNECT_MSG_ID: u8 = 1;
const DATA_MSG_ID: u8 = 2;
const ERROR_MSG_ID: u8 = 3;
const DISCONNECT_MSG_ID: u8 = 4;

impl ChannelMessage {
    fn to_string(&self) -> String {
        match &self {
            ChannelMessage::Connect(f) => format!("Connect({:?})", f.stream_id),
            ChannelMessage::Data(d) => format!("Data({:?}) n={}", d.stream_id, d.to_send.len()),
            ChannelMessage::Disconnect(d) => format!("Disconnect({:?})", d.stream_id),
            ChannelMessage::Error(d) => format!("Error({:?}) {}", d.stream_id, d.message),
            _ => format!("Unknown"),
        }
    }

    fn parse(data: &[u8]) -> Result<Option<(ChannelMessage, usize)>, MyError> {
        if data.len() < 13 {
            return Ok(None);
        }
        let header: &[u8; 13] = data[0..13].try_into().unwrap();
        let len_part: [u8; 4] = header[1..5].try_into().unwrap();
        let connection_id_part: [u8; 8] = header[5..13].try_into().unwrap();
        let type_part: u8 = header[0];
        let len: usize = u32::from_le_bytes(len_part).try_into().unwrap();
        if len < 8 {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "Everything should have a connect ion id so far",
            ))?;
        }
        let connection_id = OngoingStreamId(u64::from_le_bytes(connection_id_part));
        let frame_size = len;
        if frame_size < data.len() {
            log::debug!("frame_size {} data len {}", frame_size, data.len());
            return Ok(None);
        }
        let data_part: &[u8] = data[13..len]
            .try_into()
            .map_err(|_| "Weird data length issue")?;
        let msg: ChannelMessage = match type_part {
            CONNECT_MSG_ID => {
                if len != 13 {
                    return Err("Expected 8 len")?;
                }
                let sc = ConnectMsg {
                    stream_id: connection_id,
                };
                Ok(ChannelMessage::Connect(sc))
            }
            DISCONNECT_MSG_ID => {
                if len != 13 {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        "Expected 8 len for disconnect",
                    ))?;
                }

                let sc: DisconnectMsg = DisconnectMsg {
                    stream_id: connection_id,
                };
                Ok(ChannelMessage::Disconnect(sc))
            }
            DATA_MSG_ID => {
                let sc = DataMsg {
                    stream_id: connection_id,
                    to_send: data_part.to_vec(),
                };
                Ok(ChannelMessage::Data(sc))
            }
            ERROR_MSG_ID => {
                let msg = str::from_utf8(&data_part).map_err(|_x| {
                    io::Error::new(io::ErrorKind::Other, "unable to decode string")
                })?;
                let sc = ErrorMsg {
                    stream_id: connection_id,
                    message: String::from(msg),
                };
                Ok(ChannelMessage::Error(sc))
            }
            _ => Err(io::Error::new(
                io::ErrorKind::Other,
                format!("unknown msg id {}!", data[0]),
            )),
        }?;
        return Ok(Some((msg, frame_size)));
    }

    fn encode(&self) -> Vec<u8> {
        let mut ret: Vec<u8> = vec![];
        match &self {
            &ChannelMessage::Connect(m) => {
                ret.push(CONNECT_MSG_ID);
                ret.extend_from_slice(&13u32.to_le_bytes());
                ret.extend_from_slice(&m.stream_id.0.to_le_bytes());
            }
            &ChannelMessage::Disconnect(sd) => {
                ret.push(DISCONNECT_MSG_ID);
                ret.extend_from_slice(&13u32.to_le_bytes());
                ret.extend_from_slice(&sd.stream_id.0.to_le_bytes());
            }
            &ChannelMessage::Data(m) => match TryInto::<u32>::try_into(13 + m.to_send.len()) {
                Ok(usz) => {
                    ret.push(DATA_MSG_ID);
                    ret.extend_from_slice(&u32::from(usz).to_le_bytes());
                    ret.extend_from_slice(&m.stream_id.0.to_le_bytes());
                    ret.extend_from_slice(&m.to_send);
                }
                Err(_) => {
                    ret.push(ERROR_MSG_ID);
                    let emsg = "Bad data size".as_bytes();
                    let tot_size: u32 = (13 + emsg.len()).try_into().unwrap();
                    ret.extend_from_slice(&u32::from(tot_size).to_le_bytes());
                    ret.extend_from_slice(&m.stream_id.0.to_le_bytes());
                    ret.extend_from_slice(emsg);
                }
            },
            &ChannelMessage::Error(em) => {
                let edata = &em.message.as_bytes();
                match TryInto::<u32>::try_into(13 + edata.len()) {
                    Ok(usz) => {
                        ret.push(ERROR_MSG_ID);
                        ret.extend_from_slice(&u32::from(usz).to_le_bytes());
                        ret.extend_from_slice(&em.stream_id.0.to_le_bytes());
                        ret.extend_from_slice(&edata);
                    }
                    Err(_) => {
                        ret.push(ERROR_MSG_ID);
                        let emsg = "Bad error data size".as_bytes();
                        let tot_size: u32 = (13 + emsg.len()).try_into().unwrap();
                        ret.extend_from_slice(&u32::from(tot_size).to_le_bytes());
                        ret.extend_from_slice(&em.stream_id.0.to_le_bytes());
                        ret.extend_from_slice(emsg);
                    }
                }
            }
        }
        ret
    }
}

pub struct StreamMultiplexerClientOptions {
    pub listen_port: u16,
    pub listen_host: String,
}

pub struct StreamMultiplexerServerOptions {
    pub target_port: u16,
    pub target_address: String,
}

async fn run_internal_stream(
    mut stream: TcpStream,
    stream_id: OngoingStreamId,
    channel_tx: mpsc::Sender<ChannelMessage>,
    mut channel_rx: mpsc::Receiver<Option<ChannelMessage>>,
) -> Result<(), io::Error> {
    let mut rx_bytes: [u8; 4096] = [0; 4096];
    loop {
        log::debug!("Running internal connection {}", stream_id.0);
        tokio::select! {
            stream_read = stream.read(&mut rx_bytes) => {
                match stream_read {
                    Err(e) => {
                        log::debug!("Error reading from tcp stream {}", e);
                        if let io::ErrorKind::Interrupted = e.kind() {
                            continue;
                        }
                        return Err(e);
                    },
                    Ok(n) => {
                        let vec = rx_bytes[..n].to_vec();
                        if n == 0 {
                            log::debug!("Read eof from tcp stream");
                            break;
                        }
                        log::debug!("Read {} bytes from TcpStream, sending to app", vec.len());
                        channel_tx.send(ChannelMessage::Data(DataMsg {stream_id: stream_id.clone(), to_send: vec})).await.map_err(|_| io::Error::new(io::ErrorKind::Other, "sending app data"))?;
                    }
                }
            },
            channel_rx_msg = channel_rx.recv() => {
                log::debug!("internal connection got some message!");
                match channel_rx_msg.flatten() {
                    Some(ChannelMessage::Connect(_)) => {
                        log::error!("Weird connect");
                        break;
                    },
                    Some(ChannelMessage::Disconnect(_)) => {
                        log::debug!("disconnect!");
                        break;
                    },
                    Some(ChannelMessage::Error(_)) => {
                        log::debug!("error!");
                        break;
                    }
                    Some(ChannelMessage::Data(sd)) => {
                        log::debug!("Got {} bytes from app, sending to TcpStream", sd.to_send.len());
                        stream.write(&sd.to_send).await?;
                    }
                    None => {
                        log::debug!("Channel closed");
                        break;
                    },
                    _ => {
                        log::error!("Unexpected result");
                        break;
                    },
                }
            }
        }
    }
    log::debug!("Done internal connection {}", stream_id.0);
    match stream.shutdown().await {
        Ok(_) => log::debug!("Shutdown connection ok {}", stream_id.0),
        Err(e) => log::debug!("Shutdown connection {} had error {}", stream_id.0, e),
    }
    Ok(())
}

struct OngoingStreamState {
    // send into the stream
    into_stream_tx: mpsc::Sender<Option<ChannelMessage>>,
    join_handle: tokio::task::JoinHandle<()>,
}
type MyError = Box<dyn std::error::Error>;
struct ChannelMessageParser {
    buffer: Vec<u8>,
}

impl ChannelMessageParser {
    fn new() -> ChannelMessageParser {
        ChannelMessageParser { buffer: Vec::new() }
    }

    fn add(&mut self, mut data: Vec<u8>) {
        if self.buffer.is_empty() {
            self.buffer = data;
        } else {
            self.buffer.append(&mut data);
        }
    }

    fn next(&mut self) -> Result<Option<ChannelMessage>, MyError> {
        match ChannelMessage::parse(&self.buffer)? {
            None => return Ok(None),
            Some((msg, to_drop)) => {
                self.buffer.drain(0..to_drop);
                return Ok(Some(msg));
            }
        }
    }
}

struct OngoingStreamTracker {
    streams: HashMap<OngoingStreamId, OngoingStreamState>,
    to_channel_tx: mpsc::Sender<ChannelMessage>,
    pub from_stream_rx: mpsc::Receiver<ChannelMessage>,
}

enum StreamFactory {
    Stream(TcpStream),
    Addr((String, u16)),
}

fn swallow_error<T: Into<MyError>>(e: T) -> Result<(), MyError> {
    log::error!("Swallowed error {}", e.into());
    return Ok(());
}

impl OngoingStreamTracker {
    fn new() -> OngoingStreamTracker {
        let (tx, rx) = mpsc::channel(16);
        OngoingStreamTracker {
            streams: HashMap::new(),
            to_channel_tx: tx,
            from_stream_rx: rx,
        }
    }

    async fn remove(&mut self, id: &OngoingStreamId) {
        match self.streams.remove(&id) {
            None => (),
            Some(x) => {
                x.into_stream_tx
                    .send(None)
                    .await
                    .or_else(swallow_error)
                    .unwrap(); // tell the stream to close
                log::debug!("Start waiting for join handle");
                x.join_handle.await.or_else(swallow_error).unwrap();
                log::debug!("... done waiting for join handle");
            }
        }
    }

    async fn data(&mut self, sd: DataMsg) -> Result<(), MyError> {
        let id = sd.stream_id.clone();
        if let Some(x) = self.streams.get_mut(&sd.stream_id) {
            if let Err(e) = x.into_stream_tx.send(Some(ChannelMessage::Data(sd))).await {
                log::debug!("Error sending data to connection {}", e);
                // must be dead
                self.remove(&id).await;
            }
        } else {
            log::debug!("Got message for {:?} but it is not running", &sd.stream_id);
        }
        Ok(())
    }

    async fn new_stream(
        &mut self,
        id: OngoingStreamId,
        conn_factory: StreamFactory,
    ) -> Result<(), MyError> {
        if let Some(_) = self.streams.get_mut(&id) {
            log::error!("Already have a connection!?!?");
            return Err("Already had this connection id")?;
        }

        let to_channel_tx = self.to_channel_tx.clone();
        let (into_stream_tx, from_channel_rx) = mpsc::channel(16);
        let idc = id.clone();
        let jh = tokio::spawn(async move {
            let to_channel_tx_2 = to_channel_tx.clone();
            let conn_res = match conn_factory {
                StreamFactory::Stream(s) => Ok(s),
                StreamFactory::Addr(a) => TcpStream::connect(a).await,
            };
            match conn_res {
                Err(x) => {
                    log::error!("Error connecting {}", x);
                }
                Ok(conn) => {
                    if let Err(x) =
                        run_internal_stream(conn, idc.clone(), to_channel_tx, from_channel_rx).await
                    {
                        log::error!("Error running stream {}", x);
                    }
                }
            };
            to_channel_tx_2
                .send(ChannelMessage::Disconnect(DisconnectMsg { stream_id: idc }))
                .await
                .or_else(swallow_error)
                .unwrap();
        });
        let state = OngoingStreamState {
            into_stream_tx,
            join_handle: jh,
        };
        self.streams.insert(id, state);
        return Ok(());
    }
}

pub struct StreamMultiplexerServer {
    options: StreamMultiplexerServerOptions,
    parser: ChannelMessageParser,
    into_channel_tx: mpsc::Sender<Vec<u8>>,
    from_channel_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    streams: OngoingStreamTracker,
}

impl StreamMultiplexerServer {
    pub fn new(
        options: StreamMultiplexerServerOptions,
        into_channel_tx: mpsc::Sender<Vec<u8>>,
        from_channel_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    ) -> Result<StreamMultiplexerServer, io::Error> {
        return Ok(StreamMultiplexerServer {
            options: options,
            into_channel_tx,
            from_channel_rx,
            parser: ChannelMessageParser::new(),
            streams: OngoingStreamTracker::new(),
        });
    }

    async fn process_channel_rx(&mut self, data: Vec<u8>) -> Result<(), MyError> {
        self.parser.add(data);
        loop {
            match self.parser.next()? {
                None => break,
                Some(ChannelMessage::Error(se)) => {
                    log::error!("Stream error {}", &se.message);
                    self.streams.remove(&se.stream_id).await;
                }
                Some(ChannelMessage::Connect(sc)) => {
                    let cid = sc.stream_id.clone();
                    let addr = (
                        self.options.target_address.clone(),
                        self.options.target_port,
                    );
                    log::debug!("Server new connection {}", cid.0);
                    if let Err(e) = self
                        .streams
                        .new_stream(cid, StreamFactory::Addr(addr))
                        .await
                    {
                        log::error!("Unalbe to connect {}", e);
                    }
                }
                Some(ChannelMessage::Data(sd)) => {
                    log::debug!("Server got data {}", sd.to_send.len());
                    self.streams.data(sd).await?;
                }
                Some(ChannelMessage::Disconnect(sd)) => {
                    log::debug!("Server got disconnect {}", sd.stream_id.0);
                    self.streams.remove(&sd.stream_id).await;
                }
            }
        }
        Ok(())
    }

    pub async fn run(&mut self) -> Result<(), MyError> {
        loop {
            tokio::select! {
                from_channel_rx = self.from_channel_rx.recv() => {
                    match from_channel_rx {
                        None => {
                            return Err("rx channel died")?;
                        },
                        Some(data) => {
                            log::debug!("Server got {} bytes from channel", data.len());
                            self.process_channel_rx(data).await?;
                        }
                    }
                },
                connect_tx = self.streams.from_stream_rx.recv() => {
                    match connect_tx {
                        None => {
                            return Err("connection tracker channel died")?;
                        },
                        Some(data) => {
                            log::debug!("Server got app message {}. Encode and send it", data.to_string());
                            self.into_channel_tx.send(data.encode()).await?;
                        }
                    }
                }
            }
        }
    }
}

pub struct StreamMultiplexerClient {
    options: StreamMultiplexerClientOptions,
    listener: TcpListener,
    to_channel_tx: mpsc::Sender<Vec<u8>>,
    from_channel_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    parser: ChannelMessageParser,
    streams: OngoingStreamTracker,
}

impl StreamMultiplexerClient {
    pub async fn new(
        options: StreamMultiplexerClientOptions,
        tx: mpsc::Sender<Vec<u8>>,
        rx: mpsc::UnboundedReceiver<Vec<u8>>,
    ) -> Result<StreamMultiplexerClient, io::Error> {
        let listen = TcpListener::bind((options.listen_host.clone(), options.listen_port)).await?;
        return Ok(StreamMultiplexerClient {
            options: options,
            listener: listen,
            to_channel_tx: tx,
            from_channel_rx: rx,
            parser: ChannelMessageParser::new(),
            streams: OngoingStreamTracker::new(),
        });
    }

    async fn process_channel_rx(&mut self, data: Vec<u8>) -> Result<(), MyError> {
        self.parser.add(data);
        loop {
            match self.parser.next()? {
                None => break,
                Some(ChannelMessage::Error(se)) => {
                    log::error!("Stream error {}", &se.message);
                    self.streams.remove(&se.stream_id).await;
                }
                Some(ChannelMessage::Connect(_)) => {
                    return Err("What's the client doing getting a connection request?!?!")?;
                }
                Some(ChannelMessage::Data(sd)) => {
                    log::debug!("Client got data {}", sd.to_send.len());
                    self.streams.data(sd).await?;
                }
                Some(ChannelMessage::Disconnect(sd)) => {
                    log::debug!("Client got disconnect {}", sd.stream_id.0);
                    self.streams.remove(&sd.stream_id).await;
                }
            }
        }
        Ok(())
    }

    pub async fn run(&mut self) -> Result<(), MyError> {
        let mut stream_id: u64 = 1;
        loop {
            tokio::select! {
                new_stream_res = self.listener.accept() => {
                    let (new_stream, new_stream_addr) = new_stream_res?;
                    log::info!("Got a new stream from {}", new_stream_addr);
                    let this_id = OngoingStreamId(stream_id);
                    stream_id += 1;
                    self.to_channel_tx.send(ChannelMessage::Connect(ConnectMsg {
                        stream_id: this_id.clone()
                    }).encode()).await?;
                    self.streams.new_stream(this_id, StreamFactory::Stream(new_stream)).await?;
                },
                from_channel_rx = self.from_channel_rx.recv() => {
                    match from_channel_rx {
                        None => {
                            return Err("rx channel died")?;
                        },
                        Some(data) => {
                            log::debug!("Client got {} bytes from channel", data.len());
                            self.process_channel_rx(data).await?;
                        }
                    }
                },
                connect_tx = self.streams.from_stream_rx.recv() => {
                    match connect_tx {
                        None => {
                            return Err("connection tracker channel died")?;
                        },
                        Some(data) => {
                            log::debug!("Client got stream message {}", data.to_string());
                            // send it down the channel
                            self.to_channel_tx.send(data.encode()).await?;
                        }
                    }
                }
            }
        }
    }
}
