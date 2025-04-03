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

use bytes::{BufMut, BytesMut};
use futures::stream::StreamExt;
use rand::Rng;
use std::fmt::Display;
use std::io;
use std::net::IpAddr;
use std::str;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration};
use tokio_util::codec::FramedRead;
use tokio_util::codec::{Decoder, Encoder};
use tokio_util::sync::CancellationToken;

use crate::util::SwallowResultPrintErrExt as _;

pub struct StreamOptions {
    max_window: usize,
}

pub struct StreamState {
    id: u64,
    remote_id: Option<u64>,
    options: StreamOptions,
    sent_unacked: Vec<Vec<u8>>,
    base_offset: u64,
    receiver: mpsc::Receiver<Vec<u8>>, // data to send accross channel
    sender: mpsc::UnboundedSender<Vec<u8>>, // sender for data received from channel
    received_seq: u64,                 // last received message seq number
}

pub struct StreamSender {
    pub sender: mpsc::Sender<Vec<u8>>, // data to send across channel
    pub receiver: mpsc::UnboundedReceiver<Vec<u8>>, // data received from channel
}

impl StreamSender {
    pub async fn send(&self, value: Vec<u8>) -> Result<(), mpsc::error::SendError<Vec<u8>>> {
        self.sender.send(value).await
    }
}

pub trait DataProvider {
    fn getdata(&self) -> String {
        String::from("(Read more...)")
    }
}

impl StreamOptions {
    pub fn new(max_window: usize) -> StreamOptions {
        StreamOptions {
            max_window: max_window,
        }
    }

    pub fn default() -> StreamOptions {
        return StreamOptions {
            max_window: 1000 * 1000 * 30,
        };
    }
}

pub struct HelloMessage {
    sender_id: u64,
    sender_last_rx: u64,
}
impl StreamState {
    async fn enqueue(&mut self, data: Vec<u8>) -> io::Result<()> {
        self.sent_unacked.push(data);
        Ok(())
    }

    pub async fn run_client(
        self,
        addr: IpAddr,
        port: u16,
        mut cancel: CancellationToken,
    ) -> Result<(), String> {
        let mut stream_state = self;
        loop {
            if cancel.is_cancelled() {
                log::trace!("run_client: cancelled, ending");
                return Ok(());
            }
            log::trace!("run_client: connecting");
            let stream = TcpStream::connect((addr, port)).await;
            match stream {
                Err(_) => {
                    log::debug!("Unable to connect, will try again in a bit");
                }
                Ok(stream) => {
                    log::info!("run_client: starting stream");
                    run_stream("client", stream, &mut stream_state, &mut cancel).await;
                    log::info!("run_client: ... done stream");
                }
            };

            run_unconnected_stream(&mut stream_state, Duration::from_secs(1), &mut cancel).await;
        }
    }

    pub async fn run_server(
        self,
        listener: TcpListener,
        cancel: CancellationToken,
    ) -> Result<(), io::Error> {
        let mut stream_state = self;

        let (socket_tx, mut socket_rx) = mpsc::channel(32);
        let listen_cancel = cancel.child_token();
        let listener_task = tokio::spawn(async move {
            loop {
                log::trace!("run_server: waiting for socket");
                tokio::select! {
                    _ = listen_cancel.cancelled() => {
                        log::info!("run_server: cancelled");
                        drop(socket_tx);
                        return;
                    },
                    listener_accept = listener.accept() => {
                        match listener_accept {
                            Ok(res) => {
                                log::info!("run_server: New socket");
                                match socket_tx.send(res).await {
                                    Ok(_) => {
                                        log::debug!("... sent");
                                    }
                                    Err(e) => {
                                        log::error!("... had error {}", e);
                                    }
                                }
                            }
                            Err(e) => {
                                log::error!("Error {} accepting, ending", e);
                                return;
                            }
                        }
                    },
                };
            }
        });

        let runner_cancel = cancel.child_token();
        let runner_task = tokio::spawn(async move {
            let mut next_socket = None;
            loop {
                if runner_cancel.is_cancelled() {
                    log::info!("runner cancelled");
                    break;
                }
                let mut sub_cancel = runner_cancel.child_token();

                // wait for a socket:
                tokio::select! {
                    _ = sub_cancel.cancelled() => {
                        log::info!("runner sub-cancelled");
                        break;
                    },
                    n = socket_rx.recv() => {
                        next_socket = n;
                    },
                };

                // drain the queue
                loop {
                    match socket_rx.try_recv() {
                        Ok(s) => {
                            log::info!("server: ...dropped unused socket");
                            next_socket = Some(s);
                        }
                        Err(e) => {
                            break;
                        }
                    }
                }
                if next_socket.is_none() {
                    log::info!("server: ending, next socket is none");
                    return;
                }
                if let Some((stream, sockaddr)) = next_socket {
                    log::info!("new stream from {}", sockaddr);
                    run_stream("server", stream, &mut stream_state, &mut sub_cancel).await;
                    log::info!("... done stream");
                }
            }
        });
        listener_task.await?;
        runner_task.await?;
        return Ok(());
    }

