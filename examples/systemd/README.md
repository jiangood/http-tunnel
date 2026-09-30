# Systemd Unit Examples

The directory lists some systemd unit files for example, which can be used to run `rathole` as a service on Linux.

[The `@` symbol in the name of unit files](https://superuser.com/questions/393423/the-symbol-and-systemctl-and-vsftpd) such as
`ratholes@.service` facilitates the management of multiple instances of `rathole`.

For the naming of the example, `ratholes` stands for `rathole server`, `ratholec` stands for `rathole client`.

For security, it is suggested to store the configuration file of the server with permission `600`, that is, only the owner
can read the file, preventing arbitrary users on the system from accessing the secret tokens. The token of a client is
stored in an environment file for the same reason, since a command line argument is visible to every user via `ps`.

The server holds the whole configuration of a deployment. A client is configured by the server, so it only takes the
address of the server, its name and its token.

### With root privilege

Assuming the server is installed at `/usr/bin/rathole`, and its configuration file is in `/etc/rathole/homeserver.toml`,
the following steps show how to run an instance of `rathole server` with root.

1. Create a service file.

```bash
sudo cp ratholes@.service /etc/systemd/system/
```

2. Create the configuration file `homeserver.toml`.

```bash
sudo mkdir -p /etc/rathole
# And create the configuration file named `homeserver.toml` inside /etc/rathole
```

3. Enable and start the service.

```bash
sudo systemctl daemon-reload # Make sure systemd find the new unit
sudo systemctl enable ratholes@homeserver --now
```

### Without root privilege

Assuming the server is installed at `~/.local/bin/rathole`, and the configuration file is in
`~/.local/etc/rathole/homeserver.toml`, the following steps show how to run an instance of `rathole server` without root.

1. Edit the example service file as...

```txt
# with root
ExecStart=/usr/bin/rathole server /etc/rathole/%i.toml
# without root
ExecStart=%h/.local/bin/rathole server %h/.local/etc/rathole/%i.toml
```

2. Create a service file.

```bash
mkdir -p ~/.config/systemd/user
cp ratholes@.service ~/.config/systemd/user/
```

3. Create the configuration file `homeserver.toml`.

```bash
mkdir -p ~/.local/etc/rathole
# And create the configuration file named `homeserver.toml` inside ~/.local/etc/rathole
```

4. Enable and start the service.

```bash
systemctl --user daemon-reload # Make sure systemd find the new unit
systemctl --user enable ratholes@homeserver --now
```

### Run a client

On the host behind the NAT, the client only needs the address of the server, the name of the client and its token.

1. Edit `ratholec@.service` and change `--remote` to the address of your server.

2. Create the environment file holding the token.

```bash
sudo mkdir -p /etc/rathole
sudo sh -c 'echo RATHOLE_TOKEN=use_a_secret_that_only_you_know > /etc/rathole/client.env'
sudo chmod 600 /etc/rathole/client.env
```

3. Enable and start the service. `%i` is the name of the client, which is also one key of `[clients]` in the
   configuration of the server.

```bash
sudo cp ratholec@.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable ratholec@home_nas --now
```

### Run multiple instances

To run multiple instances at once, simply add another configuration, say `app2.toml` under `/etc/rathole`
(`~/.local/etc/rathole` for non-root), then run `sudo systemctl enable ratholes@app2 --now`
(`systemctl --user enable ratholes@app2 --now` for non-root) to start an instance for that configuration.

The same applies to `ratholec@.service` for `rathole client` and `rathole@.service` which is the same as
`ratholes@.service`.
