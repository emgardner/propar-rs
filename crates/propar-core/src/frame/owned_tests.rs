use super::*;
use std::vec;

fn read_setpoint(sequence: u8) -> Frame {
    Frame {
        sequence,
        address: 0x80,
        payload: vec![4, 1, 0x21, 1, 0x21],
    }
}

#[test]
fn golden_setpoint_request_and_ack() {
    let frame = read_setpoint(1);
    let binary = [0x10, 2, 1, 0x80, 5, 4, 1, 0x21, 1, 0x21, 0x10, 3];
    assert_eq!(frame.encode(Mode::Binary).unwrap(), binary);
    assert_eq!(
        Packet::decode(Mode::Binary, &binary),
        Ok(Packet::Frame(frame.clone()))
    );
    assert_eq!(frame.encode(Mode::Ascii).unwrap(), b":06800401210121\r\n");
    assert_eq!(
        Packet::decode(Mode::Ascii, b":06800401210121\r\n"),
        Ok(Packet::Frame(read_setpoint(0)))
    );
    assert_eq!(
        Packet::decode(Mode::Ascii, b":0480000005\r\n"),
        Ok(Packet::Frame(Frame {
            sequence: 0,
            address: 0x80,
            payload: vec![0, 0, 5]
        }))
    );
}

#[test]
fn every_field_including_length_is_stuffed() {
    let frame = Frame {
        sequence: DLE,
        address: DLE,
        payload: vec![DLE; 16],
    };
    let mut expected = vec![DLE, STX];
    expected.extend([DLE; 38]); // 2 bytes per sequence, address, length and each payload byte
    expected.extend([DLE, ETX]);
    assert_eq!(frame.encode(Mode::Binary), Ok(expected.clone()));
    assert_eq!(
        Packet::decode(Mode::Binary, &expected),
        Ok(Packet::Frame(frame))
    );
}

#[test]
fn interface_errors_keep_their_mode_specific_headers() {
    let error = Packet::BinaryError {
        sequence: DLE,
        address: 0x80,
        code: 9,
    };
    let binary = [DLE, STX, DLE, DLE, 0x80, 0, 9, DLE, ETX];
    assert_eq!(Packet::decode(Mode::Binary, &binary), Ok(error.clone()));
    assert_eq!(error.encode(Mode::Binary).unwrap(), binary);
    let error = Packet::InterfaceError(9);
    assert_eq!(Packet::decode(Mode::Ascii, b":0109\r\n"), Ok(error.clone()));
    assert_eq!(error.encode(Mode::Ascii).unwrap(), b":0109\r\n");
}

#[test]
fn bounded_stream_recovers_without_losing_the_next_start() {
    for (mode, broken) in [(Mode::Binary, &[DLE, STX, 7][..]), (Mode::Ascii, b":0")] {
        let packet = Packet::Frame(read_setpoint(0));
        let wire = packet.encode(mode).unwrap();
        let input = [b"unrelated text", broken, wire.as_slice(), wire.as_slice()].concat();
        let mut decoder = Decoder::new(mode);
        let results: Vec<_> = input
            .into_iter()
            .filter_map(|b| {
                decoder
                    .push(b)
                    .map(|packet| packet.map(PacketRef::to_owned))
                    .transpose()
            })
            .collect();
        assert_eq!(
            results,
            [
                Err(FrameError::UnexpectedStart),
                Ok(packet.clone()),
                Ok(packet)
            ]
        );
    }

    let mut decoder = Decoder::new(Mode::Binary);
    for b in [DLE, STX] {
        assert_eq!(decoder.push(b), Ok(None));
    }
    for _ in 0..258 {
        assert_eq!(decoder.push(0xff), Ok(None));
    }
    assert_eq!(decoder.push(0xff), Err(FrameError::TooLong));
    let wire = read_setpoint(1).encode(Mode::Binary).unwrap();
    assert_eq!(
        wire.into_iter()
            .filter_map(|b| decoder
                .push(b)
                .map(|packet| packet.map(PacketRef::to_owned))
                .transpose())
            .collect::<Vec<_>>(),
        [Ok(Packet::Frame(read_setpoint(1)))]
    );
}

#[test]
fn lengths_and_hex_are_checked_strictly() {
    for ascii in [
        &b":02800200\r\n"[..],
        b":0280\r\n",
        b":02GF01\r\n",
        b":0280A\r\n",
        b":0280AA\n",
        b":0280AA\rX",
        b":0280AA\r",
        b":0280AA\r\nX",
        b" :0280AA\r\n",
    ] {
        assert!(Packet::decode(Mode::Ascii, ascii).is_err(), "{ascii:?}");
    }
    assert_eq!(
        Packet::decode(Mode::Ascii, b":0280af\r\n"),
        Ok(Packet::Frame(Frame {
            sequence: 0,
            address: 0x80,
            payload: vec![0xaf]
        }))
    );
    assert_eq!(
        Packet::decode(Mode::Binary, &[DLE, STX, 1, 2, 2, 6, DLE, ETX]),
        Err(FrameError::InvalidLength)
    );
    assert_eq!(
        Packet::decode(Mode::Binary, &[DLE, STX, DLE, 7]),
        Err(FrameError::InvalidEscape)
    );
}

#[test]
fn payload_boundaries() {
    for (mode, max_size) in [(Mode::Ascii, 254), (Mode::Binary, 255)] {
        let mut frame = Frame {
            sequence: 0,
            address: 3,
            payload: vec![0x10; max_size],
        };
        let encoded = frame.encode(mode).unwrap();
        assert_eq!(
            Packet::decode(mode, &encoded),
            Ok(Packet::Frame(frame.clone()))
        );
        frame.payload.push(0);
        assert_eq!(frame.encode(mode), Err(FrameError::TooLong));
    }
}
