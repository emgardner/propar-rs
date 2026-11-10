//! Allocation-free ProPar command encoding and borrowed message parsing.
//!
//! Build a [`MessageRef`] from slices and serialize it into a caller-provided
//! buffer. [`DecodedMessage`] validates an entire payload and returns iterators
//! whose byte values borrow the input. Owned convenience types require `std`.

use core::fmt;

const CHAIN: u8 = 0x80;
const TYPE_MASK: u8 = 0x60;
const NUMBER_MASK: u8 = 0x1f;
const BHT_MAX: i32 = 41942;
const BHT_MIN: i32 = BHT_MAX + 1 - 65536;
/// Maximum explicit string width in a master request or write, per ProPar 1.3.0.
pub const MAX_MASTER_STRING_WIDTH: u8 = 247;

/// The parameter's interpretation. Signed and float variants share wire type
/// codes with the corresponding unsigned integers.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum DataType {
    U8,
    U16,
    I16,
    /// Bronkhorst bipolar integer in the range -23593 through 41942.
    Bht,
    U32,
    F32,
    /// Length in bytes. Zero selects a NUL-terminated string.
    String(u8),
}

impl DataType {
    fn wire_code(self) -> u8 {
        match self {
            Self::U8 => 0,
            Self::U16 | Self::I16 | Self::Bht => 0x20,
            Self::U32 | Self::F32 => 0x40,
            Self::String(_) => 0x60,
        }
    }
}

/// A parameter in a device's address space. The protocol uses seven process
/// bits and five parameter-number bits. Encoding rejects out-of-range numbers.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct Parameter {
    pub process: u8,
    pub number: u8,
    pub kind: DataType,
}

impl Parameter {
    pub const fn new(process: u8, number: u8, kind: DataType) -> Self {
        Self {
            process,
            number,
            kind,
        }
    }

    fn validate(self) -> Result<(), MessageError> {
        if self.process > 0x7f || self.number > NUMBER_MASK {
            Err(MessageError::InvalidAddress {
                process: self.process,
                number: self.number,
            })
        } else {
            Ok(())
        }
    }
}

/// One entry of a read request. A peer copies the response process and number
/// into its reply. They default to the parameter being requested.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Request {
    pub parameter: Parameter,
    pub response_process: u8,
    pub response_number: u8,
}

impl Request {
    pub const fn new(parameter: Parameter) -> Self {
        Self {
            parameter,
            response_process: parameter.process,
            response_number: parameter.number,
        }
    }

    pub const fn response_parameter(self) -> Parameter {
        Parameter::new(
            self.response_process,
            self.response_number,
            self.parameter.kind,
        )
    }
}

/// Acknowledged write, unacknowledged write (also a read response), or broadcast.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum SendMode {
    WithAck = 1,
    NoAck = 2,
    Broadcast = 3,
}

/// Describe a status byte from command 0. Unknown vendor-specific codes remain
/// available in [`DecodedMessage::Status`] and return `None` here.
pub fn status_description(code: u8) -> Option<&'static str> {
    const DESCRIPTIONS: [&str; 36] = [
        "success",
        "process claimed",
        "unknown command",
        "unknown process",
        "unknown parameter",
        "invalid parameter type",
        "invalid parameter value",
        "network inactive",
        "start character timeout",
        "serial line timeout",
        "hardware memory error",
        "node address error",
        "general communication error",
        "read-only parameter",
        "PC communication error",
        "no RS232 connection",
        "PC out of memory",
        "write-only parameter",
        "unknown configuration",
        "no free node address",
        "wrong interface type",
        "serial port connection error",
        "error opening communication",
        "communication error",
        "interface bus master error",
        "response timeout",
        "missing start character",
        "invalid first digit",
        "host buffer overflow",
        "buffer overflow",
        "no answer found",
        "error closing communication",
        "synchronization error",
        "send error",
        "protocol error",
        "module buffer overflow",
    ];
    DESCRIPTIONS.get(usize::from(code)).copied()
}

