//! FLOW-BUS network enumeration shared by the two master interfaces.

#[cfg(feature = "catalog-metadata")]
use crate::catalog;
use crate::message::{DataType, Parameter, Value};
use crate::{Error, LOCAL_NODE, Node};
use std::time::Duration;

const NODE_ADDRESS: Parameter = Parameter::new(0, 1, DataType::U8);
const ID: Parameter = Parameter::new(0, 0, DataType::String(0));
const NEXT_NODE: Parameter = Parameter::new(0, 3, DataType::U8);
const DEVICE_TYPE: Parameter = Parameter::new(113, 1, DataType::String(0));
const CHANNEL_COUNT: Parameter = Parameter::new(0, 18, DataType::U8);
const PROBE_TIMEOUT: Duration = Duration::from_millis(50);

fn byte(value: Value) -> Result<u8, Error> {
    match value {
        Value::U8(n) => Ok(n),
        _ => Err(Error::UnexpectedReply(
            "expected a byte during node discovery",
        )),
    }
}

fn legacy_device_type(id: &[u8]) -> String {
    let Some(&code) = id.first() else {
        return String::new();
    };
    #[cfg(feature = "catalog-metadata")]
    if let Some(entry) = catalog::values(175).find(|entry| entry.value == code) {
        return entry
            .description
            .split(':')
            .next()
            .unwrap_or_default()
            .to_owned();
    }
    code.to_string()
}

#[maybe_async_cfg::maybe(
    idents(implementation(mod, sync = "blocking", async = "asynchronous")),
    sync(feature = "blocking"),
    async(feature = "async")
)]
mod implementation {
    use super::*;
    use crate::frame::Mode;
    #[maybe_async_cfg::only_if(async)]
    use crate::{async_io::Stream, asynchronous::Master};
    #[maybe_async_cfg::only_if(sync)]
    use crate::{blocking::Master, blocking_io::Stream};

    impl<S: Stream> Master<S> {
        /// Follow the FLOW-BUS next-node links, stopping at zero or a previously seen address.
        ///
        /// With `find_first` set, probe addresses below the local instrument's
        /// network address with 50 ms response deadlines. This includes nodes
        /// earlier in the chain than the local instrument. With it clear,
        /// traversal starts directly at the local instrument (0x80).
        ///
        /// Older instruments that reject a chained identification request are
        /// queried one parameter at a time. If the device type or channel
        /// count is unsupported, use the ID prefix or a single channel. The ID
        /// prefix is translated to a device name with `catalog-metadata`, or
        /// returned as a decimal device-type code without it. An
        /// ASCII probe that times out without a complete response ends the scan
        /// because a late reply could otherwise be attributed to a later request.
        pub async fn get_nodes(&mut self, find_first: bool) -> Result<Vec<Node>, Error> {
            let mut target = LOCAL_NODE;
            if find_first {
                let local = byte(self.read(LOCAL_NODE, NODE_ADDRESS).await?)?;
                if (1..=LOCAL_NODE).contains(&local) {
                    target = local;
                }
                for candidate in 1..target {
                    match self
                        .read_with_timeout(candidate, &[NODE_ADDRESS], PROBE_TIMEOUT)
                        .await
                    {
                        Ok(_) => {
                            target = candidate;
                            break;
                        }
                        // A fully received negative response leaves either mode in sync.
                        Err(Error::Interface(5 | 9) | Error::Status { .. }) => continue,
                        // Only binary replies carry sequence numbers. In ASCII a
                        // late reply from a timed-out probe cannot be correlated.
                        Err(Error::Timeout) if self.mode == Mode::Binary => continue,
                        Err(e) => return Err(e),
                    }
                }
            }

            let mut nodes = Vec::new();
            let mut seen = [false; 256];
            while target != 0 && !seen[usize::from(target)] {
                let params = [NODE_ADDRESS, ID, NEXT_NODE];
                let info = match self.read_many(target, &params).await {
                    Ok(values) => values,
                    Err(Error::Status { .. } | Error::Interface(3 | 4)) => {
                        let mut values = Vec::with_capacity(params.len());
                        for parameter in params {
                            values.push(self.read(target, parameter).await?);
                        }
                        values
                    }
                    Err(e) => return Err(e),
                };
                let [
                    Value::U8(address),
                    Value::Bytes(identification),
                    Value::U8(next),
                ] = <[Value; 3]>::try_from(info)
                    .map_err(|_| Error::UnexpectedReply("expected three node parameters"))?
                else {
                    return Err(Error::UnexpectedReply("unexpected node parameter types"));
                };

                if seen[usize::from(address)] {
                    break;
                }
                // The local alias 0x80 and the actual network address can designate one device.
                seen[usize::from(target)] = true;
                seen[usize::from(address)] = true;
                let device_type = match self.read(target, DEVICE_TYPE).await {
                    Ok(Value::Bytes(name)) => String::from_utf8_lossy(&name).into_owned(),
                    Ok(_) => return Err(Error::UnexpectedReply("expected device type string")),
                    Err(Error::Status { .. }) => legacy_device_type(&identification),
                    Err(e) => return Err(e),
                };
                let channels = match self.read(target, CHANNEL_COUNT).await {
                    Ok(value) => byte(value)?,
                    Err(Error::Status { .. }) => 1,
                    Err(e) => return Err(e),
                };
                nodes.push(Node {
                    address,
                    identification,
                    device_type,
                    channels,
                });
                target = next;
            }
            Ok(nodes)
        }
    }
}

