use core::future::{Future, pending, poll_fn};
use core::pin::pin;
use core::task::Poll;
use std::time::Duration;

use embedded_hal_async::delay::DelayNs;
use embedded_io_async::{ErrorKind, ErrorType, Read, Write};
use propar_embedded::frame::{FrameRef, MAX_FRAME_LEN, Mode, PacketRef};
use propar_embedded::message::{
    DataType, MAX_PAYLOAD_LEN, MessageError, Parameter, ParameterValueRef, Request, SendMode,
    ValueRef,
};
use propar_embedded::{Error, LOCAL_NODE, Master};

const SETPOINT: Parameter = Parameter::new(1, 1, DataType::U16);

// Golden mixed-chain messages from the upstream Python 1.3.0 driver.
const CHAIN_REQUEST: &[u8] = b":0E8004A1C02140432143017F017F00\r\n";
const CHAIN_REPLY: &[u8] = b":158002A1C03FC000004340880000017F006B672F6800\r\n";

#[derive(Default)]
struct Wire {
    incoming: Vec<u8>,
    position: usize,
    written: Vec<u8>,
    reads: usize,
    flushes: usize,
    pending_when_empty: bool,
    write_limit: Option<usize>,
    write_delay_ms: u64,
    read_error: Option<ErrorKind>,
    zero_write: bool,
    pending_flush: bool,
    noise_reads: Option<usize>,
}

impl Wire {
    fn with_input(input: &[u8]) -> Self {
        Self {
            incoming: input.to_vec(),
            ..Self::default()
        }
    }
}

impl ErrorType for Wire {
    type Error = ErrorKind;
}

impl Read for Wire {
    async fn read(&mut self, output: &mut [u8]) -> Result<usize, Self::Error> {
        self.reads += 1;
        if let Some(error) = self.read_error {
            return Err(error);
        }
        if let Some(remaining) = &mut self.noise_reads {
            if *remaining == 0 {
                return Err(ErrorKind::InvalidData);
            }
            *remaining -= 1;
            output.fill(0x55);
            return Ok(output.len());
        }
        if self.position == self.incoming.len() && self.pending_when_empty {
            return pending().await;
        }
        let len = output.len().min(3).min(self.incoming.len() - self.position);
        output[..len].copy_from_slice(&self.incoming[self.position..self.position + len]);
        self.position += len;
        Ok(len)
    }
}

impl Write for Wire {
    async fn write(&mut self, input: &[u8]) -> Result<usize, Self::Error> {
        if self.zero_write {
            return Ok(0);
        }
        if self.write_delay_ms != 0 {
            tokio::time::sleep(Duration::from_millis(self.write_delay_ms)).await;
        }
        let remaining = self
            .write_limit
            .map(|limit| limit.saturating_sub(self.written.len()))
            .unwrap_or(usize::MAX);
        if remaining == 0 {
            return pending().await;
        }
        let len = input.len().min(2).min(remaining);
        self.written.extend_from_slice(&input[..len]);
        Ok(len)
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        self.flushes += 1;
        if self.pending_flush {
            return pending().await;
        }
        Ok(())
    }
}

struct Timer;

impl DelayNs for Timer {
    async fn delay_ns(&mut self, ns: u32) {
        tokio::time::sleep(Duration::from_nanos(u64::from(ns))).await;
    }
}

struct PollTimer {
    remaining_polls: usize,
}