    fn received_one_message(&mut self) -> u64 {
        self.received_seq += 1;
        return self.received_seq;
    }

    fn ack(&mut self, seq: u64) -> io::Result<()> {
        if seq < self.base_offset {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "acked seq is before a previously acked one",
            ));
        }
        let to_drain: usize = (seq - self.base_offset)
            .try_into()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "seq > base offset"))?;
        if to_drain > self.sent_unacked.len() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!(
                    "seq {} > sent_unacked (off={} unacked={})",
                    seq,
                    self.base_offset,
                    self.sent_unacked.len()
                ),
            ));
        }
        self.sent_unacked.drain(0..to_drain);
        self.base_offset = seq;
        Ok(())
    }

    fn make_hello(&self) -> HelloMessage {
        HelloMessage {
            sender_id: self.id,
            sender_last_rx: self.received_seq,
        }
    }

    fn unacked_size(&self) -> usize {
        self.sent_unacked.iter().map(|x| x.len()).sum()
    }

    fn is_full(&self) -> bool {
        return self.unacked_size() >= self.options.max_window;
    }

    pub fn new(stream_options: StreamOptions) -> (StreamState, StreamSender) {
        let mut rng = rand::thread_rng();

        let (tx, rx) = mpsc::channel(1);
        let (tx2, rx2) = mpsc::unbounded_channel();
        (
            StreamState {
                id: rng.gen::<u64>(),
                remote_id: None,
                options: stream_options,
                sent_unacked: Vec::new(),
                base_offset: 0,
                received_seq: 0,
                receiver: rx,
                sender: tx2,
            },
            StreamSender {
                sender: tx,
                receiver: rx2,
            },
        )
    }

    pub fn default() -> (StreamState, StreamSender) {
        StreamState::new(StreamOptions::default())
    }
}

const HELLO_MSG_ID: u8 = 1;
const DATA_MSG_ID: u8 = 2;
const ACK_MSG_ID: u8 = 3;

pub enum StreamCodecMessage {
    Hello(HelloMessage),
    Data(Vec<u8>),
    Ack(u64),
}

pub struct StreamCodec {
    name: String,
}

impl StreamCodecMessage {
    fn to_string(&self) -> String {
        match self {
            StreamCodecMessage::Hello(_hm) => format!("Hello"),
            StreamCodecMessage::Data(d) => format!("Data({})", d.len()),
            StreamCodecMessage::Ack(u) => format!("Ack({})", u),
        }
    }

    fn parse<S>(prefix: S, data: &[u8]) -> Result<Option<(StreamCodecMessage, usize)>, io::Error>
    where
        S: Display,
    {
        log::trace!("{}: Stream codec parse {}", prefix, data.len());
        if data.len() < 1 {
            return Ok(None);
        }
        let rest = &data[1..];
        match data[0] {
            HELLO_MSG_ID => {
                if rest.len() >= 16 {
                    let a = HelloMessage {
                        sender_id: u64::from_le_bytes(rest[0..8].try_into().unwrap()),
                        sender_last_rx: u64::from_le_bytes(rest[8..16].try_into().unwrap()),
                    };
                    let res = (StreamCodecMessage::Hello(a), 17);
                    Ok(Some(res))
                } else {
                    Ok(None)
                }
            }
            ACK_MSG_ID => {
                if rest.len() >= 8 {
                    let res = (
                        StreamCodecMessage::Ack(u64::from_le_bytes(rest[0..8].try_into().unwrap())),
                        9,
                    );
                    Ok(Some(res))
                } else {
                    Ok(None)
                }
            }
            DATA_MSG_ID => {
                if rest.len() >= 4 {
                    let msg_size = u32::from_le_bytes(rest[0..4].try_into().unwrap())
                        .try_into()
                        .map_err(|_| io::Error::new(io::ErrorKind::Other, "bad msg size"))?;
                    let other = &rest[4..];
                    if other.len() >= msg_size {
                        let ret = StreamCodecMessage::Data(other[..msg_size].to_vec());
                        let consumed_size = 1 + 4 + msg_size;
                        return Ok(Some((ret, consumed_size)));
                    }
                }
                Ok(None)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::Other,
                format!("unknown msg id {}!", data[0]),
            )),
        }
    }

    fn encode(&self) -> Vec<u8> {
        let mut ret: Vec<u8> = vec![];
        match self {
            StreamCodecMessage::Hello(hm) => {
                ret.push(HELLO_MSG_ID);
                ret.extend_from_slice(&hm.sender_id.to_le_bytes());
                ret.extend_from_slice(&hm.sender_last_rx.to_le_bytes());
            }
            StreamCodecMessage::Data(v) => {
                ret.push(DATA_MSG_ID);
                let sz: u32 = v.len().try_into().unwrap();
                ret.extend_from_slice(&sz.to_le_bytes());
                ret.extend(v);
            }
            StreamCodecMessage::Ack(s) => {
                ret.push(ACK_MSG_ID);
                ret.extend_from_slice(&s.to_le_bytes());
            }
        }
        ret
    }
}

