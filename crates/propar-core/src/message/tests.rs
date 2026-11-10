#![cfg(feature = "std")]

use super::*;

fn send(mode: SendMode, parameters: Vec<ParameterValue>) -> Message {
    Message::Send { mode, parameters }
}

fn decoded_values(data: &[u8]) -> Vec<ParameterValue> {
    let Message::Send { parameters, .. } = Message::decode(data).unwrap() else {
        panic!("expected parameter values")
    };
    parameters
}

#[test]
fn chained_read_and_write_match_python_1_3_0() {
    // Captured from _propar_builder using a subset of upstream test/chaining_test.py.
    let values = vec![
        ParameterValue::new(Parameter::new(97, 1, DataType::U16), Value::U16(2000)),
        ParameterValue::new(Parameter::new(97, 3, DataType::U8), Value::U8(1)),
        ParameterValue::new(Parameter::new(1, 1, DataType::U16), Value::U16(7000)),
        ParameterValue::new(Parameter::new(33, 3, DataType::F32), Value::F32(0.1)),
    ];
    let request = Message::Request(values.iter().map(|v| Request::new(v.parameter)).collect());
    let read = [
        4, 225, 161, 97, 33, 3, 97, 3, 129, 33, 1, 33, 33, 67, 33, 67,
    ];
    assert_eq!(request.encode().unwrap(), read);
    assert_eq!(Message::decode(&read).unwrap().encode().unwrap(), read);

    let write = [
        1, 225, 161, 7, 208, 3, 1, 129, 33, 27, 88, 33, 67, 61, 204, 204, 205,
    ];
    assert_eq!(
        send(SendMode::WithAck, values.clone()).encode().unwrap(),
        write
    );
    for (received, expected) in decoded_values(&write).into_iter().zip(values) {
        assert_eq!(
            received.into_value_for(expected.parameter).unwrap(),
            expected.value
        );
    }
}

#[test]
fn manual_measure_query_uses_distinct_return_index() {
    // Manual 9.17.027 §3.10.3, excluding the outer binary framing.
    let request = Request {
        parameter: Parameter::new(1, 0, DataType::U16),
        response_process: 1,
        response_number: 1,
    };
    let read = [4, 1, 0x21, 1, 0x20];
    assert_eq!(
        Message::decode(&read).unwrap(),
        Message::Request(vec![request])
    );
    assert_eq!(Message::Request(vec![request]).encode().unwrap(), read);

    let mut answer = decoded_values(&[2, 1, 0x21, 0x7d, 0x00]);
    let value = answer.pop().unwrap();
    assert!(matches!(
        value.clone().into_value_for(request.parameter),
        Err(MessageError::UnexpectedParameter { .. })
    ));
    assert_eq!(
        value.into_value_for(request.response_parameter()).unwrap(),
        Value::U16(32000)
    );
}

#[test]
fn string_modes_and_binary_payload_survive_chaining() {
    let msg = send(
        SendMode::Broadcast,
        vec![
            ParameterValue::new(
                Parameter::new(1, 2, DataType::String(0)),
                Value::Bytes(vec![]),
            ),
            ParameterValue::new(
                Parameter::new(1, 3, DataType::String(4)),
                Value::Bytes(vec![0, 255]),
            ),
            ParameterValue::new(
                Parameter::new(2, 4, DataType::String(0)),
                Value::Bytes(b"hi".to_vec()),
            ),
        ],
    );
    let wire = [
        3, 0x81, 0xe2, 0, 0, 0x63, 4, 0, 255, 32, 32, 2, 0x64, 0, b'h', b'i', 0,
    ];
    assert_eq!(msg.encode().unwrap(), wire);
    let decoded = Message::decode(&wire).unwrap();
    assert_eq!(decoded.encode().unwrap(), wire);
    assert_eq!(
        decoded_values(&wire)[1].value,
        Value::Bytes(vec![0, 255, 32, 32])
    );
    let req = Message::Request(vec![
        Request::new(Parameter::new(1, 2, DataType::String(0))),
        Request::new(Parameter::new(1, 3, DataType::String(4))),
    ]);
    let read = [4, 1, 0xe2, 1, 0x62, 0, 0x63, 1, 0x63, 4];
    assert_eq!(req.encode().unwrap(), read);
    assert_eq!(Message::decode(&read).unwrap(), req);
}

