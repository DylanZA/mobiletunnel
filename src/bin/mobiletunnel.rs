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
use libmobiletunnel::process_helper::set_kill_on_parent_death;
use libmobiletunnel::server_check_port::run_check_server_port;
use log;
use rand::distributions::Alphanumeric;
use rand::Rng;
use simple_logger::SimpleLogger;
use std::env;
use std::path::Path;
use std::process::{Command, Stdio};
use std::{net::TcpListener, time::Duration};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child as TokioChild;
use tokio::process::Command as TokioCommand;
use tokio::runtime::Runtime;
use tokio::time::sleep as TokioSleep;
use tokio::time::Duration as TokioDuration;
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

    #[clap(long, default_value = "500000000")]
    pub window: usize,

    #[clap(long)]
    pub debug_server: bool,
}

impl Args {
    fn make_location(&self, base: &str) -> Option<(String, String)> {
        let username: String = std::env::var("USER").unwrap_or("nouser".to_string());
        let base = format!("{}_{}", base, username);
        let s: String = rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(15)
            .map(char::from)
            .collect();

        let ret = Path::new(&self.copy_self_base)
            .join(format!("{}_{}", base, s))
            .to_str()?
            .to_string();
        return Some((ret, base));
    }
    fn make_pid_file_location(&self) -> Option<(String, String)> {
        self.make_location("mobiletunnel_pid")
    }
    fn make_self_location(&self) -> Option<(String, String)> {
        self.make_location("mobiletunnel")
    }
}

fn get_available_port() -> Result<(TcpListener, u16), Box<dyn std::error::Error>> {
    let mut rng = rand::thread_rng();
    for _ in 0..100 {
        let port = rng.gen_range(24000..32000);
        let res = TcpListener::bind(("127.0.0.1", port))?;
        let local_port = res.local_addr()?.port();
        return Ok((res, local_port));
    }
    return Err("Failed to find a free port")?;
}

struct AutoSsh {
    child: TokioChild,
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
}

fn tokio_command_set_kill_on_parent_death(cmd: &mut TokioCommand) -> &mut TokioCommand {
    unsafe {
        return cmd.pre_exec(|| Ok(set_kill_on_parent_death()));
    }
}

fn mk_ssh(command: &Vec<String>) -> Result<AutoSsh, Box<dyn std::error::Error>> {
    log::info!("Run reconncting ssh with {}", command.join(" "));
    let reconnecting_ssh_prog = command
        .first()
        .ok_or("no reconnecting ssh command?")?
        .clone();

    let mut child = tokio_command_set_kill_on_parent_death(
        TokioCommand::new(reconnecting_ssh_prog)
            .args(command.into_iter().skip(1))
            .stdout(Stdio::piped()),
    )
    .spawn()?;

    let stdout: tokio::process::ChildStdout =
        child.stdout.take().ok_or("No stdout on ssh command")?;
    let lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>> =
        BufReader::new(stdout).lines();
    let ret = AutoSsh {
        child: child,
        lines: lines,
    };
    return Ok(ret);
}

enum RunSshResult {
    ServerDied,
    SshDied,
}
async fn run_ssh(
    mut auto_ssh: AutoSsh,
    cancel: &CancellationToken,
) -> Result<RunSshResult, Box<dyn std::error::Error>> {
    loop {
        let sleep = TokioSleep(TokioDuration::from_secs(10));
        tokio::select! {
            waitres = auto_ssh.child.wait() => {
                let es = waitres?;
                log::info!("reconnecting ssh died with {}, restarting", es);
                return Ok(RunSshResult::SshDied);
            },
            lineres = auto_ssh.lines.next_line() => {
                let line_opt = lineres?;
                if let Some(line) = line_opt {
                    if line.starts_with("BAD") {
                        log::info!("It looks like the server running has died, so terminating. msg={}", line);
                        return Ok(RunSshResult::ServerDied);
                    }
                }
            },
            _ = sleep => {
                log::error!("Timeout waiting for line from running server, attempting to terminate ssh");
                let _ = auto_ssh.child.start_kill(); // ignore result
            },
            _ = cancel.cancelled() => {
                let _ = auto_ssh.child.start_kill(); // ignore result
            }
        }
    }
}

