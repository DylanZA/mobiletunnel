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
use futures::TryFutureExt;
use log;
use rand::distributions::Alphanumeric;
use rand::Rng;
use simple_logger::SimpleLogger;
use std::path::Path;
use std::process::Command;
use std::{net::TcpListener, time::Duration};
use tokio::runtime::Runtime;
use tokio_util::sync::CancellationToken;

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
    #[clap(long, default_value = "ssh")]
    pub ssh_base: String,
    #[clap(long, default_value = "scp")]
    pub scp_base: String,
    #[clap(
        long,
        default_value = "ssh -o ServerAliveInterval=2 -o ServerAliveCountMax=2"
    )]
    pub reconnecting_ssh_base: String,
    #[clap(long)]
    pub copy_self: bool,
    #[clap(long, default_value = "")]
    pub run_as_server: String,
    /// note the below is probably a security hole
    /// Someone could intercept the copy and place a bad executable there.
    #[clap(long, default_value = "/tmp/")]
    pub copy_self_base: String,
}

impl Args {
    fn make_self_location(&self) -> Option<String> {
        let s: String = rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(15)
            .map(char::from)
            .collect();
        let username: String = std::env::var("USER").unwrap_or("nouser".to_string());

        return Some(
            Path::new(&self.copy_self_base)
                .join(format!("mobiletunnel_{}_{}", username, s))
                .to_str()?
                .to_string(),
        );
    }
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

    if args.run_as_server.len() > 0 {
        log::info!(
            "Short circuiting to run as the server using args {}",
            args.run_as_server
        );
        let splitted = shellwords::split(&args.run_as_server)?;
        let full_args = std::iter::once(
            std::env::current_exe()?
                .to_str()
                .ok_or("cannot get current exe")?
                .to_string(),
        )
        .chain(splitted);
        let server_args = libmobiletunnel::server::Args::parse_from(full_args);
        return libmobiletunnel::server::run_server(server_args);
    }

    let mut ssh_command: Vec<String> = args.ssh_base.split(" ").map(|x| x.to_string()).collect();
    ssh_command.push(args.target_host.clone());
    let server_arg_string = format!(
        "--target-port={} --target-host=localhost --bind-port=0 --daemonize --kill-old",
        args.target_port
    );
    if args.copy_self {
        let target_server_prog = args
            .make_self_location()
            .ok_or("cannot make a random location")?;
        log::info!("Copying server to remote at {}", target_server_prog);
        let mut scp_command: Vec<String> =
            args.scp_base.split(" ").map(|x| x.to_string()).collect();
        scp_command.push(
            std::env::current_exe()?
                .to_str()
                .ok_or("cannot get current exe")?
                .to_string(),
        );
        scp_command.push(format!("{}:{}", args.target_host, target_server_prog));
        let scp_prog = scp_command.first().ok_or("no scp command?")?.clone();
        let copy_server_result = Command::new(scp_prog)
            .args(scp_command.into_iter().skip(1))
            .output()?;
        log::info!(
            "Copy server: got logs: {}",
            String::from_utf8(copy_server_result.stderr).unwrap_or("<no logs>".to_string())
        );
        ssh_command.push(format!(
            "{} --local-port 0 --target-port 0 --target-host 0 --run-as-server=\"{}\"",
            target_server_prog, server_arg_string
        ));
    } else {
        ssh_command.push(format!("{} {}", args.server_command, server_arg_string));
    }

    let server_prog = ssh_command.first().ok_or("no ssh command?")?.clone();
    let server_result = Command::new(server_prog)
        .args(ssh_command.into_iter().skip(1))
        .output()?;

    log::info!(
        "Got logs: {}",
        String::from_utf8(server_result.stderr).unwrap_or("<no logs>".to_string())
    );

    let mut server_port: u16 = String::from_utf8(server_result.stdout)?.parse::<u16>()?;
    log::info!("Got server port {}", server_port);

    let local_port = get_available_port()?;
    log::info!("Using local port of {}", local_port);

    // now run ssh and client

    let mut reconnecting_ssh_command: Vec<String> = args
        .reconnecting_ssh_base
        .split(" ")
        .map(|x| x.to_string())
        .collect();
    reconnecting_ssh_command.push(format!(
        "-L localhost:{}:localhost:{}",
        local_port, server_port
    ));
    reconnecting_ssh_command.push(args.target_host.clone());
    reconnecting_ssh_command.push("cat".to_string());

    let mk_ssh =
        |command: &Vec<String>| -> Result<std::process::Child, Box<dyn std::error::Error>> {
            log::info!("Run reconncting ssh with {}", command.join(" "));
            let reconnecting_ssh_prog = command
                .first()
                .ok_or("no reconnecting ssh command?")?
                .clone();
            return Ok(Command::new(reconnecting_ssh_prog)
                .args(command.into_iter().skip(1))
                .spawn()?);
        };

    let mut auto_ssh_command_inst = mk_ssh(&reconnecting_ssh_command.clone())?;

    let client_args = libmobiletunnel::client::Args {
        listen_port: args.local_port,
        port: local_port,
        bind_ip: "127.0.0.1".to_string(),
    };
    let client_cancel = CancellationToken::new();
    let client_cancel_child = client_cancel.child_token();
    let tokio_runtime = Runtime::new()?;
    let client_run = tokio_runtime.spawn(async move {
        libmobiletunnel::client::run_client(client_args, client_cancel_child)
            .await
            .map_err(|e| e.to_string())
    });

    loop {
        log::debug!("looping");
        // todo event driven this
        if client_run.is_finished() {
            log::info!("client died");
            break;
        }
        if let Some(es) = auto_ssh_command_inst.try_wait()? {
            log::info!("reconnecting ssh died with {}, restarting", es);
            auto_ssh_command_inst = mk_ssh(&reconnecting_ssh_command.clone())?;
            continue;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    client_cancel.cancel();
    let _ = auto_ssh_command_inst.kill();
    Ok(())
}
