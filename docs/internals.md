# Internals

## Conceptions
### Tunnel
The entity that maps a domain to the traffic that needs to be forwarded

### Server
The host that runs `http-tunnel` in the server mode. It takes no configuration file: it's started with its two
ports and a shared token, and it learns the tunnels from the clients as they connect.

### Client
The host behind the NAT that runs `http-tunnel` in the client mode. It declares its own tunnels in a
configuration file: each tunnel maps a domain to a local service that needs to be exposed.

### Visitor
Who visits a *tunnel*, via the *server*

### Control Channel
A control channel is a TCP connection between the *server* and the *client* that only carries `http-tunnel` control commands for one *tunnel*. It's also where the client announces the domain of the tunnel, so that the configuration-free server learns it.

### Data Channel

A data channel is a TCP connection between the *server* and the *client* that only carries the encapsulated data that needs forwarding for one *tunnel*.

## The Process

When `http-tunnel` starts in the client mode, it reads its configuration file: a list of *tunnels*, each an HTTP
domain mapped to a `local_addr` on the client. It opens one TCP connection to `server_port` per tunnel. These
connections act as control channels: the client announces the domain in the hello, and the server challenges it by
a nonce, so that the client is required to authenticate with the shared token given with `--token`. Once the token
is validated, the server registers the domain in its routing table — this is how the server, which takes no
configuration file, learns the tunnels of its clients.

The server also listens on `http_port` for HTTP visitors. When a visitor connects, the server reads the HTTP request line and the `Host` header, and looks up
the tunnel that claims that domain. Then it asks the client for a data channel through the corresponding control
channel, and hands the visitor over to the tunnel's connection pool. The client connects to the server to create a
data channel, and the prefetched header bytes are replayed along with the rest of the connection. In this way, a
forwarding is set up. The server also requests a few data channels in advance and caches them, to improve the latency.

The cached data channels are managed by `DataChannelPool`. It keeps a warm pool
of at least `POOL_MIN` channels, topped up every 100 ms, and it asks for one
more channel for every visitor that is routed. Following the number of visitors
instead of the number of active forwardings matters for short-lived connections,
whose forwardings are over almost instantly: the pool would otherwise stay tiny
and the throughput would be bounded by the round-trip time. On top of that base
buffer the pool keeps as many warm channels as the recent demand, a
high-water mark of the requests that were in flight which decays on every
replenish tick. A burst can then be served from the channels that are already
warm instead of waiting for a round trip, and an idle tunnel trims the surplus
back down to `POOL_MIN`.

The channels are counted as *warm* (arrived, waiting to be consumed) and *in
flight* (requested, not arrived yet) separately. A request that fails, or that a
visitor waited for until `DATA_CHANNEL_WAIT_TIMEOUT`, stops counting instead of
leaking, so a client that cannot open channels doesn't leave the pool stuck. A
visitor that finds the pool empty waits for a warm channel, bounded by
`DATA_CHANNEL_WAIT_TIMEOUT`, after which it is answered with `504` rather than
hanging. A single dispatcher task owns the channel receiver and hands each
channel that arrives to the next waiting visitor, so the waiters never queue on
a lock; a visitor that closes its connection while it waits is noticed through a
`peek` and gives up, and the channel that was reserved for it is dropped instead
of being spent on a dead visitor.

A data channel that is handed to a visitor can still die before the tunnel sends
anything back, which is how a client that cannot reach the `local_addr` of its
tunnel fails: the handshake comes before it dials the local service, so it opens
the data channel, then closes it with the visitor's request still unread, and the
server sees a reset. `relay` (in `server.rs`) treats a reset like the end of the
stream and, when not a single byte reached the visitor, answers it with a `504`
instead of leaving it with an empty reply. The failure is logged with the tunnel.

A domain is claimed while the control channel of its tunnel is connected. When the control channel goes away (the
client crashed, was stopped, or lost its connection), the server removes the domain from its routing table, so a
visitor of that host is answered with a `404` instead of being handed over to a tunnel that will never receive it.
A second control channel for a domain that is already claimed takes it over, which is how a reconnecting client is
handled; the clients share one token, so they are trusted, and a takeover by a client of another name is logged as
a warning.

On shutdown, after the HTTP listener stops accepting, the server waits for the
in-flight visitors (up to `DRAIN_TIMEOUT`, 30 seconds) before it tears the
control channels down, so a graceful restart doesn't cut the flows off. The
`ActivityTracker` counts the visitors that are still being served for that.
