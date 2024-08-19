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

use crate::{reconnecting_stream, stream_multiplexer};
use clap::Parser;
use daemonize::Daemonize;
use futures::future;
use futures::FutureExt;
use futures::TryFutureExt;
use futures::{future::Either, pin_mut};
use libc::{getuid, kill, SIGTERM};
use log;
use simple_logger::SimpleLogger;
use std::env;
use std::fs::File;
use std::future::Future;
use std::io::{self, Write};
use std::net::TcpListener as StdTcpListener;
use std::net::{AddrParseError, IpAddr};
use std::path::Path;
use sysinfo::{get_current_pid, System};
use tokio::net::TcpListener;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
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

    #[clap(long, default_value = None)]
    pub kill_old_base: Option<String>,
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
    let interrupt = CancellationToken::new();
    let mut main_chan = tokio::spawn(stream_state.run_server(listener, interrupt.child_token()));

    let mut server = stream_multiplexer::StreamMultiplexerServer::new(
        stream_multiplexer::StreamMultiplexerServerOptions {
            target_port: args.target_port,
            target_address: args.target_host,
        },
        stream_sender.sender,
        stream_sender.receiver,
    )?;

    future::join(
        server.run(interrupt.child_token()).then(|res_server| {
            interrupt.cancel();
            if let Err(e) = res_server {
                log::error!("Server died with error {}", e);
            } else {
                log::info!("Server finished cleanly");
            }
            return async { () };
        }),
        main_chan.then(|res_channel| {
            interrupt.cancel();
            match res_channel {
                Err(je) => {
                    log::error!("Main channel died with join error {}", je);
                }
                Ok(Err(e)) => {
                    log::error!("Main channel died with error {}", e);
                }
                Ok(Ok(_)) => {
                    log::info!("Main channel finished cleanly");
                }
            }
            return async { () };
        }),
    )
    .await;
    Ok(())
}

pub fn run_server(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    log::info!("Running tunnel on {}:{}", args.bind_ip, args.bind_port);
    let bind_address = parse_ip_from_uri_host(&args.bind_ip)?;
    let bind_sockaddr = (bind_address, args.bind_port);
    let _ = Runtime::new()?;
    let listener = StdTcpListener::bind(bind_sockaddr)?;

    if args.kill_old {
        let sys = System::new_all();

        let (match_str, match_whole) = match &args.kill_old_base {
            None => {
                let process_name = env::args()
                    .next()
                    .ok_or("Unable to determine process name")?;
                let process_file_name = Path::new(&process_name)
                    .file_name()
                    .map(|p| p.to_str())
                    .flatten()
                    .ok_or("Unable to determine process file name")?
                    .to_string();
                (process_file_name, true)
            }
            Some(x) => (x.to_string(), false),
        };

        let our_pid = get_current_pid()?;
        let mut our_uid = 0;
        unsafe {
            our_uid = getuid();
        }
        let is_match = |x: &str| match match_whole {
            true => x == match_str,
            false => x.starts_with(&match_str),
        };
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
                            if !is_match(&exe_str_2) {
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
            log::info!(
                "Killing {} ({})",
                pid,
                process.name().to_str().unwrap_or("<none>")
            );
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