#[test]
fn signed_ranges_round_trip_at_boundaries() {
    for (kind, bits, value) in [
        (DataType::Bht, 0xa3d6u16, Value::Bht(41942)),
        (DataType::Bht, 0xa3d7, Value::Bht(-23593)),
        (DataType::Bht, 0xffff, Value::Bht(-1)),
        (DataType::I16, 0xffff, Value::I16(-1)),
        (DataType::I16, 0x8000, Value::I16(i16::MIN)),
    ] {
        let p = Parameter::new(1, 0, kind);
        let msg = send(SendMode::NoAck, vec![ParameterValue::new(p, value.clone())]);
        let [a, b] = bits.to_be_bytes();
        let wire = [2, 1, 0x20, a, b];
        assert_eq!(msg.encode().unwrap(), wire);
        assert_eq!(
            decoded_values(&wire)
                .pop()
                .unwrap()
                .into_value_for(p)
                .unwrap(),
            value
        );
    }
}

#[test]
fn older_devices_may_repeat_process_headers_in_a_reply() {
    let values = decoded_values(&[2, 1, 1, 0xaa, 1, 2, 0xbb]);
    assert_eq!(values.len(), 2);
    assert_eq!(
        values[1],
        ParameterValue::new(Parameter::new(1, 2, DataType::U8), Value::U8(0xbb))
    );
}

#[test]
fn status_preserves_error_code_and_byte_position() {
    assert_eq!(
        Message::decode(&[0, 0x0d, 5]).unwrap(),
        Message::Status {
            code: 13,
            position: 5
        }
    );
    assert_eq!(Message::decode(&[9]).unwrap(), Message::Error(9));
    assert_eq!(Message::Error(9).encode().unwrap(), [9]);
}

#[test]
fn malformed_inputs_fail_without_partial_results() {
    for bytes in [
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
    ] {
        assert!(Message::decode(bytes).is_err(), "accepted {bytes:02x?}");
    }
    assert_eq!(Message::decode(&[0; 256]), Err(MessageError::TooLong));
}

#[test]
fn encoding_enforces_lengths_addresses_and_representable_values() {
    for (p, value) in [
        (Parameter::new(128, 1, DataType::U8), Value::U8(1)),
        (Parameter::new(1, 32, DataType::U8), Value::U8(1)),
        (Parameter::new(1, 1, DataType::U16), Value::U8(1)),
        (Parameter::new(1, 1, DataType::Bht), Value::Bht(-23594)),
        (Parameter::new(1, 1, DataType::Bht), Value::Bht(41943)),
        (
            Parameter::new(1, 1, DataType::String(0)),
            Value::Bytes(vec![0]),
        ),
        (
            Parameter::new(1, 1, DataType::String(1)),
            Value::Bytes(vec![0, 1]),
        ),
    ] {
        assert!(
            send(SendMode::WithAck, vec![ParameterValue::new(p, value)])
                .encode()
                .is_err()
        );
    }
    assert_eq!(
        Message::Request(vec![]).encode(),
        Err(MessageError::EmptyParameters)
    );
    for (width, fits) in [(251, true), (252, false)] {
        let p = Parameter::new(1, 1, DataType::String(width));
        let message = send(
            SendMode::NoAck,
            vec![ParameterValue::new(p, Value::Bytes(vec![0; width.into()]))],
        );
        assert_eq!(message.encode().is_ok(), fits);
    }
}

#[test]
fn master_string_width_limit_still_accepts_long_instrument_replies() {
    let fixed = |width| Parameter::new(1, 1, DataType::String(width));
    for (width, valid) in [(247, true), (248, false)] {
        let p = fixed(width);
        let request = Message::Request(vec![Request::new(p)]);
        let write = send(
            SendMode::WithAck,
            vec![ParameterValue::new(
                p,
                Value::Bytes(vec![1; usize::from(width)]),
            )],
        );
        assert_eq!(request.encode_for_master().is_ok(), valid);
        assert_eq!(write.encode_for_master().is_ok(), valid);
        let bytes = write.encode().unwrap();
        let received = decoded_values(&bytes).pop().unwrap();
        assert_eq!(
            received.into_value_for(fixed(0)).unwrap(),
            Value::Bytes(vec![1; usize::from(width)])
        );
    }
    let short = decoded_values(&[2, 1, 0x61, 0, b'x', 0]).pop().unwrap();
    assert_eq!(
        short.clone().into_value_for(fixed(1)).unwrap(),
        Value::Bytes(vec![b'x'])
    );
    assert_eq!(
        short.into_value_for(fixed(2)),
        Err(MessageError::UnexpectedStringLength {
            expected: 2,
            actual: 1
        })
    );
    assert_eq!(status_description(0x0d), Some("read-only parameter"));
    assert_eq!(status_description(0xff), None);
}
