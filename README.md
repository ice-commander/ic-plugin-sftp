# SFTP

SFTP connections for Ice Commander, as a plugin.

It registers one connection kind, `sftp`, and hands the host the form for it —
`documents/sftp.json`, drawn by the application rather than by the plugin — plus
the icon the connections list and the drives list show, `sftp.svg`, registered as
a plugin asset under `ic-sftp-fs`. It claims no extension: a panel gets here by
connecting to a server, never by opening a file, and `open_in` returns nothing on
purpose.

## The form

Name and folder are the record's own; the rest is the connection: host, port
(22 unless told otherwise), user, and either a password or a private key with an
optional passphrase — which of the two the form shows follows the
**Authentication Type** choice. **Remote Path** is `opens_at`: where the panel
stands when the connection opens, not a prefix. The mount is the whole server
and paths are the server's own: `/` is its root, so an empty Remote Path opens
there, and only an empty path from the host means the login folder. **Use SSH
Tunnel** opens a block for the jump host: host, port (22 unless told otherwise),
user, and its own password or key with passphrase, chosen by **Tunnel Auth
Type**.

## What it does

Listing, reading, writing, new folders, renaming, deleting and changing
permissions, straight through `ssh2`; deleting a folder walks it and takes
everything under it. A permissions change is an SFTP `setstat` of the mode alone
on the one file or folder it names, never what is under it. Nothing is marked
read-only.

One session is kept per mount and reused. Before each call it is checked with a
`stat` of the login folder, and a call that fails once reconnects and is tried
again — so a connection dropped while the panel sat idle costs one retry rather
than an error. What did fail is kept as text and handed back through
`last_error`.

## The SSH tunnel

With **Use SSH Tunnel** on, the plugin logs in to the jump host and listens on
`127.0.0.1` on a free port. Every connection accepted there gets its own
`direct-tcpip` channel through the jump session to the server's host and port,
as the jump host sees them, and the SFTP session connects to that local port
instead of the server. One jump session is kept per mount: while it is
authenticated it is reused, otherwise — or once a channel through it fails
because the jump host's connection is gone — it is dropped and a new one is
opened.
The tunnel lives as long as the mount; closing the mount stops the listener, and
the jump session goes once the last connection through it ends.

The jump key path is `~`-expanded. A tunnel without a host or a user, or without
the credentials its auth type asks for, fails with a message saying which.

## The terminal

The kind carries a shell, so a panel standing in an SFTP connection can open a
terminal on that server. It is a second SSH connection of its own with an
`xterm-256color` pty, and it `cd`s into the folder the panel is showing. With the
tunnel on it goes through the same local port, opening the tunnel first if the
mount has not yet. It runs on a thread with `std` channels and polls
non-blocking, so reading, writing and resizing all go through the same loop.

The key path is `~`-expanded for the file side and not for the terminal, which
takes it as written.

## Building

```sh
./build.sh
./test.sh
./deploy-local.sh
```

Then switch it on in **Settings → Plugins** and restart.

`ssh2` is built with `vendored-openssl`, so the first build compiles OpenSSL and
libssh2 from source and takes a few minutes; after that it is cached in
`bin/target`.

`./test.sh` leaves out the end-to-end tests that need a server, and a jump host
for the tunnelled ones; the variables they read are listed in
`src/sftp-fs/src/mount.rs`. The walk expects `IC_SFTP_E2E_HOME` to hold
`docs/inner/file.txt` reading `ic-sftp-e2e`, and checks the modes it sets with
the server's own `stat -c %a`:

```sh
cargo test -- --ignored
```

## Licence

MIT or Apache-2.0, at your option, except the icon — see
[THIRD-PARTY-LICENSES.md](THIRD-PARTY-LICENSES.md). Contributions are taken under the
[DCO](DCO); sign off with `git commit -s`.
