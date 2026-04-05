# mobiletunnel

A reconnecting tcp tunnel with stream multiplexing.
Typical use case is to have an ssh port forward which persists over network changes.

# Usage

The easiest way to use mobiletunnel is with `--copy-self`, which automatically copies the binary to the remote machine over SCP and runs it — no manual server installation required:

```
cargo run --release --bin mobiletunnel -- \
    --copy-self \
    --local-port 8022 \
    --target-host myserver.example.com \
    --target-port 22
```

This will:
1. SCP the mobiletunnel binary to the remote machine (into `/tmp/` by default)
2. SSH to the remote and start the server component as a daemon
3. Set up a local SSH port forward to the server
4. Run the client, which connects through the forward

Now you can `ssh -p 8022 localhost` and the connection will survive network changes.

Options:
* `--copy-self-base /path/` — remote directory to copy the binary to (default: `/tmp/`)
* `--ssh-base "ssh -i ~/.ssh/mykey"` — custom SSH command
* `--scp-base "scp -i ~/.ssh/mykey"` — custom SCP command

Without `--copy-self`, you need to install `mobiletunnel_server` on the remote manually and point to it with `--server-command`.

# Building

All builds produce fully static musl binaries with no glibc dependency, runnable on any Linux. This is required for `--copy-self` to work — the binary copied to the remote must not depend on the remote's glibc version. Requires the musl target:

```
rustup target add x86_64-unknown-linux-musl
```

Then:

```
cargo build            # debug build
cargo build --release  # release build (required for --copy-self)
```

# Security

* Everything is local user priveledge
* No crypto is done in app, it is all handled by the ssh tunnel
