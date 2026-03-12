# Full DAG Graphs

This page separates the currently supported protocol model into two conceptual graphs:

- downstream DAGs on the client-facing side
- upstream and exchange DAGs that the runtime may allocate or advance while serving a request

Notes:

- `exchange 1` is the reserved default upstream HTTP exchange.
- `exchange n` represents additional outbound exchanges allocated with `http::exchange::new()`.
- `udp socket 1` is the reserved default upstream UDP handle; `udp socket n` represents additional outbound sockets allocated with `udp::socket::new()`.
- `webrtc connection 1` is the reserved default upstream WebRTC handle; `webrtc connection n` represents additional outbound connections allocated with `webrtc::connection::new()`.
- These graphs show the union of currently supported DAG families. `http`, `http2`, `tls`, `websocket`, and `webrtc` are feature-gated; the default build enables `http`, `tls`, and `websocket`.
- Current HTTP/2 support lives under the generic HTTP exchange layer. The VM still uses `http::exchange::*`; feature `http2` owns upstream `h2` session reuse explicitly and tracks downstream HTTP/2 sessions in the data-plane server.
- HTTP/2 now has declared internal `session` and `stream` goals, explicit stream carrier refs attached to exchanges, and GOAWAY/reset frontier tracking. It is still an internal carrier DAG rather than a separate VM-visible `http2::*` ABI.
- Internally, carrier-specific policy is now split into `src/abi_impl/http1/` and `src/abi_impl/http2/`, while the generic exchange state remains under `src/abi_impl/http/`.
- VM host calls, request execution, graph resolution, and proxy byte-stream wiring are runtime control layers, not protocol goals. They are intentionally omitted from the graphs below.
- UDP datagrams and WebRTC data-channel messages do not currently flow through `proxy::pipe` or `proxy::tunnel`; they remain sibling message-oriented DAGs.
- These graphs are intentionally conceptual. They show ingress and egress connections between DAGs, not every internal transition implemented by each subsystem.

## Downstream Graph

```mermaid
flowchart LR
    subgraph DS_TCP["Downstream TCP DAG"]
        DT0["listener pending"]
        DT1["downstream connected"]
        DT2["downstream rx bytes"]
        DT3["downstream tx bytes"]
        DT4["downstream closed"]
        DT0 --> DT1
        DT1 --> DT2
        DT1 --> DT3
        DT2 --> DT4
        DT3 --> DT4
    end

    subgraph DS_TLS["Downstream TLS DAG"]
        DTL0["tls ingress attached"]
        DTL1["downstream handshake in progress"]
        DTL2["downstream plaintext ready"]
        DTL3["downstream tls closed or failed"]
        DTL0 --> DTL1
        DTL1 --> DTL2
        DTL2 --> DTL3
    end

    subgraph DS_HTTP["Downstream HTTP DAG"]
        DH0["http ingress admitted"]
        DH1["request head ready"]
        DH2["request body stream"]
        DH3["response output draft"]
        DH4["client response committed"]
        DH0 --> DH1
        DH0 --> DH2
        DH1 --> DH3
        DH3 --> DH4
    end

    subgraph DS_WS["Downstream WebSocket Child DAG"]
        DW0["upgrade observed on handle 0"]
        DW1["downstream websocket ingress documented"]
        DW0 --> DW1
    end

    subgraph DS_UDP["Downstream UDP Placeholder"]
        DU0["udp handle 0 reserved"]
        DU1["downstream udp unavailable in current runtime"]
        DU0 --> DU1
    end

    subgraph DS_WRTC["Downstream WebRTC Placeholder"]
        DWR0["webrtc handle 0 reserved"]
        DWR1["downstream webrtc unavailable in current runtime"]
        DWR0 --> DWR1
    end

    DT1 --> DTL0
    DT1 --> DH0
    DTL2 --> DH0
    DH1 --> DW0
```

## Upstream And Exchange Graph

```mermaid
flowchart LR
    subgraph US_TCP["Upstream TCP DAG"]
        UT0["dial pending"]
        UT1["upstream connected"]
        UT2["upstream rx bytes"]
        UT3["upstream tx bytes"]
        UT4["upstream closed"]
        UT0 --> UT1
        UT1 --> UT2
        UT1 --> UT3
        UT2 --> UT4
        UT3 --> UT4
    end

    subgraph US_TLS["Upstream TLS Session DAG"]
        UTL0["tls configured"]
        UTL1["session selected"]
        UTL2["plaintext ready"]
        UTL3["tls closed or failed"]
        UTL0 --> UTL1
        UTL1 --> UTL2
        UTL2 --> UTL3
    end

    subgraph US_H2["Upstream HTTP/2 Carrier DAG"]
        UH20["http2 session candidate"]
        UH21["http2 session open"]
        UH22["http2 stream attached to exchange"]
        UH20 --> UH21
        UH21 --> UH22
    end

    subgraph US_UDP["Upstream UDP Datagram DAG"]
        UU0["udp socket allocated"]
        UU1["udp bind configured"]
        UU2["udp target configured"]
        UU3["udp connected"]
        UU4["udp tx datagrams"]
        UU5["udp rx datagrams"]
        UU6["udp closed or failed"]
        UU0 --> UU1
        UU0 --> UU2
        UU1 --> UU2
        UU2 --> UU3
        UU3 --> UU4
        UU3 --> UU5
        UU4 --> UU6
        UU5 --> UU6
    end

    subgraph EX1["Upstream HTTP Exchange 1 DAG"]
        U1A["exchange 1 request draft"]
        U1B["exchange 1 request body stream"]
        U1C["exchange 1 send started"]
        U1D["exchange 1 response headers"]
        U1E["exchange 1 response body stream"]
        U1A --> U1B
        U1B --> U1C
        U1C --> U1D
        U1D --> U1E
    end

    subgraph EXN["Upstream HTTP Dynamic Exchange DAG"]
        UN0["exchange n allocated"]
        UN1["exchange n request draft"]
        UN2["exchange n request body stream"]
        UN3["exchange n send started"]
        UN4["exchange n response headers"]
        UN5["exchange n response body stream"]
        UN0 --> UN1
        UN1 --> UN2
        UN2 --> UN3
        UN3 --> UN4
        UN4 --> UN5
    end

    subgraph WS["Outbound WebSocket Child DAG"]
        W0["websocket upgrade request"]
        W1["websocket handshake started"]
        W2["websocket open"]
        W3["rx frame stream"]
        W4["tx frame stream"]
        W5["websocket closed"]
        W0 --> W1
        W1 --> W2
        W2 --> W3
        W2 --> W4
        W3 --> W5
        W4 --> W5
    end

    subgraph WRTC["Outbound WebRTC Data-Channel DAG"]
        R0["webrtc configured"]
        R1["remote description set"]
        R2["local offer or answer created"]
        R3["peer connecting"]
        R4["data channel open"]
        R5["rx message queue"]
        R6["tx message queue"]
        R7["webrtc closed or failed"]
        R0 --> R1
        R0 --> R2
        R1 --> R2
        R2 --> R3
        R3 --> R4
        R4 --> R5
        R4 --> R6
        R5 --> R7
        R6 --> R7
    end

    UT1 --> UTL0
    UT1 --> U1A
    UT1 --> UN1
    UTL2 --> U1A
    UTL2 --> UN1
    UTL2 --> UH20
    UH22 --> U1A
    UH22 --> UN1

    U1A --> W0
    UN1 --> W0
    R0 -. signaling outside current DAG .-> R1
```