impl Decoder for StreamCodec {
    type Item = StreamCodecMessage;
    type Error = io::Error;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<StreamCodecMessage>, Self::Error> {
        match StreamCodecMessage::parse(&self.name, buf)? {
            None => Ok(None),
            Some((msg, skip)) => {
                buf.split_to(skip);
                return Ok(Some(msg));
            }
        }
    }
}

impl Encoder<StreamCodecMessage> for StreamCodec {
    type Error = io::Error;

    fn encode(&mut self, item: StreamCodecMessage, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let encoded = item.encode();
        dst.put_slice(&encoded);
        Ok(())
    }
}

pub async fn run_unconnected_stream(
    state: &mut StreamState,
    duration: Duration,
    cancel: &mut CancellationToken,
) {
    let sleep = sleep(duration);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                break;
            },
            _ = &mut sleep => {
                // sleep finished
                break;
            },
            app_rx = state.receiver.recv(), if !state.is_full() => {
                match app_rx {
                    None => {
                        log::error!("app channel closed!");
                        break;
                    },
                    Some(msg) => {
                        log::debug!("Got {} bytes from app while sleeping: {}", msg.len(), str::from_utf8(&msg).unwrap_or("<unknown>"));
                        let enq_res = state.enqueue(msg.clone()).await;
                        if let Err(err) = enq_res {
                            log::error!("Error enqueing data {}", err);
                            break;
                        }
                    }
                }
            },
        }
    }
}

fn parse(data: &[u8]) -> String {
    match StreamCodecMessage::parse("", data) {
        Err(e) => {
            format!("Error {}", e)
        }
        Ok(None) => format!("Nothing"),
        Ok(Some((m, l))) => format!("Some message {} len {}", m.to_string(), l),
    }
}

