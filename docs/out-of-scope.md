# Out of Scope

`http-tunnel` focuses on HTTP forwarding for NAT traversal, rather than being an all-in-one development tool or a load balancer or a gateway. It's designed to *be used with them*, not *replace them*.

> Make each program do one thing well.

- *HTTP Request Logging*

  `http-tunnel` doesn't interfere with the application layer traffic beyond reading the first request's `Host` header to route the connection. A right place for this kind of stuff is the web server, and a network capture tool.

- *Per-request HTTP features*

  Routing is connection-level: only the first request of a connection is inspected. Therefore features that have to apply to *every* request — `X-Forwarded-*` injection, per-tunnel Basic Auth, path rewriting, re-routing a keep-alive connection by a later `Host` — are out of scope unless the HTTP layer is terminated on the server. `http-tunnel` deliberately pipes the connection through untouched, which is what makes WebSocket and any other upgrade transparent.

- *`frp`'s STCP or other setup that requires visitors' side configuration*

  If that kind of setup is possible, then there are a lot more tools available. You may want to consider secure tunnels like wireguard or zerotier. `http-tunnel` primarily focuses on NAT traversal by forwarding, which doesn't require any setup for visitors.

- *Mesh discovery and route advertisement*

  A chain is configured explicitly, hop by hop: a downstream always dials its
  upstream, and a process with `[upstream]` advertises its routes on the next
  hop. A v4 session carries many prefixes and many visitor streams; there is no
  automatic peer discovery, no prefix aggregation across independent sessions, no
  hop count and no per-request loop detection. `ht` validates that its upstream
  is not its own listener and rejects duplicate prefixes, but general loop
  detection belongs to a later protocol iteration, not to the HTTP-only data
  plane.
