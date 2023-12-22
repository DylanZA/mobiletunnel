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
use libmobiletunnel::{reconnecting_stream, stream_multiplexer};
use log;
use simple_logger::SimpleLogger;
use std::io;
use std::net::{AddrParseError, IpAddr};
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
    if let Err(e) = server.run().await {
        log::error!("Server died due to {}", e);
    }
    main_chan.await?;
    Ok(())
}