pub async fn run_stream(
    name: &'static str,
    stream: TcpStream,
    state: &mut StreamState,
    kill_stream_rx: &mut CancellationToken,
) {
    let (read_stream, mut write_stream) = stream.into_split();
    let initial_message = StreamCodecMessage::Hello(state.make_hello()).encode();
    log::debug!("{}: sending {}", name, parse(&initial_message));
    if write_stream.write_all(&initial_message).await.is_err() {
        log::info!("{}: Could not write hello", name);
        return;
    }

    // use a channel and spawned task as I don't trust FramedRead's cancel safety
    let (joined_channel_tx, mut joined_rx) = mpsc::unbounded_channel();
    let joined_channel_tx_2 = joined_channel_tx.clone();
    let joined_channel_tx_3 = joined_channel_tx.clone();
    let framed_reader_task = tokio::spawn(async move {
        let mut framed_rx = FramedRead::new(
            read_stream,
            StreamCodec {
                name: name.to_string(),
            },
        );
        loop {
            log::trace!("{}: framed_reader looping", name);
            let next = framed_rx.next().await;
            match next {
                Some(Ok(msg)) => {
                    log::trace!("{}: Received {}", name, msg.to_string());
                    if let Err(e) = joined_channel_tx.send(Some(msg)) {
                        log::error!("channel seems to have died: {}", e);
                        break;
                    }
                }
                None => {
                    log::info!("{}: tcp session closed for some reason", name);
                    joined_channel_tx
                        .send(None)
                        .swallow_or_print_err(format!("{} tcp closed", name));
                    break;
                }
                Some(Err(e)) => {
                    log::info!("{}: tcp session closed due to {}", name, e);
                    joined_channel_tx
                        .send(None)
                        .swallow_or_print_err(format!("{} tcp closed {}", name, e));
                    break;
                }
            }
        }
    });

    let (writer_tx, mut writer_rx) = mpsc::unbounded_channel();
    let framed_writer_task = tokio::spawn(async move {
        loop {
            let next: Option<Option<Vec<u8>>> = writer_rx.recv().await;
            match next {
                Some(Some(msg)) => {
                    log::debug!("{}: Writing to channel {}", name, parse(&msg));
                    if write_stream.write_all(&msg).await.is_err() {
                        log::info!("{}: tcp write failed, closing session", name);
                        joined_channel_tx_2.send(None); // ignore result
                        break;
                    }
                }
                None | Some(None) => {
                    log::info!("{}: tcp write session closed", name);
                    joined_channel_tx_2.send(None); // ignore result
                    break;
                }
            }
        }
    });

    tokio::select! {
        _ = kill_stream_rx.cancelled()  => {
            log::info!("{}: stream killed while waiting for hello, ending", name);
            return;
        },
        unflat_msg = joined_rx.recv() => {
            let msg = unflat_msg.flatten();
            log::trace!("{}: got message is_none={}", name, msg.is_none());
            if let Some(StreamCodecMessage::Hello(hm)) = msg {
                log::trace!("{} got hello", name);
                if let Some(other_id) = state.remote_id {
                    if other_id != hm.sender_id {
                        log::error!("{}: Bad sender id", name);
                        return;
                    }
                }
                state.remote_id = Some(hm.sender_id);
                if let Err(e) = state.ack(hm.sender_last_rx) {
                    log::error!("{}: Bad last rx {}", name, e);
                    return;
                }
            } else {
                log::info!("{}: First message not a hello", name);
                return;
            }
        }
    };

    // send anything we have in the queue:
    for msg in state.sent_unacked.iter() {
        writer_tx
            .send(Some(StreamCodecMessage::Data(msg.clone()).encode()))
            .unwrap();
    }

    loop {
        log::trace!("{}: looping", name);
        tokio::select! {
            _ = kill_stream_rx.cancelled()  => {
                log::info!("{}: stream killed while processing, ending", name);
                return;
            },
            app_rx = state.receiver.recv(), if !state.is_full() => {
                match app_rx {
                    None => {
                        log::error!("{}: app channel closed!", name);
                        break;
                    },
                    Some(msg) => {
                        log::debug!("{}: Got {} bytes from app (as_utf=\"{}\")", name, msg.len(), str::from_utf8(&msg).unwrap_or("<not utf8>"));
                        let enq_res = state.enqueue(msg.clone()).await;
                        if let Err(err) = enq_res {
                            log::error!("{}: Error enqueing data {}", name, err);
                            break;
                        }
                        if let Err(err) = writer_tx.send(Some(StreamCodecMessage::Data(msg).encode())) {
                            log::error!("{}: Error sending data {}", name, err);
                            break;
                        }
                    }
                }
            },
            tcp_rx = joined_rx.recv() => {
                match tcp_rx {
                    None | Some(None) => {
                        // tcp closed,
                        log::info!("{}: tcp closed, stop select", name);
                        break;
                    },
                    Some(Some(msg)) => {
                        log::trace!("{}: Got a message from tcp session ", name);
                        match msg {
                            StreamCodecMessage::Hello(_) => {
                                log::info!("{}: Another hello!?!", name);
                                break;
                            },
                            StreamCodecMessage::Ack(a) => {
                                log::trace!("{}: ... ack  {}", name, a);
                                if let Err(e) = state.ack(a) {
                                    log::error!("{}: Bad ack {}", name, e);
                                    return;
                                }
                            },
                            StreamCodecMessage::Data(data) => {
                                log::trace!("{}: ... data {}", name, data.len());
                                state.sender.send(data).swallow_or_print_err(name);
                                writer_tx.send(Some(StreamCodecMessage::Ack(state.received_one_message()).encode())).swallow_or_print_err(name);
                            }
                        }
                    }
                }
            }
        };
    }
    writer_tx.send(None);
    joined_channel_tx_3.send(None);
    tokio::join!(framed_reader_task);
    tokio::join!(framed_writer_task);
    log::debug!("Join done");
    return;
}