/// Failure to serialize or interpret an application-layer message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessageError {
    Truncated,
    TrailingData,
    UnknownCommand(u8),
    EmptyParameters,
    TooLong,
    BufferTooSmall {
        required: usize,
        available: usize,
    },
    InvalidAddress {
        process: u8,
        number: u8,
    },
    UnexpectedParameter {
        expected: Parameter,
        received: Parameter,
    },
    TypeMismatch,
    BhtOutOfRange(i32),
    EmbeddedNul,
    StringTooLong {
        width: u8,
        actual: usize,
    },
    UnexpectedStringLength {
        expected: u8,
        actual: usize,
    },
    MasterStringWidth(u8),
}

impl fmt::Display for MessageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => f.write_str("truncated ProPar message"),
            Self::TrailingData => f.write_str("unexpected trailing data in ProPar message"),
            Self::UnknownCommand(command) => write!(f, "unknown ProPar command {command:#04x}"),
            Self::EmptyParameters => {
                f.write_str("a parameter message needs at least one parameter")
            }
            Self::BufferTooSmall {
                required,
                available,
            } => write!(
                f,
                "output needs {required} bytes, but only {available} are available"
            ),
            Self::TooLong => f.write_str("ProPar payload exceeds 255 bytes"),
            Self::InvalidAddress { process, number } => {
                write!(f, "invalid parameter address {process}/{number}")
            }
            Self::UnexpectedParameter { expected, received } => write!(
                f,
                "expected parameter {}/{}, received {}/{}",
                expected.process, expected.number, received.process, received.number
            ),
            Self::TypeMismatch => f.write_str("ProPar parameter types differ"),
            Self::BhtOutOfRange(n) => {
                write!(f, "{n} is outside the Bronkhorst signed integer range")
            }
            Self::EmbeddedNul => f.write_str("embedded NUL in a NUL-terminated value"),
            Self::StringTooLong { width, actual } => {
                write!(f, "{actual}-byte value exceeds fixed string width {width}")
            }
            Self::UnexpectedStringLength { expected, actual } => {
                write!(f, "requested {expected} string bytes, received {actual}")
            }
            Self::MasterStringWidth(width) => {
                write!(
                    f,
                    "fixed string width {width} exceeds the master's limit of {MAX_MASTER_STRING_WIDTH}"
                )
            }
        }
    }
}

impl core::error::Error for MessageError {}

#[cfg(feature = "std")]
mod owned;
#[cfg(feature = "std")]
pub use owned::{Message, ParameterValue, Value};

/// Maximum application payload size, excluding framing.
pub const MAX_PAYLOAD_LEN: usize = 255;

/// A numeric value or borrowed string/binary content.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ValueRef<'a> {
    U8(u8),
    U16(u16),
    I16(i16),
    Bht(i32),
    U32(u32),
    F32(f32),
    Bytes(&'a [u8]),
}

/// One addressed value. Byte content borrows its source buffer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParameterValueRef<'a> {
    pub parameter: Parameter,
    pub value: ValueRef<'a>,
}

impl<'a> ParameterValueRef<'a> {
    pub const fn new(parameter: Parameter, value: ValueRef<'a>) -> Self {
        Self { parameter, value }
    }

