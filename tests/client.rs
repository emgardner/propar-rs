#![cfg(any(feature = "blocking", feature = "async"))]

use propar::{Error, frame::Mode, message::Value};

// DDE 205 (fMeasure), 206 (fSetpoint), 129 (capacity unit). Generated using
// the upstream 1.3.0 _propar_builder, independently of the Rust codec.
const CHAIN_REQUEST: &[u8] = b":0E8004A1C02140432143017F017F00\r\n";
const CHAIN_REPLY: &[u8] = b":158002A1C03FC000004340880000017F006B672F6800\r\n";

fn chain_values() -> [Value; 3] {
    [
        Value::F32(1.5),
        Value::F32(4.25),
        Value::Bytes(b"kg/h".to_vec()),
    ]
}

#[cfg(feature = "blocking")]
mod blocking {
    use super::*;
    use propar::{blocking::Master, message::SendMode};
    use std::io::{self, Read, Write};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    #[derive(Default)]
    struct Wire<'a> {
        incoming: &'a [u8],
        written: Vec<u8>,
    }

    impl Read for Wire<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let len = buf.len().min(3);
            self.incoming.read(&mut buf[..len])
        }
    }

    impl Write for Wire<'_> {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let len = buf.len().min(2);
            self.written.extend_from_slice(&buf[..len]);
            Ok(len)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn master(incoming: &[u8], mode: Mode) -> Master<Wire<'_>> {
        Master::new(
            Wire {
                incoming,
                ..Wire::default()
            },
            mode,
        )
    }

    #[test]
    fn mixed_chain_matches_the_python_driver() {
        let mut master = master(CHAIN_REPLY, Mode::Ascii);
        assert_eq!(
            master.local().read_many(&[205, 206, 129]).unwrap(),
            chain_values()
        );
        assert_eq!(master.into_inner().written, CHAIN_REQUEST);
    }

    #[test]
    fn channel_override_changes_sensor_process_but_keeps_unknown_dde_off_the_wire() {
        let mut master = master(b":080202224040F00000\r\n", Mode::Ascii);
        assert!(matches!(
            master.instrument(2, 0),
            Err(Error::InvalidChannel(0))
        ));
        let mut instrument = master.instrument(2, 2).unwrap();
        assert!(matches!(
            instrument.read(u16::MAX),
            Err(Error::UnknownParameter(u16::MAX))
        ));
        assert_eq!(instrument.read(205).unwrap(), Value::F32(7.5));
        assert_eq!(master.into_inner().written, b":06020422402240\r\n");
    }

    #[test]
    fn broadcast_and_stale_frames_do_not_consume_the_reply() {
        let bytes = [
            // Retired sequence number, then a frame for another node.
            &b"\x10\x02\x00\x03\x05\x02\x01\x21\x00\x00\x10\x03"[..],
            b"\x10\x02\x01\x04\x05\x02\x01\x21\x00\x00\x10\x03",
            // Device 4 broadcasts fMeasure = 1.5 using our current sequence.
            b"\x10\x02\x01\x04\x07\x03\x21\x40\x3f\xc0\x00\x00\x10\x03",
            // Manual 9.17.027 section 3.10.2, setpoint = full scale.
            b"\x10\x02\x01\x03\x05\x02\x01\x21\x7d\x00\x10\x03",
        ]
        .concat();
        let mut master = master(&bytes, Mode::Binary);
        let delivered = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&delivered);
        master.on_broadcast(move |msg| {
            assert_eq!(msg.address, 4);
            assert_eq!(msg.parameters.len(), 1);
            assert_eq!(msg.parameters[0].value, Value::F32(1.5));
            observed.store(true, Ordering::Relaxed);
        });
        assert_eq!(master.instrument(3, 1).unwrap().setpoint().unwrap(), 32000);
        assert!(delivered.load(Ordering::Relaxed));
        assert_eq!(
            master.into_inner().written,
            b"\x10\x02\x01\x03\x05\x04\x01\x21\x01\x21\x10\x03"
        );
    }

    #[test]
    fn acknowledged_errors_do_not_block_a_subsequent_no_ack_write() {
        let mut master = master(b":0480000D03\r\n:0109\r\n", Mode::Ascii);
        assert!(matches!(
            master.local().set_setpoint(16000),
            Err(Error::Status {
                code: 13,
                position: 3
            })
        ));
        assert!(matches!(master.local().measure(), Err(Error::Interface(9))));
        master
            .local()
            .write_many(&[(9, Value::U16(16))], SendMode::NoAck)
            .unwrap();
        assert_eq!(
            master.into_inner().written,
            b":06800101213E80\r\n:06800401200120\r\n:06800201210010\r\n"
        );
    }

    #[test]
    fn discovery_stops_at_a_self_link_and_extracts_identification() {
        let bytes = b":108002008103E000414243313233000303\r\n:108002716100496E737472756D656E7400\r\n:058002001202\r\n";
        let mut master = master(bytes, Mode::Ascii);
        let nodes = master.get_nodes(false).unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].address, 3);
        assert_eq!(nodes[0].identification, b"ABC123");
        assert_eq!(nodes[0].serial_number(), b"123");
        assert_eq!(nodes[0].device_type, "Instrument");
        assert_eq!(nodes[0].channels, 2);
        assert_eq!(
            master.into_inner().written,
            b":0D800400810001E0006000030003\r\n:0780047161716100\r\n:06800400120012\r\n"
        );
    }
}

