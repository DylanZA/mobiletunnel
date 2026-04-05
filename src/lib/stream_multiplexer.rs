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
use std::error;
use std::fmt;
use std::io;
use std::str;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinError;
use tokio_util::sync::CancellationToken;

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct OngoingStreamId(pub u64);

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct ConnectMsg {
    pub stream_id: OngoingStreamId,
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct DisconnectMsg {
    pub stream_id: OngoingStreamId,
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct DataMsg {
    pub stream_id: OngoingStreamId,
    pub to_send: Vec<u8>,
}
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct ErrorMsg {
    stream_id: OngoingStreamId,
    message: String,
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct PauseMsg {
    stream_id: OngoingStreamId,
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct ResumeMsg {
    stream_id: OngoingStreamId,
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub enum ChannelMessage {
    Connect(ConnectMsg),
    Data(DataMsg),
    Disconnect(DisconnectMsg),
    Error(ErrorMsg),
    Pause(PauseMsg),
    Resume(ResumeMsg),
}

const CONNECT_MSG_ID: u8 = 1;
const DATA_MSG_ID: u8 = 2;
const ERROR_MSG_ID: u8 = 3;
const DISCONNECT_MSG_ID: u8 = 4;
const PAUSE_MSG_ID: u8 = 5;
const RESUME_MSG_ID: u8 = 6;

/// Per-stream pending message count at which we send PAUSE to the remote,
/// telling it to stop reading from its TCP socket for this stream.
const BACKPRESSURE_HIGH_WATER: usize = 64;
/// Once a paused stream drains below this count, we send RESUME.
const BACKPRESSURE_LOW_WATER: usize = 16;

impl ChannelMessage {
    fn to_string(&self) -> String {
        match &self {
            ChannelMessage::Connect(f) => format!("Connect({:?})", f.stream_id),
            ChannelMessage::Data(d) => format!("Data({:?}) n={}", d.stream_id, d.to_send.len()),
            ChannelMessage::Disconnect(d) => format!("Disconnect({:?})", d.stream_id),
            ChannelMessage::Error(d) => format!("Error({:?}) {}", d.stream_id, d.message),
            ChannelMessage::Pause(d) => format!("Pause({:?})", d.stream_id),
            ChannelMessage::Resume(d) => format!("Resume({:?})", d.stream_id),
        }
    }

    fn parse(data: &[u8]) -> Result<Option<(ChannelMessage, usize)>, MultiplexerError> {
        if data.len() < 13 {
            return Ok(None);
        }
        let header: &[u8; 13] = data[0..13].try_into().unwrap();
        let len_part: [u8; 4] = header[1..5].try_into().unwrap();
        let connection_id_part: [u8; 8] = header[5..13].try_into().unwrap();
        let type_part: u8 = header[0];
        let len: usize = u32::from_le_bytes(len_part).try_into().unwrap();
        if len < 13 {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "Frame too small: minimum is 13 bytes",
            ))?;
        }
        let connection_id = OngoingStreamId(u64::from_le_bytes(connection_id_part));
        let frame_size = len;
        if frame_size > data.len() {
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
            PAUSE_MSG_ID => {
                if len != 13 {
                    return Err("Expected 13 len for pause")?;
                }
                Ok(ChannelMessage::Pause(PauseMsg {
                    stream_id: connection_id,
                }))
            }
            RESUME_MSG_ID => {
                if len != 13 {
                    return Err("Expected 13 len for resume")?;
                }
                Ok(ChannelMessage::Resume(ResumeMsg {
                    stream_id: connection_id,
                }))
            }
            _ => Err(io::Error::new(
                io::ErrorKind::Other,
                format!("unknown msg id {}!", data[0]),
            )),
        }?;
        return Ok(Some((msg, frame_size)));
    }

    pub fn encode(&self) -> Vec<u8> {
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
            &ChannelMessage::Pause(ref m) => {
                ret.push(PAUSE_MSG_ID);
                ret.extend_from_slice(&13u32.to_le_bytes());
                ret.extend_from_slice(&m.stream_id.0.to_le_bytes());
            }
            &ChannelMessage::Resume(ref m) => {
                ret.push(RESUME_MSG_ID);
                ret.extend_from_slice(&13u32.to_le_bytes());
                ret.extend_from_slice(&m.stream_id.0.to_le_bytes());
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
    stream: TcpStream,
    stream_id: OngoingStreamId,
    channel_tx: mpsc::Sender<ChannelMessage>,
    mut channel_rx: mpsc::UnboundedReceiver<Option<ChannelMessage>>,
    flow: Arc<StreamFlowControl>,
    interrupt: CancellationToken,
) -> Result<(), io::Error> {
    let (mut read_half, mut write_half) = stream.into_split();
    let done = CancellationToken::new();

    // Write task: drains channel_rx and writes to TCP independently.
    // After each message, decrements the pending counter. If below
    // low-water mark and we previously sent PAUSE, sends RESUME.
    let write_done = done.clone();
    let write_done2 = done.clone();
    let write_interrupt = interrupt.clone();
    let write_flow = flow.clone();
    let write_sid = stream_id.clone();
    let write_channel_tx = channel_tx.clone();
    let write_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = write_done.cancelled() => {
                    log::trace!("write task cancelled");
                    break;
                },
                _ = write_interrupt.cancelled() => {
                    log::trace!("write task interrupted");
                    break;
                },
                msg = channel_rx.recv() => {
                    match msg.flatten() {
                        Some(ChannelMessage::Data(sd)) => {
                            log::debug!("Got {} bytes from app, sending to TcpStream", sd.to_send.len());
                            tokio::select! {
                                _ = write_done2.cancelled() => {
                                    log::debug!("write cancelled during write_all");
                                    break;
                                },
                                result = write_half.write_all(&sd.to_send) => {
                                    if let Err(e) = result {
                                        log::debug!("Error writing to tcp stream: {}", e);
                                        break;
                                    }
                                }
                            }
                            // Decrement pending count; send RESUME if we cross low-water
                            let prev = write_flow.pending.fetch_sub(1, Ordering::Relaxed);
                            if prev <= BACKPRESSURE_LOW_WATER
                                && write_flow.sent_pause.swap(false, Ordering::Relaxed)
                            {
                                log::debug!("Stream {:?} below low-water, sending RESUME", write_sid.0);
                                if let Err(e) = write_channel_tx
                                    .send(ChannelMessage::Resume(ResumeMsg {
                                        stream_id: write_sid.clone(),
                                    }))
                                    .await
                                {
                                    log::warn!("Failed to send RESUME for stream {:?}: {}", write_sid.0, e);
                                    break;
                                }
                            }
                        }
                        Some(ChannelMessage::Disconnect(_)) => {
                            log::debug!("disconnect!");
                            break;
                        }
                        Some(ChannelMessage::Error(_)) => {
                            log::debug!("error!");
                            break;
                        }
                        Some(ChannelMessage::Connect(_))
                        | Some(ChannelMessage::Pause(_))
                        | Some(ChannelMessage::Resume(_)) => {
                            log::error!("Unexpected message in write task");
                            break;
                        }
                        None => {
                            log::debug!("Channel closed");
                            break;
                        }
                    }
                }
            }
        }
        let _ = write_half.shutdown().await;
    });

    // Read task: reads from TCP and sends to multiplexer.
    // Respects the read_paused flag set when the remote sends PAUSE.
    let read_done = done.clone();
    let read_interrupt = interrupt.clone();
    let read_flow = flow.clone();
    let sid = stream_id.clone();
    let read_task = tokio::spawn(async move {
        let mut rx_bytes = [0u8; 4096];
        loop {
            // If paused by the remote, wait for RESUME notification.
            if read_flow.read_paused.load(Ordering::Relaxed) {
                log::debug!("Read task paused for stream {:?}", sid.0);
                tokio::select! {
                    _ = read_done.cancelled() => break,
                    _ = read_interrupt.cancelled() => break,
                    _ = read_flow.read_resume_notify.notified() => {
                        log::debug!("Read task resumed for stream {:?}", sid.0);
                        continue;
                    }
                }
            }

            tokio::select! {
                _ = read_done.cancelled() => {
                    log::trace!("read task cancelled");
                    break;
                },
                _ = read_interrupt.cancelled() => {
                    log::trace!("read task interrupted");
                    break;
                },
                stream_read = read_half.read(&mut rx_bytes) => {
                    match stream_read {
                        Err(e) => {
                            log::debug!("Error reading from tcp stream {}", e);
                            if e.kind() == io::ErrorKind::Interrupted {
                                continue;
                            }
                            break;
                        }
                        Ok(0) => {
                            log::debug!("Read eof from tcp stream");
                            break;
                        }
                        Ok(n) => {
                            let vec = rx_bytes[..n].to_vec();
                            log::debug!("Read {} bytes from TcpStream, sending to app", vec.len());
                            if channel_tx.send(ChannelMessage::Data(DataMsg {
                                stream_id: sid.clone(),
                                to_send: vec,
                            })).await.is_err() {
                                log::debug!("channel_tx send failed, ending read task");
                                break;
                            }
                        }
                    }
                }
            }
        }
    });

    let _drop_guard = done.drop_guard();

    tokio::select! {
        _ = write_task => {
            log::debug!("Write task finished for {}", stream_id.0);
        }
        _ = read_task => {
            log::debug!("Read task finished for {}", stream_id.0);
        }
    }
    log::debug!("Done internal connection {}", stream_id.0);
    Ok(())
}

/// Shared state for per-stream backpressure signaling.
struct StreamFlowControl {
    /// Number of messages pending in the unbounded channel.
    pending: AtomicUsize,
    /// Whether we've sent PAUSE to the remote for this stream's inbound data.
    sent_pause: AtomicBool,
    /// Whether the remote sent PAUSE for this stream's outbound data.
    read_paused: AtomicBool,
    /// Notifies the read task when RESUME is received.
    read_resume_notify: Notify,
}

struct OngoingStreamState {
    into_stream_tx: mpsc::UnboundedSender<Option<ChannelMessage>>,
    join_handle: tokio::task::JoinHandle<()>,
    flow: Arc<StreamFlowControl>,
}

#[derive(Debug, Clone)]
pub struct MultiplexerError {
    msg: String,
}

impl From<&str> for MultiplexerError {
    fn from(cause: &str) -> Self {
        MultiplexerError {
            msg: cause.to_string(),
        }
    }
}

impl From<std::io::Error> for MultiplexerError {
    fn from(cause: std::io::Error) -> Self {
        MultiplexerError {
            msg: cause.to_string(),
        }
    }
}

impl<T> From<mpsc::error::SendError<T>> for MultiplexerError {
    fn from(cause: mpsc::error::SendError<T>) -> Self {
        MultiplexerError {
            msg: format!("senderror: {}", cause),
        }
    }
}

impl From<JoinError> for MultiplexerError {
    fn from(cause: JoinError) -> Self {
        MultiplexerError {
            msg: format!("join error: {}", cause),
        }
    }
}

impl fmt::Display for MultiplexerError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Error: {}", self.msg)
    }
}