    /// Validate the address and wire type, then apply the requested signed,
    /// floating point, or string-width interpretation.
    pub fn into_value_for(self, expected: Parameter) -> Result<ValueRef<'a>, MessageError> {
        if (self.parameter.process, self.parameter.number) != (expected.process, expected.number) {
            return Err(MessageError::UnexpectedParameter {
                expected,
                received: self.parameter,
            });
        }
        if self.parameter.kind.wire_code() != expected.kind.wire_code() {
            return Err(MessageError::TypeMismatch);
        }
        match (expected.kind, self.value) {
            (DataType::U8, v @ ValueRef::U8(_))
            | (DataType::U16, v @ ValueRef::U16(_))
            | (DataType::U32, v @ ValueRef::U32(_)) => Ok(v),
            (DataType::String(width), ValueRef::Bytes(bytes)) => {
                if width != 0 && bytes.len() != usize::from(width) {
                    return Err(MessageError::UnexpectedStringLength {
                        expected: width,
                        actual: bytes.len(),
                    });
                }
                Ok(ValueRef::Bytes(bytes))
            }
            (DataType::F32, ValueRef::U32(bits)) => Ok(ValueRef::F32(f32::from_bits(bits))),
            (DataType::I16, ValueRef::U16(bits)) => Ok(ValueRef::I16(bits as i16)),
            (DataType::Bht, ValueRef::U16(bits)) => {
                let n = i32::from(bits);
                Ok(ValueRef::Bht(if n > BHT_MAX { n - 65536 } else { n }))
            }
            _ => Err(MessageError::TypeMismatch),
        }
    }

    fn encode_value(&self, out: &mut Writer<'_>) -> Result<(), MessageError> {
        match (self.parameter.kind, self.value) {
            (DataType::U8, ValueRef::U8(v)) => out.push(v)?,
            (DataType::U16, ValueRef::U16(v)) => out.extend(&v.to_be_bytes())?,
            (DataType::I16, ValueRef::I16(v)) => out.extend(&v.to_be_bytes())?,
            (DataType::Bht, ValueRef::Bht(v)) => {
                if !(BHT_MIN..=BHT_MAX).contains(&v) {
                    return Err(MessageError::BhtOutOfRange(v));
                }
                out.extend(&(v as u16).to_be_bytes())?;
            }
            (DataType::U32, ValueRef::U32(v)) => out.extend(&v.to_be_bytes())?,
            (DataType::F32, ValueRef::F32(v)) => out.extend(&v.to_be_bytes())?,
            (DataType::String(size), ValueRef::Bytes(bytes)) => {
                if bytes.len() > MAX_PAYLOAD_LEN {
                    return Err(MessageError::TooLong);
                }
                out.push(size)?;
                if size == 0 {
                    if bytes.contains(&0) {
                        return Err(MessageError::EmbeddedNul);
                    }
                    out.extend(bytes)?;
                    out.push(0)?;
                } else {
                    let width = usize::from(size);
                    if bytes.len() > width {
                        return Err(MessageError::StringTooLong {
                            width: size,
                            actual: bytes.len(),
                        });
                    }
                    out.extend(bytes)?;
                    out.repeat(b' ', width - bytes.len())?;
                }
            }
            _ => return Err(MessageError::TypeMismatch),
        }
        Ok(())
    }
}

/// A message to serialize. Slices and string values stay in caller-owned storage.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MessageRef<'a> {
    Status {
        code: u8,
        position: u8,
    },
    Error(u8),
    Request(&'a [Request]),
    Send {
        mode: SendMode,
        parameters: &'a [ParameterValueRef<'a>],
    },
}

impl MessageRef<'_> {
    /// Return the encoded size after validating all parameters.
    pub fn encoded_len(&self) -> Result<usize, MessageError> {
        let mut out = Writer {
            buffer: None,
            position: 0,
        };
        self.write(&mut out)?;
        Ok(out.position)
    }

    /// Encode a complete payload and return its size. The output is unchanged
    /// if validation fails or the buffer is too small.
    pub fn encode_into(&self, output: &mut [u8]) -> Result<usize, MessageError> {
        let required = self.encoded_len()?;
        if output.len() < required {
            return Err(MessageError::BufferTooSmall {
                required,
                available: output.len(),
            });
        }
        self.write(&mut Writer {
            buffer: Some(output),
            position: 0,
        })?;
        Ok(required)
    }

    /// Encode a master operation, enforcing the upstream explicit string-width
    /// limit. Instrument replies may use wider strings with [`Self::encode_into`].
    pub fn encode_for_master_into(&self, output: &mut [u8]) -> Result<usize, MessageError> {
        let check = |p: Parameter| match p.kind {
            DataType::String(width) if width > MAX_MASTER_STRING_WIDTH => {
                Err(MessageError::MasterStringWidth(width))
            }
            _ => Ok(()),
        };
        match self {
            Self::Request(requests) => requests.iter().try_for_each(|r| check(r.parameter))?,
            Self::Send { parameters, .. } => {
                parameters.iter().try_for_each(|v| check(v.parameter))?
            }
            _ => (),
        }
        self.encode_into(output)
    }

    fn write(&self, out: &mut Writer<'_>) -> Result<(), MessageError> {
        match self {
            Self::Status { code, position } => out.extend(&[0, *code, *position]),
            Self::Error(code) => out.push(*code),
            Self::Request(requests) => encode_chain(
                4,
                requests,
                |r| r.response_parameter(),
                |r, out| {
                    let p = r.parameter;
                    p.validate()?;
                    out.extend(&[p.process, p.number | p.kind.wire_code()])?;
                    if let DataType::String(size) = p.kind {
                        out.push(size)?;
                    }
                    Ok(())
                },
                out,
            ),
            Self::Send { mode, parameters } => encode_chain(
                *mode as u8,
                parameters,
                |v| v.parameter,
                |v, out| v.encode_value(out),
                out,
            ),
        }
    }
}

