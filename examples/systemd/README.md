# Systemd Unit Examples

The directory lists some systemd unit files for example, which can be used to run `http-tunnel` as a service on Linux.

[The `@` symbol in the name of unit files](https://superuser.com/questions/393423/the-symbol-and-systemctl-and-vsftpd) such as
`http-tunnel-server@.service` facilitates the management of multiple instances of `http-tunnel`.

For the naming of the example, `http-tunnel-server` runs `http-tunnel server`, and `http-tunnel-client` runs
`http-tunnel client`.

For security, it is suggested to store the configuration file of the server with permission `600`, that is, only the owner
can read the file, preventing arbitrary users on the system from accessing the secret tokens. The token of a client is
stored in an environment file for the same reason, since a command line argument is visible to every user via `ps`.

The server holds the whole configuration of a deployment. A client is configured by the server, so it only takes the
address of the server, its name and its token.

### With root privilege

Assuming the server is installed at `/usr/bin/http-tunnel`, and its configuration file is in `/etc/http-tunnel/homeserver.toml`,
the following steps show how to run an instance of `http-tunnel server` with root.

1. Create a service file.

```bash
sudo cp http-tunnel-server@.service /etc/systemd/system/
```

2. Create the configuration file `homeserver.toml`.

```bash
sudo mkdir -p /etc/http-tunnel
# And create the configuration file named `homeserver.toml` inside /etc/http-tunnel
```

3. Enable and start the service.

```bash
sudo systemctl daemon-reload # Make sure systemd find the new unit
sudo systemctl enable http-tunnel-server@homeserver --now
```

### Without root privilege

Assuming the server is installed at `~/.local/bin/http-tunnel`, and the configuration file is in
`~/.local/etc/http-tunnel/homeserver.toml`, the following steps show how to run an instance of `http-tunnel server` without root.

1. Edit the example service file as...

```txt
# with root
ExecStart=/usr/bin/http-tunnel server /etc/http-tunnel/%i.toml
# without root
ExecStart=%h/.local/bin/http-tunnel server %h/.local/etc/http-tunnel/%i.toml
```

2. Create a service file.

```bash
mkdir -p ~/.config/systemd/user
cp http-tunnel-server@.service ~/.config/systemd/user/
```

3. Create the configuration file `homeserver.toml`.

```bash
mkdir -p ~/.local/etc/http-tunnel
# And create the configuration file named `homeserver.toml` inside ~/.local/etc/http-tunnel
```

4. Enable and start the service.

```bash
systemctl --user daemon-reload # Make sure systemd find the new unit
systemctl --user enable http-tunnel-server@homeserver --now
```

### Run a client

On the host behind the NAT, the client only needs the address of the server, the name of the client and its token.

1. Edit `http-tunnel-client@.service` and change `--remote` to the address of your server.

2. Create the environment file holding the token.

```bash
sudo mkdir -p /etc/http-tunnel
sudo sh -c 'echo HTTP_TUNNEL_TOKEN=use_a_secret_that_only_you_know > /etc/http-tunnel/client.env'
sudo chmod 600 /etc/http-tunnel/client.env
```

3. Enable and start the service. `%i` is the name of the client, which is also one key of `[clients]` in the
   configuration of the server.

```bash
sudo cp http-tunnel-client@.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable http-tunnel-client@home_nas --now
```

### Run multiple instances

To run multiple instances at once, simply add another configuration, say `app2.toml` under `/etc/http-tunnel`
(`~/.local/etc/http-tunnel` for non-root), then run `sudo systemctl enable http-tunnel-server@app2 --now`
(`systemctl --user enable http-tunnel-server@app2 --now` for non-root) to start an instance for that configuration.

The same applies to `http-tunnel-client@.service` for `http-tunnel client` and `http-tunnel@.service` which is the same as
`http-tunnel-server@.service`.
