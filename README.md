# propar

A Rust port of [Bronkhorst's ProPar master](https://github.com/bronkhorst-developer/bronkhorst-propar/tree/ebda5ba3cf3f22b02cfd58666783734cd5fd0ae0). Use it to communicate with a local RS232/RS485 instrument or address several nodes on a FLOW-BUS network.

This Cargo workspace separates the protocol from parameter data and transports:

| Crate | Purpose | Default requirements |
| --- | --- | --- |
| `propar-core` | Frame and message codecs, protocol types and constants | `no_std`, no allocator or dependencies |
| `propar-catalog` | Numeric FlowDDE definitions and address lookup | `no_std`, depends only on `propar-core`; descriptive text is opt-in |
| `propar-embedded` | Async master over embedded serial streams | `no_std`, no allocator, runtime or catalog; embedded I/O and HAL traits |
| `propar` | Blocking and async clients, discovery, and serial adapters | Blocking serial client and catalog metadata |

`propar` remains the package at the repository root; the other crates live under `crates/`. It re-exports the codecs as `propar::frame` and `propar::message`, and parameter lookup as `propar::catalog`. Embedded applications can depend directly on `propar-core` for codecs or `propar-embedded` for an async master without the catalog.

## Blocking serial

The default features include the blocking serial client. `Master::open` uses enhanced binary mode at 38400 baud, 8N1. `local()` selects address `0x80` and channel 1.

```rust,no_run
use propar::blocking::Master;

fn main() -> Result<(), propar::Error> {
    let mut bus = Master::open("/dev/ttyUSB0")?;
    let mut flow = bus.local();
    println!("Measure: {} (32000 = full scale)", flow.measure()?);

    // Read engineering-unit measure and capacity unit together by FlowDDE number.
    let values = flow.read_many(&[205, 129])?;
    println!("{values:?}");
    Ok(())
}
```

Use `bus.instrument(address, channel)?` to select another node or channel. The instrument borrows the master for the duration of its use. For example, `flow.set_setpoint(16_000)?` writes 50% of full scale and waits for the device acknowledgement. For raw process/parameter addressing use `bus.read`, `bus.write`, `bus.read_many`, and `bus.write_many`.

## Async serial

Enable the `async-serial` feature. Call the same methods on `asynchronous::Master` inside a Tokio runtime. The example uses Tokio’s `rt` and `macros` features.

```rust,no_run
use propar::asynchronous::Master;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), propar::Error> {
    let mut bus = Master::open("/dev/ttyUSB0")?;
    let nodes = bus.get_nodes(false).await?; // Follow links from the local node.
    for node in nodes {
        let mut instrument = bus.instrument(node.address, 1)?;
        println!("node {}: {:?}", node.address, instrument.id().await?);
    }
    Ok(())
}
```

For an existing serial port or another byte stream, use `Master::new(stream, mode)` with `std::io::Read + Write` or Tokio's `AsyncRead + AsyncWrite + Unpin`. Both clients issue one transaction at a time. `on_broadcast` handles unsolicited messages encountered during a transaction. Call `next_broadcast` to listen while idle.

The default transaction timeout is 500 ms. Use `set_timeout` to change it. A custom blocking stream must have finite OS-level I/O timeouts because a generic `Read` or `Write` call can block past the deadline. The Tokio client enforces the deadline on the stream operations. A timeout or cancellation while waiting for a binary reply allows a new request with a new sequence number. An interrupted frame write, or an abandoned ASCII request whose reply could still arrive, returns `Error::Desynchronized` on reuse. Reopen or otherwise resynchronize the transport before creating a new master. A missing acknowledgement leaves the outcome of a write unknown.

## Embedded async

Use `propar-embedded` for an allocation-free `no_std` master. It uses `embedded-io-async` 0.7 `Read + Write` for serial traffic and `embedded-hal-async` 1.0 `DelayNs` for timeouts. The HAL's async serial traits live in `embedded-io-async`, as described in the [HAL documentation](https://docs.rs/embedded-hal-async/1.0.0/embedded_hal_async/#serialuart-traits).

```toml
[dependencies]
propar-embedded = { path = "../propar-rs/crates/propar-embedded" }
```

Construct `propar_embedded::Master::new(stream, delay, mode)` with your HAL's async serial stream and a delay implementation. `read` and `read_many` use a caller-provided reply buffer; returned strings borrow that buffer. `write` and `write_many` accept borrowed values. Both ASCII and binary framing are supported, with no executor or catalog dependency. See the [embedded crate example](crates/propar-embedded/README.md) for a complete generic function.

`set_timeout_ms` changes the default 500 ms timeout for the whole transaction. The delay must support cancellation and reuse because a completed exchange drops the pending timer. An interrupted transaction, including a timeout after I/O starts, returns `Error::Desynchronized` on reuse in either framing mode. Recover the stream with `into_inner()`, resynchronize it externally, and create a new master. Unsolicited broadcasts are skipped while waiting for a reply; discovery and the desktop `Instrument` wrapper are not included.

The root crate also exposes this client as `propar::embedded` with `default-features = false, features = ["embedded-async"]`. Depend directly on `propar-embedded` to omit the catalog entirely.

## Codec only

Depend on `propar-core` for the dependency-free `no_std` protocol. It uses only `core` and requires no allocator. The crate has no default features and contains no FlowDDE catalog. For a local checkout, adjust this path to your application:

```toml
[dependencies]
propar-core = { path = "../propar-rs/crates/propar-core" }
```

`MessageRef` serializes slices of requests or values into your output buffer. `DecodedMessage` validates a payload and returns iterators whose string values borrow the input.

`FrameRef` serializes the wire framing into your buffer. `PacketRef::decode_into` parses one packet into caller-provided payload storage. `Decoder::push` handles fragmented streams with a fixed 258-byte internal buffer and lends the completed packet until the next mutable use of the decoder.

```rust
use propar_core::frame::{FrameRef, Mode, PacketRef, MAX_FRAME_LEN};
use propar_core::message::{
    DataType, DecodedMessage, MessageRef, Parameter, Request, ValueRef, MAX_PAYLOAD_LEN,
};

let setpoint = Parameter::new(1, 1, DataType::U16);
let requests = [Request::new(setpoint)];
let mut payload = [0; MAX_PAYLOAD_LEN];
let len = MessageRef::Request(&requests).encode_into(&mut payload).unwrap();

let frame = FrameRef { sequence: 1, address: 0x80, payload: &payload[..len] };
let mut wire = [0; MAX_FRAME_LEN];
let len = frame.encode_into(Mode::Ascii, &mut wire).unwrap();
assert_eq!(&wire[..len], b":06800401210121\r\n");

let mut received = [0; MAX_PAYLOAD_LEN];
let PacketRef::Frame(reply) = PacketRef::decode_into(
    Mode::Ascii, b":06800201213E80\r\n", &mut received,
).unwrap() else {
    panic!("expected an addressed reply");
};
let DecodedMessage::Send { mut parameters, .. } =
    DecodedMessage::decode(reply.payload).unwrap()
else {
    panic!("expected parameter data");
};
let entry = parameters.next().unwrap().unwrap();
assert_eq!(entry.into_value_for(setpoint).unwrap(), ValueRef::U16(16_000));
```

Encoding checks the full message before writing and returns `BufferTooSmall` when output storage is insufficient. Enable `propar-core`'s `std` feature for the owned `Message`, `Value`, `Frame`, and `Packet` convenience types. The `propar` blocking and async features enable it automatically.

For numeric FlowDDE lookup on an embedded device, add `propar-catalog` without enabling its `metadata` feature:

```toml
propar-catalog = { path = "../propar-rs/crates/propar-catalog" }
```

```rust
let setpoint = propar_catalog::by_dde(9).unwrap().parameter;
assert_eq!(setpoint, propar_core::message::Parameter::new(
    1, 1, propar_core::message::DataType::U16,
));
```

Numeric lookup includes 425 FlowDDE definitions and alternate wire definitions. Parameter names, name search, and 437 enum and bitfield descriptions require `propar-catalog`'s `metadata` feature. With it disabled, definitions contain no name field and the descriptive strings are excluded from compilation. All catalog operations work without allocation.

The wire carries four storage types. Signed integers, Bronkhorst bipolar integers, and IEEE-754 floats are interpreted using the requested parameter's type. Bipolar measures use the documented range −23593 through 41942, with `0xffff` decoding to −1. This corrects an off-by-one in the Python 1.3.0 conversion for negative measures. Text and binary strings are returned as bytes to preserve embedded data, fixed-width padding, and non-UTF-8 content.

The `propar` client crate forwards the core and catalog features:

| `propar` Cargo features | Available interfaces |
| --- | --- |
| No default features | Allocation-free frames, messages and numeric FlowDDE catalog |
| `std` | Owned codec convenience types |
| `blocking` | Blocking master over a caller-supplied stream |
| `serial` (default) | Blocking master and serial port constructors |
| `async` | Async master over a Tokio stream |
| `async-serial` | Async master and serial port constructors |
| `embedded-async` | Allocation-free async master over embedded I/O and HAL traits |
| `catalog-metadata` (default) | Parameter names, name search and value descriptions |

Resolve by DDE or wire address with `catalog::by_dde` and `catalog::by_address`. With metadata enabled, search names with `catalog::search` and inspect enum and bitfield meanings with `catalog::values`. Setting `default-features = false` on `propar` excludes metadata; transport features can then be selected independently. Without metadata, discovery reports a numeric device-type code for legacy instruments that lack a device-type parameter.

## Development

Run the workspace tests, then check each embedded crate separately to avoid masking its minimal configuration through Cargo feature unification:

```sh
cargo test --workspace --all-features
cargo test -p propar-core --no-default-features
cargo test -p propar-catalog --no-default-features
cargo test -p propar-embedded --no-default-features
cargo test -p propar --no-default-features
```

With the `thumbv7em-none-eabihf` target installed, check the embedded builds:

```sh
cargo check -p propar-core -p propar-catalog -p propar-embedded --no-default-features --target thumbv7em-none-eabihf
cargo check -p propar --no-default-features --features embedded-async --target thumbv7em-none-eabihf
```

The upstream implementation and parameter data are [MIT licensed](https://github.com/bronkhorst-developer/bronkhorst-propar/blob/ebda5ba3cf3f22b02cfd58666783734cd5fd0ae0/LICENSE).