impl error::Error for MultiplexerError {
    fn description(&self) -> &str {
        &self.msg
    }
}

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

    fn next(&mut self) -> Result<Option<ChannelMessage>, MultiplexerError> {
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

fn swallow_error<T: Into<MultiplexerError>>(e: T) -> Result<(), MultiplexerError> {
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
        if let Some(x) = self.streams.remove(&id) {
            // Don't use send(None).await — the channel may be full (which is
            // why we're removing this stream), so it would block forever.
            // abort() is sufficient: it cancels the task and drops resources.
            x.join_handle.abort();
            log::debug!("Aborted stream task for {:?}", id);
        }
    }

    async fn data(
        &mut self,
        sd: DataMsg,
        into_channel_tx: &mpsc::Sender<Vec<u8>>,
    ) -> Result<(), MultiplexerError> {
        let id = sd.stream_id.clone();
        if let Some(x) = self.streams.get_mut(&sd.stream_id) {
            // Unbounded send — never blocks the multiplexer.
            if let Err(e) = x.into_stream_tx.send(Some(ChannelMessage::Data(sd))) {
                log::debug!("Stream {:?} channel closed: {}", id, e);
                self.remove(&id).await;
                return Ok(());
            }
            // Track pending count; send PAUSE if we cross high-water.
            let pending = x.flow.pending.fetch_add(1, Ordering::Relaxed) + 1;
            if pending >= BACKPRESSURE_HIGH_WATER
                && !x.flow.sent_pause.swap(true, Ordering::Relaxed)
            {
                log::debug!("Stream {:?} above high-water ({}), sending PAUSE", id.0, pending);
                match into_channel_tx.try_send(
                    ChannelMessage::Pause(PauseMsg {
                        stream_id: id,
                    })
                    .encode(),
                ) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        // Channel full — natural backpressure is already active
                        // (the multiplexer run loop will block on the next data
                        // send). Reset sent_pause so we retry on the next message.
                        x.flow.sent_pause.store(false, Ordering::Relaxed);
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        return Err("channel closed")?;
                    }
                }
            }
        } else {
            log::debug!("Got message for {:?} but it is not running", &sd.stream_id);
        }
        Ok(())
    }

    fn pause_read(&self, id: &OngoingStreamId) {
        if let Some(x) = self.streams.get(id) {
            x.flow.read_paused.store(true, Ordering::Relaxed);
        }
    }

    fn resume_read(&self, id: &OngoingStreamId) {
        if let Some(x) = self.streams.get(id) {
            x.flow.read_paused.store(false, Ordering::Relaxed);
            x.flow.read_resume_notify.notify_one();
        }
    }

    async fn new_stream(
        &mut self,
        id: OngoingStreamId,
        conn_factory: StreamFactory,
        interrupt: CancellationToken,
    ) -> Result<(), MultiplexerError> {
        if let Some(_) = self.streams.get_mut(&id) {
            log::error!("Already have a connection!?!?");
            return Err("Already had this connection id")?;
        }

        let to_channel_tx = self.to_channel_tx.clone();
        let (into_stream_tx, from_channel_rx) = mpsc::unbounded_channel();
        let flow = Arc::new(StreamFlowControl {
            pending: AtomicUsize::new(0),
            sent_pause: AtomicBool::new(false),
            read_paused: AtomicBool::new(false),
            read_resume_notify: Notify::new(),
        });
        let idc = id.clone();
        let task_flow = flow.clone();
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
                    if let Err(x) = run_internal_stream(
                        conn,
                        idc.clone(),
                        to_channel_tx,
                        from_channel_rx,
                        task_flow,
                        interrupt,
                    )
                    .await
                    {
                        log::error!("Error running stream {}", x);
                    }
                }
            };
            let _ = to_channel_tx_2
                .send(ChannelMessage::Disconnect(DisconnectMsg { stream_id: idc }))
                .await
                .or_else(swallow_error);
        });
        let state = OngoingStreamState {
            into_stream_tx,
            join_handle: jh,
            flow,
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
    interrupt: CancellationToken,
    streams: OngoingStreamTracker,
}

