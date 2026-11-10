use super::*;

#[test]
fn fixed_buffers_encode_and_decode_mixed_chains() {
    let entries = [
        ParameterValueRef::new(Parameter::new(97, 1, DataType::U16), ValueRef::U16(2000)),
        ParameterValueRef::new(Parameter::new(97, 3, DataType::U8), ValueRef::U8(1)),
        ParameterValueRef::new(Parameter::new(1, 1, DataType::U16), ValueRef::U16(7000)),
        ParameterValueRef::new(Parameter::new(33, 3, DataType::F32), ValueRef::F32(0.1)),
    ];
    let expected = [
        1, 225, 161, 7, 208, 3, 1, 129, 33, 27, 88, 33, 67, 61, 204, 204, 205,
    ];
    let mut output = [0; MAX_PAYLOAD_LEN];
    let msg = MessageRef::Send {
        mode: SendMode::WithAck,
        parameters: &entries,
    };
    assert_eq!(msg.encoded_len(), Ok(expected.len()));
    assert_eq!(msg.encode_into(&mut output), Ok(expected.len()));
    assert_eq!(output[..expected.len()], expected);
    let DecodedMessage::Send {
        mode,
        mut parameters,
    } = DecodedMessage::decode(&output[..expected.len()]).unwrap()
    else {
        panic!("expected send");
    };
    assert_eq!(mode, SendMode::WithAck);
    for expected in entries {
        let received = parameters.next().unwrap().unwrap();
        assert_eq!(
            received.into_value_for(expected.parameter).unwrap(),
            expected.value
        );
    }
    assert!(parameters.next().is_none());
    assert!(parameters.next().is_none());
}

#[test]
fn request_iterators_preserve_distinct_return_addresses_and_strings() {
    let requests = [
        Request {
            parameter: Parameter::new(1, 0, DataType::U16),
            response_process: 1,
            response_number: 1,
        },
        Request::new(Parameter::new(2, 3, DataType::String(8))),
    ];
    let mut output = [0; 12];
    let expected = [4, 129, 33, 1, 32, 2, 99, 2, 99, 8];
    let len = MessageRef::Request(&requests)
        .encode_into(&mut output)
        .unwrap();
    assert_eq!(&output[..len], expected);
    let DecodedMessage::Request(mut decoded) = DecodedMessage::decode(&output[..len]).unwrap()
    else {
        panic!("expected requests");
    };
    for expected in requests {
        assert_eq!(decoded.next(), Some(Ok(expected)));
    }
    assert!(decoded.next().is_none());
}

#[test]
fn strings_borrow_input_and_keep_padding_and_binary_bytes() {
    let input = [
        2, 0x81, 0x60, 0, b'a', b'b', 0, 2, 0x63, 4, 0, 255, b' ', b' ',
    ];
    let DecodedMessage::Send { mut parameters, .. } = DecodedMessage::decode(&input).unwrap()
    else {
        panic!("expected values");
    };
    let first = parameters.next().unwrap().unwrap();
    let ValueRef::Bytes(text) = first.value else {
        panic!("expected text")
    };
    assert!(core::ptr::eq(text.as_ptr(), input[4..].as_ptr()));
    assert_eq!(text, b"ab");
    let second = parameters.next().unwrap().unwrap();
    assert_eq!(second.value, ValueRef::Bytes(&[0, 255, b' ', b' ']));
    assert!(matches!(
        second.into_value_for(Parameter::new(2, 3, DataType::String(3))),
        Err(MessageError::UnexpectedStringLength { .. })
    ));
}

#[test]
fn insufficient_output_and_invalid_values_leave_storage_unchanged() {
    let request = [Request::new(Parameter::new(1, 1, DataType::U16))];
    let mut small = [0xaa; 4];
    assert_eq!(
        MessageRef::Request(&request).encode_into(&mut small),
        Err(MessageError::BufferTooSmall {
            required: 5,
            available: 4
        })
    );
    assert_eq!(small, [0xaa; 4]);
    let entries = [
        ParameterValueRef::new(Parameter::new(1, 1, DataType::U16), ValueRef::U16(20)),
        ParameterValueRef::new(
            Parameter::new(1, 2, DataType::String(0)),
            ValueRef::Bytes(b"embedded\0nul"),
        ),
    ];
    let mut output = [0xaa; 32];
    assert_eq!(
        MessageRef::Send {
            mode: SendMode::WithAck,
            parameters: &entries
        }
        .encode_into(&mut output),
        Err(MessageError::EmbeddedNul)
    );
    assert_eq!(output, [0xaa; 32]);
}

#[test]
fn decoder_validates_every_entry_before_exposing_a_message() {
    for data in [
        &[][..],
        &[0, 0],
        &[0, 0, 0, 1],
        &[7, 1],
        &[4, 1, 0, 1],
        &[4, 1, 0, 1, 0x20],
        &[4, 1, 0, 0x81, 0],
        &[4, 1, 0x60, 1, 0x60],
        &[4, 1, 0, 1, 0, 2],
        &[2, 1, 0x42, 0, 0],
        &[2, 0x81, 0, 0],
        &[2, 1, 0x80, 0],
        &[2, 1, 0x60, 2, 42],
        &[2, 1, 0x60, 0],
        &[2, 1, 0x60, 0, 42],
        &[2, 1, 1, 0xaa, 1, 0x42, 0], // Valid first entry, incomplete second entry.
    ] {
        assert!(
            DecodedMessage::decode(data).is_err(),
            "accepted {data:02x?}"
        );
    }
}

#[test]
fn payload_limits_and_master_string_width_are_checked_without_allocation() {
    for (width, fits) in [(251, true), (252, false)] {
        let bytes = [0; 252];
        let entry = [ParameterValueRef::new(
            Parameter::new(1, 1, DataType::String(width)),
            ValueRef::Bytes(&bytes[..usize::from(width)]),
        )];
        let msg = MessageRef::Send {
            mode: SendMode::NoAck,
            parameters: &entry,
        };
        let mut output = [0; MAX_PAYLOAD_LEN];
        assert_eq!(msg.encode_into(&mut output).is_ok(), fits);
        assert_eq!(
            msg.encode_for_master_into(&mut output),
            Err(MessageError::MasterStringWidth(width))
        );
    }
    let mut output = [0; 8];
    assert_eq!(
        MessageRef::Request(&[]).encode_into(&mut output),
        Err(MessageError::EmptyParameters)
    );
    assert!(matches!(
        DecodedMessage::decode(&[0; 256]),
        Err(MessageError::TooLong)
    ));
}
