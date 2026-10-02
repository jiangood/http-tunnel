# Internals

## Conceptions
### Tunnel
The entity that maps a domain to the traffic that needs to be forwarded

### Server
The host that runs `http-tunnel` in the server mode. It holds the whole configuration

### Client
The host behind the NAT that runs `http-tunnel` in the client mode. It has some local services that need to be exposed, and the *tunnels* that forward them are assigned to it by the *server*

### Visitor
Who visits a *tunnel*, via the *server*

### Config Channel

A config channel is a TCP connection between the *server* and the *client* that carries the configuration pushed by the *server*. A client has no configuration of its own.

### Control Channel
A control channel is a TCP connection between the *server* and the *client* that only carries `http-tunnel` control commands for one *tunnel*.

### Data Channel

A data channel is a TCP connection between the *server* and the *client* that only carries the encapsulated data that needs forwarding for one *tunnel*.

## The Process

When `http-tunnel` starts in the client mode, it connects to `bind_addr` and establishes a config channel. It identifies itself by the name given with `--name`, and the server challenges it by a nonce, so that the client is required to authenticate with the token given with `--token`. In this way, the server knows which *client* of its configuration the connection belongs to.

Then the server pushes the *tunnels* of that client over the config channel, and keeps the channel open, so that the later changes made through the administration API are pushed to the client as well. Each of the tunnels carries the `local_addr` on the client side, so the client doesn't need any configuration file.

The client then creates one connection to `bind_addr` for each of the tunnels. These connections act as control channels. When a control channel starts, the server challenges the client by a nonce, the client is required to authenticate as the tunnel it wants to represent, with the token of the client it belongs to. Then that tunnel is set up.

The server also listens on `http_bind_addr` for HTTP visitors. When a visitor connects, the server reads the HTTP request line and the `Host` header, and looks up
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

On shutdown, after the HTTP listener stops accepting, the server waits for the
in-flight visitors (up to `DRAIN_TIMEOUT`, 30 seconds) before it tears the
control channels down, so a graceful restart doesn't cut the flows off. The
`ActivityTracker` counts the visitors that are still being served for that.

The counters of the HTTP entrypoint (visitors accepted, responses by status,
bytes per direction, pool levels) live in `ServerMetrics` and are exposed by the
administration API at `/api/status`.