struct Writer<'a> {
    buffer: Option<&'a mut [u8]>,
    position: usize,
}

impl Writer<'_> {
    fn push(&mut self, byte: u8) -> Result<(), MessageError> {
        self.extend(&[byte])
    }

    fn extend(&mut self, bytes: &[u8]) -> Result<(), MessageError> {
        let end = self
            .position
            .checked_add(bytes.len())
            .filter(|&n| n <= MAX_PAYLOAD_LEN)
            .ok_or(MessageError::TooLong)?;
        if let Some(buffer) = &mut self.buffer {
            buffer[self.position..end].copy_from_slice(bytes);
        }
        self.position = end;
        Ok(())
    }

    fn repeat(&mut self, byte: u8, count: usize) -> Result<(), MessageError> {
        for _ in 0..count {
            self.push(byte)?;
        }
        Ok(())
    }
}

fn encode_chain<T>(
    command: u8,
    entries: &[T],
    address: impl Fn(&T) -> Parameter,
    encode_entry: impl Fn(&T, &mut Writer<'_>) -> Result<(), MessageError>,
    out: &mut Writer<'_>,
) -> Result<(), MessageError> {
    if entries.is_empty() {
        return Err(MessageError::EmptyParameters);
    }
    if entries.len() > MAX_PAYLOAD_LEN {
        return Err(MessageError::TooLong);
    }
    out.push(command)?;
    let mut groups = entries
        .chunk_by(|a, b| address(a).process == address(b).process)
        .peekable();
    while let Some(group) = groups.next() {
        let process = address(&group[0]).process;
        out.push(process | if groups.peek().is_some() { CHAIN } else { 0 })?;
        for (index, entry) in group.iter().enumerate() {
            let p = address(entry);
            p.validate()?;
            let chain_bit = if index + 1 < group.len() { CHAIN } else { 0 };
            out.push(p.number | p.kind.wire_code() | chain_bit)?;
            encode_entry(entry, out)?;
        }
    }
    Ok(())
}

/// A validated payload. Its parameter and request iterators borrow the input,
/// preserving byte strings without copying or allocating.
#[derive(Clone, Debug)]
pub enum DecodedMessage<'a> {
    Status {
        code: u8,
        position: u8,
    },
    Error(u8),
    Request(Requests<'a>),
    Send {
        mode: SendMode,
        parameters: Parameters<'a>,
    },
}

impl<'a> DecodedMessage<'a> {
    /// Validate the whole message before exposing its entries. Signed and float
    /// data initially use the corresponding unsigned wire representation.
    pub fn decode(data: &'a [u8]) -> Result<Self, MessageError> {
        if data.len() > MAX_PAYLOAD_LEN {
            return Err(MessageError::TooLong);
        }
        let (&command, rest) = data.split_first().ok_or(MessageError::Truncated)?;
        if rest.is_empty() {
            return Ok(Self::Error(command));
        }
        match command {
            0 => match rest {
                &[code, position] => Ok(Self::Status { code, position }),
                [_] => Err(MessageError::Truncated),
                _ => Err(MessageError::TrailingData),
            },
            1..=3 => {
                let mode = match command {
                    1 => SendMode::WithAck,
                    2 => SendMode::NoAck,
                    _ => SendMode::Broadcast,
                };
                let parameters = Parameters(Chain::new(rest, true));
                parameters.clone().try_for_each(|entry| entry.map(|_| ()))?;
                Ok(Self::Send { mode, parameters })
            }
            4 => {
                let requests = Requests(Chain::new(rest, false));
                requests.clone().try_for_each(|entry| entry.map(|_| ()))?;
                Ok(Self::Request(requests))
            }
            other => Err(MessageError::UnknownCommand(other)),
        }
    }
}

/// Iterate over decoded request entries in wire order.
#[derive(Clone, Debug)]
pub struct Requests<'a>(Chain<'a>);