impl DelayNs for PollTimer {
    async fn delay_ns(&mut self, _ns: u32) {
        poll_fn(|cx| {
            self.remaining_polls -= 1;
            if self.remaining_polls == 0 {
                Poll::Ready(())
            } else {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
        .await;
    }
}

fn master(input: &[u8], mode: Mode) -> Master<Wire, Timer> {
    Master::new(Wire::with_input(input), Timer, mode)
}

fn packet(mode: Mode, payload: &[u8]) -> Vec<u8> {
    let mut output = [0; MAX_FRAME_LEN];
    let len = FrameRef {
        sequence: 1,
        address: LOCAL_NODE,
        payload,
    }
    .encode_into(mode, &mut output)
    .unwrap();
    output[..len].to_vec()
}

#[tokio::test]
async fn fragmented_ascii_chain_matches_the_python_driver_and_borrows_strings() {
    let mut master = master(CHAIN_REPLY, Mode::Ascii);
    let parameters = [
        Parameter::new(33, 0, DataType::F32),
        Parameter::new(33, 3, DataType::F32),
        Parameter::new(1, 31, DataType::String(0)),
    ];
    let requests = parameters.map(Request::new);
    let mut response = [0; MAX_PAYLOAD_LEN];
    let storage_start = response.as_ptr() as usize;
    let storage_end = storage_start + response.len();
    let values: Vec<_> = master
        .read_many(LOCAL_NODE, &requests, &mut response)
        .await
        .unwrap()
        .zip(parameters)
        .map(|(entry, parameter)| entry.unwrap().into_value_for(parameter).unwrap())
        .collect();
    assert_eq!(
        values,
        [
            ValueRef::F32(1.5),
            ValueRef::F32(4.25),
            ValueRef::Bytes(b"kg/h")
        ]
    );
    let ValueRef::Bytes(bytes) = values[2] else {
        panic!("expected a borrowed unit string");
    };
    assert!((storage_start..storage_end).contains(&(bytes.as_ptr() as usize)));
    let (wire, _) = master.into_inner();
    assert_eq!(wire.written, CHAIN_REQUEST);
    assert_eq!(wire.flushes, 1);
}

#[tokio::test]
async fn binary_read_ignores_stale_frames_other_nodes_and_broadcasts() {
    let incoming = [
        &b"\x10\x02\x00\x03\x05\x02\x01\x21\x00\x00\x10\x03"[..],
        b"\x10\x02\x01\x04\x05\x02\x01\x21\x00\x00\x10\x03",
        b"\x10\x02\x01\x03\x07\x03\x21\x40\x3f\xc0\x00\x00\x10\x03",
        b"\x10\x02\x01\x03\x05\x02\x01\x21\x7d\x00\x10\x03",
    ]
    .concat();
    let mut master = master(&incoming, Mode::Binary);
    let mut response = [0; MAX_PAYLOAD_LEN];
    assert_eq!(
        master.read(3, SETPOINT, &mut response).await.unwrap(),
        ValueRef::U16(32000)
    );
    let (wire, _) = master.into_inner();
    assert_eq!(
        wire.written,
        b"\x10\x02\x01\x03\x05\x04\x01\x21\x01\x21\x10\x03"
    );
}

#[tokio::test]
async fn binary_interface_errors_must_match_both_sequence_and_address() {
    let mut incoming = Vec::new();
    for (sequence, address, code) in [(0, LOCAL_NODE, 3), (1, 7, 4), (1, LOCAL_NODE, 9)] {
        let mut output = [0; MAX_FRAME_LEN];
        let len = PacketRef::BinaryError {
            sequence,
            address,
            code,
        }
        .encode_into(Mode::Binary, &mut output)
        .unwrap();
        incoming.extend_from_slice(&output[..len]);
    }
    let mut master = master(&incoming, Mode::Binary);
    let mut response = [0; MAX_PAYLOAD_LEN];
    assert!(matches!(
        master.read(LOCAL_NODE, SETPOINT, &mut response).await,
        Err(Error::Interface(9))
    ));
    // A complete matching negative response leaves the transport usable.
    master
        .write_many(
            LOCAL_NODE,
            &[ParameterValueRef::new(SETPOINT, ValueRef::U16(0))],
            SendMode::NoAck,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn single_read_applies_requested_numeric_interpretation() {
    for mode in [Mode::Ascii, Mode::Binary] {
        for (parameter, payload, expected) in [
            (
                Parameter::new(1, 0, DataType::Bht),
                &[2, 1, 0x20, 0xff, 0xff][..],
                ValueRef::Bht(-1),
            ),
            (
                Parameter::new(33, 0, DataType::F32),
                &[2, 33, 0x40, 0x3f, 0xc0, 0, 0][..],
                ValueRef::F32(1.5),
            ),
        ] {
            let mut master = master(&packet(mode, payload), mode);
            let mut response = [0; MAX_PAYLOAD_LEN];
            assert_eq!(
                master
                    .read(LOCAL_NODE, parameter, &mut response)
                    .await
                    .unwrap(),
                expected
            );
        }
    }
}

#[tokio::test]
async fn read_many_honors_distinct_response_addresses() {
    let mut master = master(b":0680020220007B\r\n", Mode::Ascii);
    let request = Request {
        parameter: SETPOINT,
        response_process: 2,
        response_number: 0,
    };
    let mut response = [0; MAX_PAYLOAD_LEN];
    let mut values = master
        .read_many(LOCAL_NODE, &[request], &mut response)
        .await
        .unwrap();
    assert_eq!(
        values
            .next()
            .unwrap()
            .unwrap()
            .into_value_for(request.response_parameter())
            .unwrap(),
        ValueRef::U16(123)
    );
    assert!(values.next().is_none());
    assert_eq!(master.into_inner().0.written, b":06800402200121\r\n");
}

#[tokio::test]
async fn rejects_wrong_parameter_type_count_and_malformed_payload() {
    let requests = [Request::new(SETPOINT)];
    let mut response = [0; MAX_PAYLOAD_LEN];
    let mut wrong_address = master(b":0680020122007B\r\n", Mode::Ascii);
    assert!(matches!(
        wrong_address
            .read_many(LOCAL_NODE, &requests, &mut response)
            .await,
        Err(Error::Message(MessageError::UnexpectedParameter { .. }))
    ));
    let mut wrong_type = master(b":05800201017B\r\n", Mode::Ascii);
    assert!(matches!(
        wrong_type
            .read_many(LOCAL_NODE, &requests, &mut response)
            .await,
        Err(Error::Message(MessageError::TypeMismatch))
    ));
    let mut wrong_count = master(b":0680020121007B\r\n", Mode::Ascii);
    assert!(matches!(
        wrong_count
            .read_many(LOCAL_NODE, &[requests[0], requests[0]], &mut response)
            .await,
        Err(Error::UnexpectedReply)
    ));
    let reply = packet(Mode::Ascii, &[2, 1, 0xa1, 0, 1, 0x21, 0, 2]);
    let mut extra_values = master(&reply, Mode::Ascii);
    assert!(matches!(
        extra_values
            .read_many(LOCAL_NODE, &requests, &mut response)
            .await,
        Err(Error::UnexpectedReply)
    ));
    let mut malformed = master(b":05800201217B\r\n", Mode::Ascii);
    assert!(matches!(
        malformed
            .read_many(LOCAL_NODE, &requests, &mut response)
            .await,
        Err(Error::Message(MessageError::Truncated))
    ));
}

#[tokio::test]
async fn acknowledges_writes_and_preserves_status_and_interface_errors() {
    let mut master = master(b":0480000000\r\n:0480000D03\r\n:0109\r\n", Mode::Ascii);
    master
        .write(LOCAL_NODE, SETPOINT, ValueRef::U16(16000))
        .await
        .unwrap();
    assert!(matches!(
        master
            .write(LOCAL_NODE, SETPOINT, ValueRef::U16(16000))
            .await,
        Err(Error::Status {
            code: 13,
            position: 3
        })
    ));
    let mut response = [0; MAX_PAYLOAD_LEN];
    assert!(matches!(
        master.read(LOCAL_NODE, SETPOINT, &mut response).await,
        Err(Error::Interface(9))
    ));
    master
        .write_many(
            LOCAL_NODE,
            &[ParameterValueRef::new(SETPOINT, ValueRef::U16(16))],
            SendMode::NoAck,
        )
        .await
        .unwrap();
    assert_eq!(
        master.into_inner().0.written,
        b":06800101213E80\r\n:06800101213E80\r\n:06800401210121\r\n:06800201210010\r\n"
    );
}

#[tokio::test]
async fn unacknowledged_and_broadcast_writes_flush_without_reading() {
    for mode in [Mode::Ascii, Mode::Binary] {
        for send_mode in [SendMode::NoAck, SendMode::Broadcast] {
            let mut master = master(&[], mode);
            master
                .write_many(
                    LOCAL_NODE,
                    &[ParameterValueRef::new(SETPOINT, ValueRef::U16(16))],
                    send_mode,
                )
                .await
                .unwrap();
            let (wire, _) = master.into_inner();
            assert_eq!(wire.reads, 0);
            assert_eq!(wire.flushes, 1);
            let mut payload = [0; MAX_PAYLOAD_LEN];
            let PacketRef::Frame(frame) =
                PacketRef::decode_into(mode, &wire.written, &mut payload).unwrap()
            else {
                panic!("expected an addressed write");
            };
            assert_eq!(frame.payload, [send_mode as u8, 1, 0x21, 0, 16]);
        }
    }
}

#[tokio::test]
async fn validation_errors_do_not_send_any_bytes_or_desynchronize() {
    let mut master = master(b":0680020121007B\r\n", Mode::Ascii);
    let mut response = [0; MAX_PAYLOAD_LEN];
    assert!(matches!(
        master.read_many(LOCAL_NODE, &[], &mut response).await,
        Err(Error::Message(MessageError::EmptyParameters))
    ));
    assert!(matches!(
        master.write(LOCAL_NODE, SETPOINT, ValueRef::U8(1)).await,
        Err(Error::Message(MessageError::TypeMismatch))
    ));
    assert!(master.get_mut().written.is_empty());
    assert_eq!(
        master
            .read(LOCAL_NODE, SETPOINT, &mut response)
            .await
            .unwrap(),
        ValueRef::U16(123)
    );
}

#[tokio::test(start_paused = true)]
async fn timeout_while_waiting_for_reply_blocks_reuse_in_either_framing_mode() {
    for mode in [Mode::Ascii, Mode::Binary] {
        let wire = Wire {
            pending_when_empty: true,
            ..Wire::default()
        };
        let mut master = Master::new(wire, Timer, mode);
        master.set_timeout_ms(5);
        let mut response = [0; MAX_PAYLOAD_LEN];
        assert!(matches!(
            master.read(LOCAL_NODE, SETPOINT, &mut response).await,
            Err(Error::Timeout)
        ));
        let written = master.get_mut().written.len();
        assert!(written > 0);
        assert!(matches!(
            master.read(LOCAL_NODE, SETPOINT, &mut response).await,
            Err(Error::Desynchronized)
        ));
        assert_eq!(master.get_mut().written.len(), written);
    }
}

#[tokio::test(start_paused = true)]
async fn timeout_covers_the_whole_write_instead_of_restarting_for_each_fragment() {
    let wire = Wire {
        write_delay_ms: 2,
        ..Wire::default()
    };
    let mut master = Master::new(wire, Timer, Mode::Ascii);
    master.set_timeout_ms(5);
    assert!(matches!(
        master
            .write(LOCAL_NODE, SETPOINT, ValueRef::U16(16000))
            .await,
        Err(Error::Timeout)
    ));
    assert!(!master.get_mut().written.is_empty());
    assert!(master.get_mut().written.len() < b":06800101213E80\r\n".len());
    assert_eq!(master.get_mut().reads, 0);
}

#[tokio::test]
async fn continuously_ready_noise_does_not_starve_the_timeout() {
    let wire = Wire {
        noise_reads: Some(10),
        ..Wire::default()
    };
    let timer = PollTimer { remaining_polls: 4 };
    let mut master = Master::new(wire, timer, Mode::Binary);
    let mut response = [0; MAX_PAYLOAD_LEN];
    assert!(matches!(
        master.read(LOCAL_NODE, SETPOINT, &mut response).await,
        Err(Error::Timeout)
    ));
    assert!((1..10).contains(&master.get_mut().reads));
}

#[tokio::test]
async fn zero_timeout_leaves_the_transport_untouched_and_reusable() {
    let mut master = master(b":0680020121007B\r\n", Mode::Ascii);
    let mut response = [0; MAX_PAYLOAD_LEN];
    master.set_timeout_ms(0);
    assert!(matches!(
        master.read(LOCAL_NODE, SETPOINT, &mut response).await,
        Err(Error::Timeout)
    ));
    assert!(master.get_mut().written.is_empty());
    assert_eq!(master.get_mut().reads, 0);
    assert_eq!(master.get_mut().flushes, 0);
    master.set_timeout_ms(500);
    assert_eq!(
        master
            .read(LOCAL_NODE, SETPOINT, &mut response)
            .await
            .unwrap(),
        ValueRef::U16(123)
    );
}

#[tokio::test]
async fn cancelled_read_partial_write_and_flush_block_reuse() {
    for mode in [Mode::Ascii, Mode::Binary] {
        for interruption in 0..3 {
            let wire = Wire {
                pending_when_empty: true,
                write_limit: (interruption == 1).then_some(1),
                pending_flush: interruption == 2,
                ..Wire::default()
            };
            let mut master = Master::new(wire, Timer, mode);
            let mut response = [0; MAX_PAYLOAD_LEN];
            {
                let mut operation = pin!(master.read(LOCAL_NODE, SETPOINT, &mut response));
                poll_fn(|cx| {
                    assert!(operation.as_mut().poll(cx).is_pending());
                    Poll::Ready(())
                })
                .await;
            }
            let written = master.get_mut().written.len();
            assert!(written > 0);
            if interruption == 1 {
                assert_eq!(written, 1);
            }
            assert!(matches!(
                master.read(LOCAL_NODE, SETPOINT, &mut response).await,
                Err(Error::Desynchronized)
            ));
            assert_eq!(master.get_mut().written.len(), written);
        }
    }
}

#[tokio::test]
async fn reports_transport_eof_error_and_zero_write() {
    let mut response = [0; MAX_PAYLOAD_LEN];
    let mut eof = master(&[], Mode::Ascii);
    assert!(matches!(
        eof.read(LOCAL_NODE, SETPOINT, &mut response).await,
        Err(Error::UnexpectedEof)
    ));
    let wire = Wire {
        read_error: Some(ErrorKind::Other),
        ..Wire::default()
    };
    let mut failed = Master::new(wire, Timer, Mode::Ascii);
    assert!(matches!(
        failed.read(LOCAL_NODE, SETPOINT, &mut response).await,
        Err(Error::Io(ErrorKind::Other))
    ));
    let wire = Wire {
        zero_write: true,
        ..Wire::default()
    };
    let mut stalled = Master::new(wire, Timer, Mode::Ascii);
    assert!(matches!(
        stalled.read(LOCAL_NODE, SETPOINT, &mut response).await,
        Err(Error::WriteZero)
    ));
}
