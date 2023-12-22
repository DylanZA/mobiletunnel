# mobiletunnel

A reconnecting tcp tunnel with stream multiplexing.
Typical use case is to have an ssh port forward which persists over network changes.

# Usage

Right now not very ergonomic:
1) start the server on remote in tmux or screen: 
    cargo run --release --bin server -- --target-port <TARGET PORT> --target-host localhost --bind-port 0
2) note down the bind port (or specify one) as <REMOTE_BIND_PORT>
3) start the client locally, target a local free port
    cargo run --release --bin client -- --port <LOCAL_TUNNEL_PORT> --listen-port <LOCAL_BIND_PORT>
4) setup an autossh tunnel to the remote:
    autossh -M 0  -L localhost:<LOCAL_TUNNEL_PORT>:localhost:<REMOTE_BIND_PORT> <remote hostname>

now you can connect to <LOCAL_BIND_PORT> and the connection will stay alive over network breaks!

# Security

* Everything is local user priveledge
* No crypto is done in app, it is all handled by the ssh tunnel
