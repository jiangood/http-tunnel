# Out of Scope

`http-tunnel` focuses on HTTP forwarding for NAT traversal, rather than being an all-in-one development tool or a load balancer or a gateway. It's designed to *be used with them*, not *replace them*.

> Make each program do one thing well.

- *HTTP Request Logging*

  `http-tunnel` doesn't interfere with the application layer traffic beyond reading the first request's `Host` header to route the connection. A right place for this kind of stuff is the web server, and a network capture tool.

- *`frp`'s STCP or other setup that requires visitors' side configuration*

  If that kind of setup is possible, then there are a lot more tools available. You may want to consider secure tunnels like wireguard or zerotier. `http-tunnel` primarily focuses on NAT traversal by forwarding, which doesn't require any setup for visitors.
