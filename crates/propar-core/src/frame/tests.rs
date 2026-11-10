use super::*;

const READ: FrameRef<'static> = FrameRef {
    sequence: 1,
    address: 0x80,
    payload: &[4, 1, 0x21, 1, 0x21],
};

#[test]
fn caller_buffers_encode_and_decode_golden_packets() {
    for (mode, expected) in [
        (
            Mode::Binary,
            &b"\x10\x02\x01\x80\x05\x04\x01\x21\x01\x21\x10\x03"[..],
        ),
        (Mode::Ascii, &b":06800401210121\r\n"[..]),
    ] {
        let mut wire = [0; MAX_FRAME_LEN];
        let len = READ.encode_into(mode, &mut wire).unwrap();
        assert_eq!(&wire[..len], expected);
        let mut payload = [0; MAX_PAYLOAD_LEN];
        let result = PacketRef::decode_into(mode, expected, &mut payload).unwrap();
        let sequence = if mode == Mode::Ascii {
            0
        } else {
            READ.sequence
        };
        assert_eq!(result, PacketRef::Frame(FrameRef { sequence, ..READ }));
    }
}

#[test]
fn capacity_errors_leave_caller_buffers_unchanged() {
    for mode in [Mode::Binary, Mode::Ascii] {
        let mut output = [0xaa; 8];
        assert_eq!(
            READ.encode_into(mode, &mut output),
            Err(FrameError::BufferTooSmall)
        );
        assert_eq!(output, [0xaa; 8]);
        let mut wire = [0; MAX_FRAME_LEN];
        let len = READ.encode_into(mode, &mut wire).unwrap();
        let mut payload = [0xaa; 4];
        assert_eq!(
            PacketRef::decode_into(mode, &wire[..len], &mut payload),
            Err(FrameError::BufferTooSmall)
        );
        assert_eq!(payload, [0xaa; 4]);
        wire[len] = 0;
        assert_eq!(
            PacketRef::decode_into(mode, &wire[..len + 1], &mut payload),
            Err(FrameError::TrailingData)
        );
        assert_eq!(payload, [0xaa; 4]);
    }
}

#[test]
fn every_legal_payload_length_fits_fixed_buffers() {
    let payload = [DLE; 256];
    for mode in [Mode::Binary, Mode::Ascii] {
        let max_len = if mode == Mode::Binary { 255 } else { 254 };
        for len in 1..=max_len {
            let frame = FrameRef {
                sequence: DLE,
                address: DLE,
                payload: &payload[..len],
            };
            let mut wire = [0; MAX_FRAME_LEN];
            let encoded = frame.encode_into(mode, &mut wire).unwrap();
            assert_eq!(frame.encoded_len(mode).unwrap(), encoded);
            let mut output = [0; MAX_PAYLOAD_LEN];
            let PacketRef::Frame(decoded) =
                PacketRef::decode_into(mode, &wire[..encoded], &mut output).unwrap()
            else {
                panic!("expected an addressed frame");
            };
            assert_eq!(decoded.payload, frame.payload);
            assert_eq!(decoded.address, frame.address);
            let mut decoder = Decoder::new(mode);
            for (index, &byte) in wire[..encoded].iter().enumerate() {
                let result = decoder.push(byte).unwrap();
                if index + 1 == encoded {
                    assert_eq!(result, Some(PacketRef::Frame(decoded)));
                } else {
                    assert_eq!(result, None);
                }
            }
        }
        let frame = FrameRef {
            payload: &payload[..max_len + 1],
            ..READ
        };
        let mut wire = [0xaa; MAX_FRAME_LEN];
        assert_eq!(frame.encode_into(mode, &mut wire), Err(FrameError::TooLong));
        assert_eq!(wire, [0xaa; MAX_FRAME_LEN]);
    }
}

#[test]
fn error_packets_work_without_payload_storage() {
    let error = PacketRef::BinaryError {
        sequence: DLE,
        address: 0x80,
        code: 9,
    };
    let wire = [DLE, STX, DLE, DLE, 0x80, 0, 9, DLE, ETX];
    let mut output = [0; 9];
    assert_eq!(
        error.encode_into(Mode::Binary, &mut output).unwrap(),
        wire.len()
    );
    assert_eq!(output, wire);
    assert_eq!(
        PacketRef::decode_into(Mode::Binary, &wire, &mut []),
        Ok(error)
    );
    let error = PacketRef::InterfaceError(9);
    assert_eq!(
        PacketRef::decode_into(Mode::Ascii, b":0109\r\n", &mut []),
        Ok(error)
    );
    assert_eq!(
        error.encode_into(Mode::Binary, &mut output),
        Err(FrameError::WrongMode)
    );
}

#[test]
fn incremental_errors_recover_at_the_next_start() {
    let mut decoder = Decoder::new(Mode::Binary);
    for byte in [DLE, STX] {
        assert_eq!(decoder.push(byte), Ok(None));
    }
    for _ in 0..MAX_BODY_LEN {
        assert_eq!(decoder.push(0xff), Ok(None));
    }
    assert_eq!(decoder.push(0xff), Err(FrameError::TooLong));
    let wire = [DLE, STX, 1, 0x80, 1, 9, DLE, ETX];
    let mut completed = 0;
    for byte in wire {
        if let Some(packet) = decoder.push(byte).unwrap() {
            completed += 1;
            assert_eq!(
                packet,
                PacketRef::Frame(FrameRef {
                    sequence: 1,
                    address: 0x80,
                    payload: &[9]
                })
            );
        }
    }
    assert_eq!(completed, 1);
    // Completion resets parser state while lending its payload. The next frame reuses storage.
    let mut decoder = Decoder::new(Mode::Ascii);
    let mut completed = 0;
    for _ in 0..2 {
        for &byte in b":0109\r\n" {
            if let Some(packet) = decoder.push(byte).unwrap() {
                completed += 1;
                assert_eq!(packet, PacketRef::InterfaceError(9));
            }
        }
    }
    assert_eq!(completed, 2);
}

#[test]
fn malformed_input_is_strict_and_does_not_modify_output() {
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
        let mut output = [0xaa; MAX_PAYLOAD_LEN];
        assert!(PacketRef::decode_into(Mode::Ascii, ascii, &mut output).is_err());
        assert_eq!(output, [0xaa; MAX_PAYLOAD_LEN]);
    }
    assert_eq!(
        PacketRef::decode_into(Mode::Binary, &[DLE, STX, DLE, 7], &mut []),
        Err(FrameError::InvalidEscape)
    );
    assert_eq!(
        PacketRef::decode_into(Mode::Binary, &[], &mut []),
        Err(FrameError::Incomplete)
    );
}
