# mobiletunnel

A reconnecting tcp tunnel with stream multiplexing.
Typical use case is to have an ssh port forward which persists over network changes.

# Usage

Right now not very ergonomic:
1) install the server on remote
2) cargo run --bin mobiletunnel -- --local-port <LOCAL_BIND_PORT> --target-host <host> --target-port <TARGET PORT> --server-command ..../mobiletunnel/target/release/mobiletunnel_server  --client-command target/debug/mobiletunnel_client

now you can connect to <LOCAL_BIND_PORT> and the connection will stay alive over network breaks!

# Security

* Everything is local user priveledge
* No crypto is done in app, it is all handled by the ssh tunnel
