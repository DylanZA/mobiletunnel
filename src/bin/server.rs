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

use clap::Parser;
use daemonize::Daemonize;
use futures::TryFutureExt;
use libc::{getuid, kill, SIGTERM};
use libmobiletunnel::{reconnecting_stream, stream_multiplexer};
use log;
use simple_logger::SimpleLogger;
use std::env;
use std::fs::File;
use std::io::{self, Write};
use std::net::TcpListener as StdTcpListener;
use std::net::{AddrParseError, IpAddr};
use std::path::Path;
use sysinfo::{get_current_pid, System};
use tokio::net::TcpListener;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[clap(long, default_value = "127.0.0.1")]
    pub bind_ip: String,
    #[clap(long)]
    pub bind_port: u16,
    #[clap(long, required = true)]
    pub target_port: u16,
    #[clap(long, required = true)]
    pub target_host: String,
    #[clap(long, action)]
    pub daemonize: bool,
    #[clap(long, action)]
    pub kill_old: bool,

    #[clap(long, default_value = "/var/tmp/mobiletunnel")]
    pub logs_location: String,
}

fn ipv6_stripped(host: &str) -> &str {
    let h = host.strip_prefix("[").unwrap_or(host);
    return h.strip_suffix("]").unwrap_or(h);
}

fn parse_ip_from_uri_host(host: &str) -> Result<IpAddr, AddrParseError> {
    host.parse::<IpAddr>().or_else(|_|
        // Parsing failed, try as bracketed IPv6
        ipv6_stripped(host)
            .parse::<IpAddr>())
}

async fn main_channel(
    listener: TcpListener,
    stream_state_in: reconnecting_stream::StreamState,
) -> Result<(), io::Error> {
    let mut stream_state = stream_state_in;

    let (socket_tx, mut socket_rx) = mpsc::channel(32);

    let listener_task = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok(res) => {
                    log::info!("New socket");
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
        }
    });

    let runner_task = tokio::spawn(async move {
        let mut next_socket = None;
        loop {
            if next_socket.is_none() {
                next_socket = socket_rx.recv().await;
                log::info!("Received socket: {}", next_socket.is_some());
            }
            // drain the queue
            loop {
                match socket_rx.try_recv() {
                    Ok(s) => {
                        log::info!("...dropped unused socket");
                        next_socket = Some(s);
                    }
                    Err(_) => break,
                }
            }
            if let Some((stream, sockaddr)) = next_socket {
                log::info!("new stream from {}", sockaddr);
                next_socket =
                    reconnecting_stream::run_stream(stream, &mut stream_state, &mut socket_rx)
                        .await;
                log::info!("... done stream");
            }
        }
    });
    listener_task.await?;
    runner_task.await?;
    return Ok(());
}

#[tokio::main]
async fn tokio_main(
    args: Args,
    std_listener: StdTcpListener,
) -> Result<(), Box<dyn std::error::Error>> {
    std_listener.set_nonblocking(true)?;
    let listener = TcpListener::from_std(std_listener)?;
    let bind_address = parse_ip_from_uri_host(&args.bind_ip)?;
    let (stream_state, stream_sender) = reconnecting_stream::StreamState::new();
    let listener_port = listener.local_addr()?.port();
    log::info!("bound to {}:{}", bind_address, listener_port);
    let main_chan = tokio::spawn(main_channel(listener, stream_state));

    let mut server = stream_multiplexer::StreamMultiplexerServer::new(
        stream_multiplexer::StreamMultiplexerServerOptions {
            target_port: args.target_port,
            target_address: args.target_host,
        },
        stream_sender.sender,
        stream_sender.receiver,
    )?;

    if let Err(e) = server.run().await {
        log::error!("Server died due to {}", e);
    }
    if let Err(e) = main_chan.await? {
        log::error!("Main channel error {}", e);
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    SimpleLogger::new()
        .with_level(log::LevelFilter::Info)
        .env()
        .init()
        .unwrap();
    let args = Args::parse();
    log::info!("Running tunnel on {}:{}", args.bind_ip, args.bind_port);
    let bind_address = parse_ip_from_uri_host(&args.bind_ip)?;
    let bind_sockaddr = (bind_address, args.bind_port);
    let rt = Runtime::new()?;
    let listener = StdTcpListener::bind(bind_sockaddr)?;

    if args.kill_old {
        let mut sys = System::new_all();

        // Prints each argument on a separate line
        let process_name = env::args()
            .next()
            .ok_or("Unable to determine process name")?;
        let process_file_name = Path::new(&process_name)
            .file_name()
            .map(|p| p.to_str())
            .flatten()
            .ok_or("Unable to determine process file name")?;
        let our_pid = get_current_pid()?;
        let mut our_uid = 0;
        unsafe {
            our_uid = getuid();
        }
        for (pid, process) in sys.processes() {
            if pid == &our_pid {
                continue;
            }
            if let Some(_) = process.thread_kind() {
                // don't care about threads
                continue;
            }
            match process.exe().and_then(|x| x.file_name()) {
                None => continue,
                Some(exe) => {
                    let exe_str = exe
                        .to_str()
                        .map(|x| x.to_string().replace(" (deleted)", ""));
                    match exe_str {
                        None => continue,
                        Some(exe_str_2) => {
                            if exe_str_2 != process_file_name {
                                continue;
                            }
                        }
                    }
                }
            }
            match process.user_id() {
                None => continue,
                Some(uid) => {
                    if **uid != our_uid {
                        continue;
                    }
                }
            }
            log::info!("Killing {} ({})", pid, process.name());
            let kill_res = process.kill();
            log::info!("... result {}", kill_res);
        }
    }
    if args.daemonize {
        let username = env::var("USER").unwrap_or("nouser".to_string());
        let our_pid = get_current_pid()?;
        let stderr = File::create(format!(
            "{}_{}_{}_stderr.log",
            args.logs_location, username, our_pid
        ))
        .unwrap();
        let listener_port = listener.local_addr()?.port();
        log::info!("Writing bind port ({}) to stdout", listener_port);
        print!("{}", listener_port);
        io::stdout().flush().unwrap();
        let daemonize = Daemonize::new().stderr(stderr);
        daemonize.start()?;
    }
    return tokio_main(args, listener);
}
