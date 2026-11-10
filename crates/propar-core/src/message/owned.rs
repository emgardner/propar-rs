//! Owned convenience types for standard-library clients.

use super::{
    DecodedMessage, MAX_PAYLOAD_LEN, MessageError, MessageRef, Parameter, ParameterValueRef,
    Request, SendMode, ValueRef,
};

/// An owned parameter value. Strings retain arbitrary bytes and fixed-width padding.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    U8(u8),
    U16(u16),
    I16(i16),
    Bht(i32),
    U32(u32),
    F32(f32),
    Bytes(Vec<u8>),
}

impl Value {
    /// Borrow the value for use with the allocation-free codec.
    pub fn as_ref(&self) -> ValueRef<'_> {
        match self {
            Self::U8(v) => ValueRef::U8(*v),
            Self::U16(v) => ValueRef::U16(*v),
            Self::I16(v) => ValueRef::I16(*v),
            Self::Bht(v) => ValueRef::Bht(*v),
            Self::U32(v) => ValueRef::U32(*v),
            Self::F32(v) => ValueRef::F32(*v),
            Self::Bytes(v) => ValueRef::Bytes(v),
        }
    }
}

impl ValueRef<'_> {
    /// Copy a borrowed value into an owned value. Byte strings allocate storage.
    pub fn to_owned(self) -> Value {
        match self {
            Self::U8(v) => Value::U8(v),
            Self::U16(v) => Value::U16(v),
            Self::I16(v) => Value::I16(v),
            Self::Bht(v) => Value::Bht(v),
            Self::U32(v) => Value::U32(v),
            Self::F32(v) => Value::F32(v),
            Self::Bytes(v) => Value::Bytes(v.to_vec()),
        }
    }
}

/// An owned value and the parameter address under which it was sent.
#[derive(Clone, Debug, PartialEq)]
pub struct ParameterValue {
    pub parameter: Parameter,
    pub value: Value,
}

impl ParameterValue {
    pub const fn new(parameter: Parameter, value: Value) -> Self {
        Self { parameter, value }
    }

    /// Borrow this entry for encoding without copying its bytes.
    pub fn as_ref(&self) -> ParameterValueRef<'_> {
        ParameterValueRef::new(self.parameter, self.value.as_ref())
    }

    /// Validate the reply address and reinterpret the value using the requested type.
    pub fn into_value_for(self, expected: Parameter) -> Result<Value, MessageError> {
        let value = self.as_ref().into_value_for(expected)?;
        if matches!(value, ValueRef::Bytes(_)) {
            return Ok(self.value);
        }
        Ok(value.to_owned())
    }
}

impl ParameterValueRef<'_> {
    /// Copy this entry into an owned parameter value.
    pub fn to_owned(self) -> ParameterValue {
        ParameterValue::new(self.parameter, self.value.to_owned())
    }
}

/// An owned application-layer message, available with the `std` feature.
#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    Status {
        code: u8,
        position: u8,
    },
    /// Host/bridge error. Physical links may use a special error frame.
    Error(u8),
    Request(Vec<Request>),
    Send {
        mode: SendMode,
        parameters: Vec<ParameterValue>,
    },
}

impl Message {
    /// Serialize a master operation with the upstream explicit string-width limit.
    pub fn encode_for_master(&self) -> Result<Vec<u8>, MessageError> {
        self.encode_with(true)
    }

    /// Serialize a message using the allocation-free core codec.
    pub fn encode(&self) -> Result<Vec<u8>, MessageError> {
        self.encode_with(false)
    }

    fn encode_with(&self, master: bool) -> Result<Vec<u8>, MessageError> {
        let mut buffer = [0; MAX_PAYLOAD_LEN];
        let mut encode = |message: MessageRef<'_>| {
            if master {
                message.encode_for_master_into(&mut buffer)
            } else {
                message.encode_into(&mut buffer)
            }
        };
        let len = match self {
            Self::Status { code, position } => encode(MessageRef::Status {
                code: *code,
                position: *position,
            }),
            Self::Error(code) => encode(MessageRef::Error(*code)),
            Self::Request(requests) => encode(MessageRef::Request(requests)),
            Self::Send { mode, parameters } => {
                if parameters.len() > MAX_PAYLOAD_LEN {
                    return Err(MessageError::TooLong);
                }
                let entries: Vec<_> = parameters.iter().map(ParameterValue::as_ref).collect();
                encode(MessageRef::Send {
                    mode: *mode,
                    parameters: &entries,
                })
            }
        }?;
        Ok(buffer[..len].to_vec())
    }

    /// Decode and collect the borrowed core parser's values into owned storage.
    pub fn decode(data: &[u8]) -> Result<Self, MessageError> {
        match DecodedMessage::decode(data)? {
            DecodedMessage::Status { code, position } => Ok(Self::Status { code, position }),
            DecodedMessage::Error(code) => Ok(Self::Error(code)),
            DecodedMessage::Request(requests) => {
                Ok(Self::Request(requests.collect::<Result<_, _>>()?))
            }
            DecodedMessage::Send { mode, parameters } => Ok(Self::Send {
                mode,
                parameters: parameters
                    .map(|entry| entry.map(ParameterValueRef::to_owned))
                    .collect::<Result<_, _>>()?,
            }),
        }
    }
}