impl StreamMultiplexerServer {
    pub fn new(
        options: StreamMultiplexerServerOptions,
        into_channel_tx: mpsc::Sender<Vec<u8>>,
        from_channel_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    ) -> Result<StreamMultiplexerServer, MultiplexerError> {
        return Ok(StreamMultiplexerServer {
            options: options,
            into_channel_tx,
            from_channel_rx,
            interrupt: CancellationToken::new(),
            parser: ChannelMessageParser::new(),
            streams: OngoingStreamTracker::new(),
        });
    }

    async fn process_channel_rx(&mut self, data: Vec<u8>) -> Result<(), MultiplexerError> {
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
                    let interrupt = self.interrupt.child_token();
                    if let Err(e) = self
                        .streams
                        .new_stream(cid, StreamFactory::Addr(addr), interrupt)
                        .await
                    {
                        log::error!("Unalbe to connect {}", e);
                    }
                }
                Some(ChannelMessage::Data(sd)) => {
                    log::debug!("Server got data {}", sd.to_send.len());
                    self.streams.data(sd, &self.into_channel_tx).await?;
                }
                Some(ChannelMessage::Disconnect(sd)) => {
                    log::debug!("Server got disconnect {}", sd.stream_id.0);
                    self.streams.remove(&sd.stream_id).await;
                }
                Some(ChannelMessage::Pause(p)) => {
                    log::debug!("Server got pause for stream {}", p.stream_id.0);
                    self.streams.pause_read(&p.stream_id);
                }
                Some(ChannelMessage::Resume(r)) => {
                    log::debug!("Server got resume for stream {}", r.stream_id.0);
                    self.streams.resume_read(&r.stream_id);
                }
            }
        }
        Ok(())
    }

    pub async fn run(&mut self, interrupt: CancellationToken) -> Result<(), MultiplexerError> {
        loop {
            tokio::select! {
                _ = interrupt.cancelled() => {
                    log::trace!("multiplexer client stopped due to cancel");
                    self.interrupt.cancel();
                    return Ok(());
                },
                from_channel_rx = self.from_channel_rx.recv() => {
                    match from_channel_rx {
                        None => {
                            return Err("rx channel died, ending server")?;
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
    _options: StreamMultiplexerClientOptions,
    listener: TcpListener,
    to_channel_tx: mpsc::Sender<Vec<u8>>,
    from_channel_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    parser: ChannelMessageParser,
    streams: OngoingStreamTracker,
    pub port: u16,
}

impl StreamMultiplexerClient {
    pub async fn new(
        options: StreamMultiplexerClientOptions,
        tx: mpsc::Sender<Vec<u8>>,
        rx: mpsc::UnboundedReceiver<Vec<u8>>,
    ) -> Result<StreamMultiplexerClient, MultiplexerError> {
        let listen = TcpListener::bind((options.listen_host.clone(), options.listen_port)).await?;
        let port = listen.local_addr()?.port();
        return Ok(StreamMultiplexerClient {
            _options: options,
            listener: listen,
            to_channel_tx: tx,
            from_channel_rx: rx,
            parser: ChannelMessageParser::new(),
            streams: OngoingStreamTracker::new(),
            port: port,
        });
    }

    async fn process_channel_rx(&mut self, data: Vec<u8>) -> Result<(), MultiplexerError> {
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
                    self.streams.data(sd, &self.to_channel_tx).await?;
                }
                Some(ChannelMessage::Disconnect(sd)) => {
                    log::debug!("Client got disconnect {}", sd.stream_id.0);
                    self.streams.remove(&sd.stream_id).await;
                }
                Some(ChannelMessage::Pause(p)) => {
                    log::debug!("Client got pause for stream {}", p.stream_id.0);
                    self.streams.pause_read(&p.stream_id);
                }
                Some(ChannelMessage::Resume(r)) => {
                    log::debug!("Client got resume for stream {}", r.stream_id.0);
                    self.streams.resume_read(&r.stream_id);
                }
            }
        }
        Ok(())
    }

    pub async fn run(&mut self, interrupt: CancellationToken) -> Result<(), MultiplexerError> {
        let mut stream_id: u64 = 1;
        loop {
            tokio::select! {
                _ = interrupt.cancelled() => {
                    log::trace!("multiplexer client stopped");
                    return Ok(());
                },
                new_stream_res = self.listener.accept() => {
                    let (new_stream, new_stream_addr) = new_stream_res?;
                    log::info!("Got a new stream from {}", new_stream_addr);
                    let this_id = OngoingStreamId(stream_id);
                    stream_id += 1;
                    self.to_channel_tx.send(ChannelMessage::Connect(ConnectMsg {
                        stream_id: this_id.clone()
                    }).encode()).await?;
                    self.streams.new_stream(this_id, StreamFactory::Stream(new_stream), interrupt.child_token()).await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(id: u64) -> OngoingStreamId {
        OngoingStreamId(id)
    }

    #[test]
    fn test_roundtrip_connect() {
        let msg = ChannelMessage::Connect(ConnectMsg { stream_id: sid(42) });
        let encoded = msg.encode();
        let parsed = ChannelMessage::parse(&encoded).unwrap().unwrap();
        assert_eq!(parsed.1, encoded.len());
        assert_eq!(parsed.0, msg);
    }

    #[test]
    fn test_roundtrip_disconnect() {
        let msg = ChannelMessage::Disconnect(DisconnectMsg { stream_id: sid(99) });
        let encoded = msg.encode();
        let parsed = ChannelMessage::parse(&encoded).unwrap().unwrap();
        assert_eq!(parsed.1, encoded.len());
        assert_eq!(parsed.0, msg);
    }

    #[test]
    fn test_roundtrip_data() {
        let msg = ChannelMessage::Data(DataMsg {
            stream_id: sid(7),
            to_send: b"hello world".to_vec(),
        });
        let encoded = msg.encode();
        let parsed = ChannelMessage::parse(&encoded).unwrap().unwrap();
        assert_eq!(parsed.1, encoded.len());
        assert_eq!(parsed.0, msg);
    }

    #[test]
    fn test_roundtrip_error() {
        let msg = ChannelMessage::Error(ErrorMsg {
            stream_id: sid(3),
            message: "something broke".to_string(),
        });
        let encoded = msg.encode();
        let parsed = ChannelMessage::parse(&encoded).unwrap().unwrap();
        assert_eq!(parsed.1, encoded.len());
        assert_eq!(parsed.0, msg);
    }

    #[test]
    fn test_parse_multiple_frames() {
        let msg1 = ChannelMessage::Connect(ConnectMsg { stream_id: sid(1) });
        let msg2 = ChannelMessage::Data(DataMsg {
            stream_id: sid(2),
            to_send: b"payload".to_vec(),
        });
        let mut buf = msg1.encode();
        buf.extend(msg2.encode());

        let (parsed1, consumed1) = ChannelMessage::parse(&buf).unwrap().unwrap();
        assert_eq!(parsed1, msg1);

        let (parsed2, consumed2) = ChannelMessage::parse(&buf[consumed1..]).unwrap().unwrap();
        assert_eq!(parsed2, msg2);
        assert_eq!(consumed1 + consumed2, buf.len());
    }

    #[test]
    fn test_parse_incomplete_data_frame() {
        let msg = ChannelMessage::Data(DataMsg {
            stream_id: sid(5),
            to_send: b"some data here".to_vec(),
        });
        let encoded = msg.encode();
        let truncated = &encoded[..encoded.len() - 1];
        assert!(ChannelMessage::parse(truncated).unwrap().is_none());
    }

    #[test]
    fn test_parse_incomplete_header() {
        assert!(ChannelMessage::parse(&[1, 0, 0]).unwrap().is_none());
        assert!(ChannelMessage::parse(&[]).unwrap().is_none());
    }

    #[test]
    fn test_parse_invalid_small_len() {
        // len=5 is below both old (8) and new (13) thresholds
        let mut buf = vec![CONNECT_MSG_ID];
        buf.extend_from_slice(&5u32.to_le_bytes());
        buf.extend_from_slice(&0u64.to_le_bytes());
        assert!(ChannelMessage::parse(&buf).is_err());
    }

    #[test]
    fn test_parse_len_10_is_invalid() {
        // len=10 is between old threshold (8) and new threshold (13).
        // With the old `len < 8` check, this would pass validation and
        // then panic on `data[13..10]`. With `len < 13`, it correctly
        // returns an error.
        let mut buf = vec![CONNECT_MSG_ID];
        buf.extend_from_slice(&10u32.to_le_bytes());
        buf.extend_from_slice(&0u64.to_le_bytes());
        assert!(ChannelMessage::parse(&buf).is_err());
    }

    #[test]
    fn test_roundtrip_pause() {
        let msg = ChannelMessage::Pause(PauseMsg { stream_id: sid(11) });
        let encoded = msg.encode();
        let parsed = ChannelMessage::parse(&encoded).unwrap().unwrap();
        assert_eq!(parsed.1, encoded.len());
        assert_eq!(parsed.0, msg);
    }

    #[test]
    fn test_roundtrip_resume() {
        let msg = ChannelMessage::Resume(ResumeMsg { stream_id: sid(22) });
        let encoded = msg.encode();
        let parsed = ChannelMessage::parse(&encoded).unwrap().unwrap();
        assert_eq!(parsed.1, encoded.len());
        assert_eq!(parsed.0, msg);
    }

    #[test]
    fn test_channel_message_parser_streaming() {
        let msg1 = ChannelMessage::Data(DataMsg {
            stream_id: sid(1),
            to_send: b"first".to_vec(),
        });
        let msg2 = ChannelMessage::Data(DataMsg {
            stream_id: sid(2),
            to_send: b"second".to_vec(),
        });
        let mut combined = msg1.encode();
        combined.extend(msg2.encode());

        let mut parser = ChannelMessageParser::new();
        for byte in combined {
            parser.add(vec![byte]);
        }
        let parsed1 = parser.next().unwrap().unwrap();
        assert_eq!(parsed1, msg1);
        let parsed2 = parser.next().unwrap().unwrap();
        assert_eq!(parsed2, msg2);
        assert!(parser.next().unwrap().is_none());
    }
}