impl Iterator for Requests<'_> {
    type Item = Result<Request, MessageError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.entry(|(response_process, index), input| {
            let process = input.byte()?;
            let number = input.byte()?;
            if process & CHAIN != 0 || number & CHAIN != 0 {
                return Err(MessageError::InvalidAddress { process, number });
            }
            if number & TYPE_MASK != index & TYPE_MASK {
                return Err(MessageError::TypeMismatch);
            }
            Ok(Request {
                parameter: input.parameter(process, number)?,
                response_process,
                response_number: index & NUMBER_MASK,
            })
        })
    }
}
impl core::iter::FusedIterator for Requests<'_> {}

/// Iterate over decoded parameter values in wire order.
#[derive(Clone, Debug)]
pub struct Parameters<'a>(Chain<'a>);

impl<'a> Iterator for Parameters<'a> {
    type Item = Result<ParameterValueRef<'a>, MessageError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.entry(|(process, number), input| {
            let parameter = input.parameter(process, number)?;
            Ok(ParameterValueRef::new(
                parameter,
                input.value(parameter.kind)?,
            ))
        })
    }
}
impl core::iter::FusedIterator for Parameters<'_> {}

#[derive(Clone, Debug)]
struct Cursor<'a>(&'a [u8]);

impl<'a> Cursor<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], MessageError> {
        let (data, rest) = self
            .0
            .split_at_checked(len)
            .ok_or(MessageError::Truncated)?;
        self.0 = rest;
        Ok(data)
    }

    fn byte(&mut self) -> Result<u8, MessageError> {
        Ok(self.take(1)?[0])
    }

    fn parameter(&mut self, process: u8, number: u8) -> Result<Parameter, MessageError> {
        let kind = match number & TYPE_MASK {
            0 => DataType::U8,
            0x20 => DataType::U16,
            0x40 => DataType::U32,
            _ => DataType::String(self.byte()?),
        };
        Ok(Parameter::new(process, number & NUMBER_MASK, kind))
    }

    fn value(&mut self, kind: DataType) -> Result<ValueRef<'a>, MessageError> {
        Ok(match kind {
            DataType::U8 => ValueRef::U8(self.byte()?),
            DataType::U16 => {
                let bytes = self.take(2)?;
                ValueRef::U16(u16::from_be_bytes([bytes[0], bytes[1]]))
            }
            DataType::U32 => {
                let bytes = self.take(4)?;
                ValueRef::U32(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
            }
            DataType::String(0) => {
                let len = self
                    .0
                    .iter()
                    .position(|b| *b == 0)
                    .ok_or(MessageError::Truncated)?;
                ValueRef::Bytes(&self.take(len + 1)?[..len])
            }
            DataType::String(n) => ValueRef::Bytes(self.take(usize::from(n))?),
            _ => return Err(MessageError::TypeMismatch),
        })
    }
}

#[derive(Clone, Debug)]
struct Chain<'a> {
    input: Cursor<'a>,
    process: u8,
    need_process: bool,
    done: bool,
    allow_unchained: bool,
}

impl<'a> Chain<'a> {
    fn new(input: &'a [u8], allow_unchained: bool) -> Self {
        Self {
            input: Cursor(input),
            process: 0,
            need_process: true,
            done: false,
            allow_unchained,
        }
    }

    fn entry<T>(
        &mut self,
        read: impl FnOnce((u8, u8), &mut Cursor<'a>) -> Result<T, MessageError>,
    ) -> Option<Result<T, MessageError>> {
        if self.done {
            return None;
        }
        let result = (|| {
            if self.need_process {
                self.process = self.input.byte()?;
            }
            let number = self.input.byte()?;
            let value = read((self.process & !CHAIN, number), &mut self.input)?;
            self.need_process = number & CHAIN == 0;
            if self.need_process && self.process & CHAIN == 0 {
                if self.input.0.is_empty() {
                    self.done = true;
                } else if !self.allow_unchained {
                    return Err(MessageError::TrailingData);
                }
            }
            Ok(value)
        })();
        if result.is_err() {
            self.done = true;
        }
        Some(result)
    }
}

#[cfg(test)]
mod borrowed_tests;
#[cfg(all(test, feature = "std"))]
mod tests;
