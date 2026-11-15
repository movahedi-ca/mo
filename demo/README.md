# MO demo

A runnable demo of MO on a generic Linux host.

## Run it

```sh
cargo build --release --features cli
sh demo/run-demo.sh
```

## What it does

1. Starts `mo-daemon` with `MO_HOST=generic-linux` in a fresh temp dir.
   The database, socket, lock, and dashboard all live in that dir.
2. Fires 15 concurrent `cpu-ram` lease requests (1 core, 256 MB,
   5 minutes, priority P2) through `mo-cli`.
3. Prints how many were granted and how many queued. On a small host
   most requests queue. That is the scheduler doing its job, and the
   demo reports it as it happened.
4. Heartbeats and releases every grant.
5. Cancels every request that stayed queued.
6. Restarts the daemon on the same database and reads status and the
   lease list back. State lives in SQLite, so nothing active is lost
   in a restart.

The demo touches no production daemon, no production database, and
no display. It opens no network port. MO never does.

A saved run from the launch night is in `output-2026-11-15.txt`.
Numbers in that file are from that run, on that host, only.