#[cfg(all(test, feature = "blocking"))]
mod tests {
    use super::*;
    use crate::blocking::Master;
    use crate::frame::{Decoder, Frame, Mode, Packet};
    use std::io::{self, Cursor, Read, Write};

    #[derive(Default)]
    struct Bus {
        input: Cursor<Vec<u8>>,
        output: Vec<u8>,
    }

    impl Read for Bus {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.input.read(buf)
        }
    }

    impl Write for Bus {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.output.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn probes_scattered_addresses_and_identifies_a_legacy_node() {
        let mut bus = Bus::default();
        // Each item is a response to one request. Node 3 rejects chaining and
        // has neither device type nor channel count, as on older firmware.
        let responses: &[(u8, &[u8])] = &[
            (0x80, &[2, 0, 1, 7]), // local instrument lives at address 7
            (1, &[9]),             // no response from node 1
            (2, &[0, 11, 0]),      // node address rejected
            (3, &[2, 0, 1, 3]),    // first live node
            (3, &[0, 3, 1]),       // chained identification rejected
            (3, &[2, 0, 1, 3]),
            (3, &[2, 0, 0x60, 0, b')', b':', b':', b'S', 0]),
            (3, &[2, 0, 3, 7]),
            (3, &[0, 3, 1]), // unknown device type parameter
            (3, &[0, 4, 2]), // unknown channel count parameter
            (7, &[2, 0, 0x81, 7, 0xe0, 0, b'Q', 0, 3, 3]),
            (7, &[2, 113, 0x61, 0, b'D', b'M', 0]),
            (7, &[2, 0, 18, 4]), // four channels
        ];
        for (n, &(address, payload)) in responses.iter().enumerate() {
            let frame = Frame {
                sequence: (n + 1) as u8,
                address,
                payload: payload.into(),
            };
            bus.input
                .get_mut()
                .extend(frame.encode(Mode::Binary).unwrap());
        }
        let mut master = Master::new(bus, Mode::Binary);
        let nodes = master.get_nodes(true).unwrap();
        assert_eq!(nodes.iter().map(|n| n.address).collect::<Vec<_>>(), [3, 7]);
        assert_eq!(nodes[0].serial_number(), b"S");
        let legacy_type = if cfg!(feature = "catalog-metadata") {
            "DC"
        } else {
            "41"
        };
        assert_eq!(
            (nodes[0].device_type.as_str(), nodes[0].channels),
            (legacy_type, 1)
        );
        assert_eq!(
            (nodes[1].device_type.as_str(), nodes[1].channels),
            ("DM", 4)
        );
        let mut decoder = Decoder::new(Mode::Binary);
        let requests: Vec<_> = master
            .into_inner()
            .output
            .into_iter()
            .filter_map(|b| decoder.push(b).unwrap().map(|packet| packet.to_owned()))
            .map(|p| match p {
                Packet::Frame(f) => f,
                _ => panic!("expected a request"),
            })
            .collect();
        assert_eq!(requests.len(), responses.len());
        assert_eq!(
            &requests[4].payload,
            &[4, 0, 0x81, 0, 1, 0xe0, 0, 0x60, 0, 3, 0, 3]
        );
    }

    #[test]
    fn propagates_io_failure_instead_of_reporting_an_empty_network() {
        let mut master = Master::new(Bus::default(), Mode::Binary);
        assert!(
            matches!(master.get_nodes(true), Err(Error::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof)
        );
    }
}