async fn run_ssh_loop(
    reconnecting_ssh_command: Vec<String>,
    cancel: CancellationToken,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut running_ssh = run_ssh(mk_ssh(&reconnecting_ssh_command.clone())?, &cancel);
    loop {
        if cancel.is_cancelled() {
            return Ok(());
        }
        match running_ssh.await? {
            RunSshResult::ServerDied => {
                return Ok(());
            }
            RunSshResult::SshDied => {
                running_ssh = run_ssh(mk_ssh(&reconnecting_ssh_command.clone())?, &cancel);
            }
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    SimpleLogger::new()
        .with_level(log::LevelFilter::Info)
        .env()
        .init()
        .unwrap();

    let all_args: Vec<String> = env::args().collect();
    if all_args.len() == 3 && all_args[1] == "check-server-pid" {
        set_kill_on_parent_death();
        let pid = all_args[2].parse::<u32>()?;
        return run_check_server_port(&all_args[0], pid);
    }

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

    let ssh_prog: String = ssh_command.first().ok_or("no ssh command?")?.clone();
    let ssh_command_base: Vec<String> = ssh_command.into_iter().skip(1).collect();

    let (daemon_pid_file_location, _) = args
        .make_pid_file_location()
        .ok_or("cannot make a pid file location")?;
    let server_arg_string = format!(
        "--target-port={} --target-host=localhost --bind-port=0 --daemonize={} --kill-old",
        args.target_port, daemon_pid_file_location
    );

    log::info!("daemon file @ {}", daemon_pid_file_location);
    let mut server_command: Vec<String> = ssh_command_base.clone();
    let target_server_prog: String = if args.copy_self {
        let (target_server_prog, kill_base) = args
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
        server_command.push(format!(
            "{} {} --local-port 0 --target-port 0 --target-host 0 --run-as-server=\"{} --kill-old-base={} --window={}\"",
            if args.debug_server {"RUST_LOG=debug"} else {""}, target_server_prog, server_arg_string, kill_base, args.window
        ));
        target_server_prog
    } else {
        server_command.push(format!("{} {}", args.server_command, server_arg_string));
        args.server_command.clone()
    };

    let server_result = Command::new(&ssh_prog).args(server_command).output()?;

    log::info!(
        "Got logs: {}",
        String::from_utf8(server_result.stderr).unwrap_or("<no logs>".to_string())
    );

    let server_output = String::from_utf8(server_result.stdout)?;
    let server_port: u16 = server_output.parse::<u16>()?;
    log::info!("Got server port {}", server_port);

    // now to find the server pid
    let mut server_pid_maybe: Option<u32> = None;
    for _ in 0..20 {
        let mut server_pid_args = ssh_command_base.clone();
        server_pid_args.push(format!("cat {}", daemon_pid_file_location));

        let server_pid_result = Command::new(&ssh_prog).args(server_pid_args).output()?;

        log::info!(
            "Got pid logs: {}",
            String::from_utf8(server_pid_result.stderr).unwrap_or("<no logs>".to_string())
        );

        let server_pid_output = String::from_utf8(server_pid_result.stdout)?;
        log::debug!("server pid output was {}", server_pid_output);
        if server_pid_output.len() == 0 {
            log::info!("logs not there, sleeping");
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }

        server_pid_maybe = Some(server_pid_output.trim().parse::<u32>()?);
        break;
    }

    let server_pid = server_pid_maybe.ok_or("no server pid found")?;
    log::info!("Got server pid {}", server_pid);

    let (local_port_bind, local_port) = get_available_port()?;
    log::info!("Using local port of {}", local_port);

    // now run ssh and client
    let check_command = format!("{} check-server-pid {}", target_server_prog, server_pid);

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
    reconnecting_ssh_command.push(check_command);

    // race condition here, have to race with mk_ssh to get a free local port
    drop(local_port_bind);

    let client_args = libmobiletunnel::client::Args {
        listen_port: args.local_port,
        port: local_port,
        bind_ip: "127.0.0.1".to_string(),
        window: args.window,
    };
    let client_cancel: CancellationToken = CancellationToken::new();
    let client_cancel_child = client_cancel.child_token();
    let tokio_runtime = Runtime::new()?;
    let client_run = async move {
        libmobiletunnel::client::run_client(client_args, client_cancel_child)
            .await
            .map_err(|e| e.to_string())
    };
    let ssh_cancel: CancellationToken = CancellationToken::new();
    let ssh_run = run_ssh_loop(reconnecting_ssh_command, ssh_cancel.child_token());
    tokio_runtime.block_on(tokio_runtime.spawn(async move {
        let _ = tokio::join!(
            tokio::spawn(async move {
                let ssh_run_result = ssh_run.await;
                log::info!("ssh run finished: {:?}", ssh_run_result);
                client_cancel.cancel();
            }),
            tokio::spawn(async move {
                let client_run_result = client_run.await;
                log::info!("client finished: {:?}", client_run_result);
                ssh_cancel.cancel();
            }),
        );
    }))?;

    Ok(())
}
