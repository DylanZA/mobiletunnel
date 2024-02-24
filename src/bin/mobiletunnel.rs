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
use log;
use simple_logger::SimpleLogger;
use std::process::Command;
use std::{
    fs::File,
    io::{self, Write},
    process::Stdio,
    str::FromStr,
};
use std::{net::TcpListener, time::Duration};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[clap(long, required = true)]
    pub local_port: u16,
    #[clap(long, required = true)]
    pub target_port: u16,
    #[clap(long, required = true)]
    pub target_host: String,
    #[clap(long, default_value = "mobiletunnel_server")]
    pub server_command: String,
    #[clap(long, default_value = "mobiletunnel_client")]
    pub client_command: String,
    #[clap(long, default_value = "ssh")]
    pub ssh_base: String,
    #[clap(long, default_value = "autossh -M 0")]
    pub autossh_base: String,
}

fn port_is_available(port: u16) -> bool {
    match TcpListener::bind(("127.0.0.1", port)) {
        Ok(_) => true,
        Err(_) => false,
    }
}

fn get_available_port() -> Result<u16, Box<dyn std::error::Error>> {
    let res = (24000..30000)
        .find(|port| port_is_available(*port))
        .ok_or("no free local port found?")?;
    return Ok(res);
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    SimpleLogger::new()
        .with_level(log::LevelFilter::Info)
        .env()
        .init()
        .unwrap();
    let args = Args::parse();

    let mut ssh_command: Vec<String> = args.ssh_base.split(" ").map(|x| x.to_string()).collect();
    ssh_command.push(args.target_host.clone());
    ssh_command.push(format!(
        "{} --target-port {} --target-host localhost --bind-port 0 --daemonize --kill-old",
        args.server_command, args.target_port
    ));
    let server_prog = ssh_command.first().ok_or("no ssh command?")?.clone();
    let server_result = Command::new(server_prog)
        .args(ssh_command.into_iter().skip(1))
        .output()?;

    log::info!(
        "Got logs: {}",
        String::from_utf8(server_result.stderr).unwrap_or("<no logs>".to_string())
    );

    let server_port: u16 = String::from_utf8(server_result.stdout)?.parse::<u16>()?;
    log::info!("Got server port {}", server_port);

    let local_port = get_available_port()?;
    log::info!("Using local port of {}", local_port);

    // now run ssh and client

    let mut autossh_command: Vec<String> = args
        .autossh_base
        .split(" ")
        .map(|x| x.to_string())
        .collect();
    autossh_command.push("-T".to_string());
    autossh_command.push(format!(
        "-L localhost:{}:localhost:{}",
        local_port, server_port
    ));
    autossh_command.push(args.target_host.clone());
    autossh_command.push("cat".to_string());

    log::info!("Run auto ssh with {}", autossh_command.join(" "));
    let autossh_prog = autossh_command
        .first()
        .ok_or("no autossh command?")?
        .clone();
    let mut auto_ssh_command_inst = Command::new(autossh_prog)
        .args(autossh_command.into_iter().skip(1))
        .spawn()?;

    let mut client_command: Vec<String> = args
        .client_command
        .split(" ")
        .map(|x| x.to_string())
        .collect();
    client_command.push(format!("--listen-port={}", args.local_port));
    client_command.push(format!("--port={}", local_port));

    log::info!("Run client with {}", client_command.join(" "));
    let client_prog = client_command.first().ok_or("no autossh command?")?.clone();
    let mut client_command_inst = Command::new(client_prog)
        .args(client_command.into_iter().skip(1))
        .stdin(Stdio::null())
        .spawn()?;

    loop {
        log::debug!("looping");
        // todo event driven this
        if let Some(es) = client_command_inst.try_wait()? {
            log::info!("client died");
            break;
        }
        if let Some(es) = auto_ssh_command_inst.try_wait()? {
            log::info!("autossh died with {}", es);
            break;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    let _ = client_command_inst.kill();
    let _ = auto_ssh_command_inst.kill();
    Ok(())
}