#[cfg(feature = "async")]
mod asynchronous {
    use super::*;
    use propar::asynchronous::Master;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

    #[tokio::test]
    async fn mixed_chain_crosses_a_fragmented_duplex_stream() {
        let (stream, mut peer) = duplex(4);
        let mut master = Master::new(stream, Mode::Ascii);
        let exchange = async {
            let mut request = vec![0; CHAIN_REQUEST.len()];
            peer.read_exact(&mut request).await.unwrap();
            assert_eq!(request, CHAIN_REQUEST);
            peer.write_all(CHAIN_REPLY).await.unwrap();
        };
        let mut instrument = master.local();
        let (values, ()) = tokio::join!(instrument.read_many(&[205, 206, 129]), exchange);
        assert_eq!(values.unwrap(), chain_values());
    }

    #[tokio::test]
    async fn binary_cancellation_skips_the_late_reply() {
        let (stream, mut peer) = duplex(128);
        let mut master = Master::new(stream, Mode::Binary);
        let mut instrument = master.local();
        let mut request = [0; 12];
        // Observe the fully transmitted request, then drop its pending future.
        tokio::select! {
            result = instrument.setpoint() => panic!("unexpected reply {result:?}"),
            result = peer.read_exact(&mut request) => result.unwrap(),
        };
        assert_eq!(
            &request,
            b"\x10\x02\x01\x80\x05\x04\x01\x21\x01\x21\x10\x03"
        );
        peer.write_all(b"\x10\x02\x01\x80\x05\x02\x01\x21\x00\x01\x10\x03\x10\x02\x02\x80\x05\x02\x01\x21\x00\x02\x10\x03").await.unwrap();
        assert_eq!(instrument.setpoint().await.unwrap(), 2);
        peer.read_exact(&mut request).await.unwrap();
        assert_eq!(
            &request,
            b"\x10\x02\x02\x80\x05\x04\x01\x21\x01\x21\x10\x03"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn ascii_timeout_keeps_the_next_request_off_the_wire() {
        let (stream, mut peer) = duplex(128);
        let mut master = Master::new(stream, Mode::Ascii);
        assert!(matches!(
            master.local().measure().await,
            Err(Error::Timeout)
        ));
        assert!(matches!(
            master.local().setpoint().await,
            Err(Error::Desynchronized)
        ));
        drop(master);
        let mut sent = Vec::new();
        peer.read_to_end(&mut sent).await.unwrap();
        assert_eq!(sent, b":06800401200120\r\n");
    }

    #[tokio::test]
    async fn cancelled_partial_write_cannot_be_retried_on_the_same_master() {
        let (stream, mut peer) = duplex(1);
        let mut master = Master::new(stream, Mode::Binary);
        let mut instrument = master.local();
        // One byte fills this pipe. Cancelling now leaves an incomplete outbound frame.
        tokio::select! {
            result = instrument.set_setpoint(32000) => panic!("unexpected completion {result:?}"),
            byte = peer.read_u8() => assert_eq!(byte.unwrap(), 0x10),
        }
        assert!(matches!(
            instrument.setpoint().await,
            Err(Error::Desynchronized)
        ));
    }
}
