# propar-embedded

An allocation-free `no_std` ProPar master for embedded async serial streams. It uses the frame and message codecs from `propar-core` without pulling in the parameter catalog, `std`, an allocator, or a runtime.

The stream implements `embedded-io-async` 0.7 `Read + Write`. Transaction timeouts use `embedded-hal-async` 1.0 `DelayNs`. The [HAL documentation](https://docs.rs/embedded-hal-async/1.0.0/embedded_hal_async/#serialuart-traits) directs serial implementations to these embedded I/O traits.

Configure your UART for the instrument's baud rate and serial settings before constructing the master. The factory default is 38400 baud, 8N1. Supply a delay implementation compatible with your executor, and choose binary or ASCII framing.

```rust,no_run
use embedded_hal_async::delay::DelayNs;
use embedded_io_async::{Read, Write};
use propar_embedded::{
    Error, Master, LOCAL_NODE,
    frame::Mode,
    message::{DataType, Parameter, ValueRef, MAX_PAYLOAD_LEN},
};

async fn update_setpoint<S, D>(stream: S, delay: D) -> Result<(), Error<S::Error>>
where
    S: Read + Write,
    D: DelayNs,
{
    let mut bus = Master::new(stream, delay, Mode::Binary);
    bus.set_timeout_ms(500);

    let setpoint = Parameter::new(1, 1, DataType::U16);
    let mut reply = [0; MAX_PAYLOAD_LEN];
    let current = bus.read(LOCAL_NODE, setpoint, &mut reply).await?;
    assert!(matches!(current, ValueRef::U16(_)));

    // 16000 corresponds to 50% of full scale. Wait for acknowledgement.
    bus.write(LOCAL_NODE, setpoint, ValueRef::U16(16_000)).await?;
    Ok(())
}
```

`read` returns a value interpreted using the requested parameter's type. Strings and binary values borrow the reply buffer. A buffer of `MAX_PAYLOAD_LEN` bytes accommodates any legal response payload. The master uses fixed internal storage for framing; no heap allocation is needed.

`read_many` accepts a slice of `Request` entries, checks the response count, addresses and types, and returns a borrowed `Parameters` iterator in request order. Its entries retain the unsigned wire representation: call `entry.into_value_for(request.response_parameter())` to obtain signed integers, bipolar measures, or floats. Using the response parameter also handles requests with a distinct return address. `write_many` accepts `ParameterValueRef` entries and a `SendMode` for acknowledged, unacknowledged, or broadcast writes.

The default timeout is 500 ms and covers the complete transaction, including writing and flushing the request. Set it with `set_timeout_ms`. Timing precision depends on the supplied delay and executor. The delay implementation must allow its pending future to be dropped and then reused: a successful exchange cancels its timer.

Cancellation, timeout, or an I/O failure after a transaction starts leaves the master desynchronized in both binary and ASCII modes. Subsequent operations return `Error::Desynchronized`. Use `into_inner()` to recover `(stream, delay)`, externally resynchronize the transport, and construct a new master. A timeout does not determine whether an instrument applied a write. `get_mut()` provides access to the stream for configuration; direct I/O bypasses the master's transaction tracking.

Replies from other addresses, stale binary sequence numbers, and unsolicited broadcasts are skipped while awaiting the requested reply. The embedded client provides raw parameter operations; it does not include discovery, broadcast callbacks, or the desktop `Instrument` wrapper. Add `propar-catalog` separately if numeric FlowDDE lookup is useful, leaving its `metadata` feature disabled to exclude descriptive strings.
