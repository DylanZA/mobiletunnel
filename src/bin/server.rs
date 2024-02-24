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
use libc::{getuid, kill, SIGTERM};
use libmobiletunnel::{reconnecting_stream, stream_multiplexer};
use log;
use simple_logger::SimpleLogger;
use std::env;
use std::fs::File;
use std::io::{self, Write};
use std::net::{AddrParseError, IpAddr};
use std::path::Path;
use sysinfo::{get_current_pid, System};
use tokio::net::TcpListener;

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
) -> io::Result<()> {
    let mut stream_state = stream_state_in;
    loop {
        let (stream, sockaddr) = listener.accept().await?;
        log::info!("new stream from {}", sockaddr);
        reconnecting_stream::run_stream(stream, &mut stream_state).await;
        log::info!("... done stream");
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    SimpleLogger::new()
        .with_level(log::LevelFilter::Info)
        .env()
        .init()
        .unwrap();
    let args = Args::parse();
    log::info!("Running tunnel on {}:{}", args.bind_ip, args.bind_port);
    let bind_address = parse_ip_from_uri_host(&args.bind_ip)?;
    let (stream_state, stream_sender) = reconnecting_stream::StreamState::new();
    let bind_sockaddr = (bind_address, args.bind_port);
    let listener = TcpListener::bind(bind_sockaddr).await?;
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
        let stderr = File::create(format!("{}_stderr.log", args.logs_location)).unwrap();
        log::info!("Writing bind port ({}) to stdout", listener_port);
        print!("{}", listener_port);
        io::stdout().flush().unwrap();
        let daemonize = Daemonize::new().stderr(stderr);
        daemonize.start()?;
    }

    if let Err(e) = server.run().await {
        log::error!("Server died due to {}", e);
    }

    main_chan.await?;
    Ok(())
}
