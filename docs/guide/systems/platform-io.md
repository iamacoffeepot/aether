# Platform and network I/O

Native capabilities turn host resources into bounded mail contracts. Guest code
never receives an unmediated socket, file descriptor, clipboard handle, or
credential.

| Boundary | Chapter |
|---|---|
| Namespaced host files | [File I/O](file-io.md) |
| Outbound HTTP | [HTTP egress](http.md) |
| Inbound HTTP/routes/streams | [HTTP server](http-server.md) |
| Framed connections | [TCP](tcp.md) |
| Internal process transport | [RPC](rpc.md) |
| Text clipboard | [Clipboard](clipboard.md) |
| Provider APIs/subprocesses | [Content generation](content-generation.md) |

Across these systems, preserve the same boundary rules:

- blocking OS/provider work leaves the actor dispatcher;
- actor state remains the serialization point;
- inputs, buffers, in-flight work, and lifetimes are bounded;
- failures become typed replies or terminal events;
- credentials and raw handles stay native;
- headless/disabled backends fail fast rather than hang settlement.

Use a higher-level capability when it exists. HTTP route actors are safer and
more observable than reimplementing HTTP over raw TCP; content-generation
adapters are safer than sending credentials through general egress.
