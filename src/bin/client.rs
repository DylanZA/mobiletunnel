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
use std::net::{AddrParseError, IpAddr};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::Duration;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[clap(long, default_value = "127.0.0.1")]
    pub bind_ip: String,
    #[clap(long, required = true)]
    pub port: u16,
    #[clap(long, required = true)]
    pub listen_port: u16,
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
    addr: IpAddr,
    port: u16,
    stream_state_in: reconnecting_stream::StreamState,
) -> Result<(), String> {
    let mut stream_state = stream_state_in;
    let (_, mut end_rx) = mpsc::channel(1);
    loop {
        let stream = TcpStream::connect((addr, port)).await;
        match stream {
            Err(_) => {
                log::debug!("Unable to connect, will try again in a bit");
            }
            Ok(stream) => {
                log::info!("starting stream");
                reconnecting_stream::run_stream::<()>(stream, &mut stream_state, &mut end_rx).await;
                log::info!("... done stream");
            }
        };
        reconnecting_stream::run_unconnected_stream(&mut stream_state, Duration::from_secs(1))
            .await;
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
    log::info!("Running tunnel to {}:{}", args.bind_ip, args.port);
    let bind_address = parse_ip_from_uri_host(&args.bind_ip)?;
    let (stream_state, stream_sender) = reconnecting_stream::StreamState::new();
    let main_chan = tokio::spawn(main_channel(bind_address.clone(), args.port, stream_state));
    log::info!("bind to {}:{}", bind_address, args.listen_port);
    let mut multiplexer = stream_multiplexer::StreamMultiplexerClient::new(
        stream_multiplexer::StreamMultiplexerClientOptions {
            listen_host: bind_address.to_string(),
            listen_port: args.listen_port,
        },
        stream_sender.sender,
        stream_sender.receiver,
    )
    .await?;
    multiplexer.run().await?;
    main_chan.await?;
    Ok(())
}
