//! ProPar enhanced binary and ASCII serial framing without allocation.
//!
//! [`PacketRef::decode_into`] parses exactly one packet into caller-provided
//! storage. [`Decoder`] incrementally parses a stream and lends completed
//! payloads from its fixed internal buffer. With `std`, `Frame` and `Packet`
//! provide owned convenience wrappers around the same codec.

use core::fmt;
#[cfg(feature = "std")]
use std::{vec, vec::Vec};

const DLE: u8 = 0x10;
const STX: u8 = 0x02;
const ETX: u8 = 0x03;
const MAX_BODY_LEN: usize = 258;

/// Largest ProPar payload in enhanced binary mode. ASCII supports 254 bytes.
pub const MAX_PAYLOAD_LEN: usize = 255;
/// Buffer size sufficient for any encoded frame, including binary byte stuffing.
pub const MAX_FRAME_LEN: usize = 520;

/// Serial encoding. Enhanced binary is the upstream master's default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Binary,
    Ascii,
}

/// A borrowed addressed ProPar message, before serial framing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameRef<'a> {
    /// Echoed by the peer in binary mode. ASCII has no sequence field;
    /// its encoder ignores this value and its decoder returns zero.
    pub sequence: u8,
    pub address: u8,
    /// A command and its body. At most 255 bytes in binary, 254 in ASCII.
    pub payload: &'a [u8],
}

impl FrameRef<'_> {
    /// Number of bytes needed to encode this frame.
    pub fn encoded_len(&self, mode: Mode) -> Result<usize, FrameError> {
        let len = self.payload_len(mode)?;
        Ok(match mode {
            Mode::Binary => binary_len(self.sequence, self.address, len, self.payload),
            Mode::Ascii => 3 + 2 * (2 + self.payload.len()),
        })
    }

    /// Encode into caller storage and return the number of bytes written.
    /// Validation and capacity errors leave `output` unchanged.
    pub fn encode_into(&self, mode: Mode, output: &mut [u8]) -> Result<usize, FrameError> {
        PacketRef::Frame(*self).encode_into(mode, output)
    }

    fn payload_len(&self, mode: Mode) -> Result<u8, FrameError> {
        if self.payload.is_empty() {
            return Err(FrameError::InvalidLength);
        }
        let len = u8::try_from(self.payload.len()).map_err(|_| FrameError::TooLong)?;
        if mode == Mode::Ascii && len == u8::MAX {
            return Err(FrameError::TooLong);
        }
        Ok(len)
    }

    /// Copy the payload into an owned frame.
    #[cfg(feature = "std")]
    pub fn to_owned(self) -> Frame {
        Frame {
            sequence: self.sequence,
            address: self.address,
            payload: self.payload.to_vec(),
        }
    }
}

/// A borrowed message or an error reported by the serial interface itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketRef<'a> {
    Frame(FrameRef<'a>),
    /// ASCII interface error, such as `:0109\r\n`. No node or sequence is sent.
    InterfaceError(u8),
    /// Enhanced binary interface error. Its length field is zero even though
    /// an error code follows it (Bronkhorst manual 9.17.027, §3.3.4).
    BinaryError {
        sequence: u8,
        address: u8,
        code: u8,
    },
}

impl<'a> PacketRef<'a> {
    /// Number of bytes needed to encode this packet.
    pub fn encoded_len(&self, mode: Mode) -> Result<usize, FrameError> {
        match (*self, mode) {
            (Self::Frame(frame), _) => frame.encoded_len(mode),
            (Self::InterfaceError(_), Mode::Ascii) => Ok(7),
            (
                Self::BinaryError {
                    sequence,
                    address,
                    code,
                },
                Mode::Binary,
            ) => Ok(binary_len(sequence, address, 0, &[code])),
            _ => Err(FrameError::WrongMode),
        }
    }

    /// Encode into caller storage and return the number of bytes written.
    /// Validation and capacity errors leave `output` unchanged.
    pub fn encode_into(&self, mode: Mode, output: &mut [u8]) -> Result<usize, FrameError> {
        let needed = self.encoded_len(mode)?;
        if output.len() < needed {
            return Err(FrameError::BufferTooSmall);
        }
        match (*self, mode) {
            (Self::Frame(frame), Mode::Binary) => {
                encode_binary(
                    frame.sequence,
                    frame.address,
                    frame.payload.len() as u8,
                    frame.payload,
                    output,
                );
            }
            (Self::Frame(frame), Mode::Ascii) => {
                encode_ascii(
                    [frame.payload.len() as u8 + 1, frame.address]
                        .into_iter()
                        .chain(frame.payload.iter().copied()),
                    output,
                );
            }
            (Self::InterfaceError(code), Mode::Ascii) => encode_ascii([1, code], output),
            (
                Self::BinaryError {
                    sequence,
                    address,
                    code,
                },
                Mode::Binary,
            ) => {
                encode_binary(sequence, address, 0, &[code], output);
            }
            _ => return Err(FrameError::WrongMode),
        }
        Ok(needed)
    }

    /// Parse exactly one complete packet, copying its unescaped payload into
    /// `output`. The returned frame borrows only the written payload bytes.
    /// Error packets require no output storage. Incomplete or invalid packets,
    /// trailing input and insufficient output leave `output` unchanged.
    pub fn decode_into(mode: Mode, bytes: &[u8], output: &'a mut [u8]) -> Result<Self, FrameError> {
        let prefix: &[u8] = match mode {
            Mode::Binary => &[DLE, STX],
            Mode::Ascii => b":",
        };
        if !bytes.starts_with(prefix) {
            return Err(if prefix.starts_with(bytes) {
                FrameError::Incomplete
            } else {
                FrameError::InvalidStart
            });
        }
        let mut decoder = Decoder::new(mode);
        let mut input = bytes.iter().copied();
        while let Some(byte) = input.next() {
            if let Some(packet) = decoder.push(byte)? {
                if input.len() != 0 {
                    return Err(FrameError::TrailingData);
                }
                return match packet {
                    PacketRef::Frame(frame) => {
                        let target = output
                            .get_mut(..frame.payload.len())
                            .ok_or(FrameError::BufferTooSmall)?;
                        target.copy_from_slice(frame.payload);
                        Ok(Self::Frame(FrameRef {
                            sequence: frame.sequence,
                            address: frame.address,
                            payload: target,
                        }))
                    }
                    PacketRef::InterfaceError(code) => Ok(Self::InterfaceError(code)),
                    PacketRef::BinaryError {
                        sequence,
                        address,
                        code,
                    } => Ok(Self::BinaryError {
                        sequence,
                        address,
                        code,
                    }),
                };
            }
        }
        Err(FrameError::Incomplete)
    }

    /// Copy any borrowed payload into an owned packet.
    #[cfg(feature = "std")]
    pub fn to_owned(self) -> Packet {
        match self {
            Self::Frame(frame) => Packet::Frame(frame.to_owned()),
            Self::InterfaceError(code) => Packet::InterfaceError(code),
            Self::BinaryError {
                sequence,
                address,
                code,
            } => Packet::BinaryError {
                sequence,
                address,
                code,
            },
        }
    }
}

/// An owned addressed ProPar message, available with the `std` feature.
#[cfg(feature = "std")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub sequence: u8,
    pub address: u8,
    pub payload: Vec<u8>,
}

#[cfg(feature = "std")]
impl Frame {
    pub fn as_ref(&self) -> FrameRef<'_> {
        FrameRef {
            sequence: self.sequence,
            address: self.address,
            payload: &self.payload,
        }
    }

    /// Serialize the frame, including delimiters and binary byte stuffing.
    pub fn encode(&self, mode: Mode) -> Result<Vec<u8>, FrameError> {
        let mut output = vec![0; self.as_ref().encoded_len(mode)?];
        self.as_ref().encode_into(mode, &mut output)?;
        Ok(output)
    }
}

/// An owned message or serial-interface error, available with `std`.
#[cfg(feature = "std")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Packet {
    Frame(Frame),
    InterfaceError(u8),
    BinaryError { sequence: u8, address: u8, code: u8 },
}

#[cfg(feature = "std")]
impl Packet {
    pub fn as_ref(&self) -> PacketRef<'_> {
        match self {
            Self::Frame(frame) => PacketRef::Frame(frame.as_ref()),
            &Self::InterfaceError(code) => PacketRef::InterfaceError(code),
            &Self::BinaryError {
                sequence,
                address,
                code,
            } => PacketRef::BinaryError {
                sequence,
                address,
                code,
            },
        }
    }

    /// Serialize a packet. An interface error requires its corresponding mode.
    pub fn encode(&self, mode: Mode) -> Result<Vec<u8>, FrameError> {
        let mut output = vec![0; self.as_ref().encoded_len(mode)?];
        self.as_ref().encode_into(mode, &mut output)?;
        Ok(output)
    }

    /// Parse exactly one complete packet, rejecting leading or trailing data.
    pub fn decode(mode: Mode, bytes: &[u8]) -> Result<Self, FrameError> {
        let mut payload = [0; MAX_PAYLOAD_LEN];
        Ok(PacketRef::decode_into(mode, bytes, &mut payload)?.to_owned())
    }
}

fn binary_len(sequence: u8, address: u8, len: u8, payload: &[u8]) -> usize {
    7 + payload.len()
        + [sequence, address, len]
            .into_iter()
            .chain(payload.iter().copied())
            .filter(|&byte| byte == DLE)
            .count()
}

fn encode_binary(sequence: u8, address: u8, len: u8, payload: &[u8], output: &mut [u8]) {
    output[..2].copy_from_slice(&[DLE, STX]);
    let mut offset = 2;
    for byte in [sequence, address, len]
        .into_iter()
        .chain(payload.iter().copied())
    {
        output[offset] = byte;
        offset += 1;
        if byte == DLE {
            output[offset] = DLE;
            offset += 1;
        }
    }
    output[offset..offset + 2].copy_from_slice(&[DLE, ETX]);
}

fn encode_ascii(bytes: impl IntoIterator<Item = u8>, output: &mut [u8]) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    output[0] = b':';
    let mut offset = 1;
    for byte in bytes {
        output[offset..offset + 2]
            .copy_from_slice(&[HEX[usize::from(byte >> 4)], HEX[usize::from(byte & 15)]]);
        offset += 2;
    }
    output[offset..offset + 2].copy_from_slice(b"\r\n");
}

/// Errors in wire framing, independent of parameter or I/O errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    InvalidStart,
    UnexpectedStart,
    InvalidEscape,
    InvalidHex,
    InvalidTerminator,
    InvalidLength,
    TooLong,
    Incomplete,
    TrailingData,
    WrongMode,
    BufferTooSmall,
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidStart => "invalid start of ProPar frame",
            Self::UnexpectedStart => "new frame started before the old frame ended",
            Self::InvalidEscape => "invalid binary DLE escape",
            Self::InvalidHex => "invalid ASCII hex digit or incomplete pair",
            Self::InvalidTerminator => "expected ASCII CRLF terminator",
            Self::InvalidLength => "invalid ProPar frame length",
            Self::TooLong => "ProPar frame exceeds the wire length limit",
            Self::Incomplete => "incomplete ProPar frame",
            Self::TrailingData => "trailing bytes after ProPar frame",
            Self::WrongMode => "interface error belongs to a different framing mode",
            Self::BufferTooSmall => "output buffer is too small for ProPar frame",
        })
    }
}

impl core::error::Error for FrameError {}

#[derive(Debug, Clone, Copy)]
enum State {
    Idle,
    StartDle,
    Binary,
    DataDle,
    Ascii { high: Option<u8> },
    Cr,
}

/// Incremental frame decoder with fixed storage of 258 bytes.
///
/// Noise between frames is skipped. A framing error discards the invalid
/// frame. If a new start delimiter caused the error, it remains consumed as
/// the beginning of the next frame. Keep feeding bytes to resynchronize.
/// Completed packets borrow the decoder until the next byte is consumed.
#[derive(Debug)]
pub struct Decoder {
    mode: Mode,
    state: State,
    body: [u8; MAX_BODY_LEN],
    body_len: usize,
}

impl Decoder {
    pub const fn new(mode: Mode) -> Self {
        Self {
            mode,
            state: State::Idle,
            body: [0; MAX_BODY_LEN],
            body_len: 0,
        }
    }

    /// Forget a partial frame and wait for the next start delimiter.
    pub fn reset(&mut self) {
        self.state = State::Idle;
        self.body_len = 0;
    }

    /// Consume one byte, lending a packet as soon as its terminator arrives.
    pub fn push(&mut self, byte: u8) -> Result<Option<PacketRef<'_>>, FrameError> {
        match self.state {
            State::Idle => match (self.mode, byte) {
                (Mode::Binary, DLE) => self.state = State::StartDle,
                (Mode::Ascii, b':') => self.state = State::Ascii { high: None },
                _ => (),
            },
            State::StartDle => match byte {
                STX => self.state = State::Binary,
                DLE => (),
                _ => self.state = State::Idle,
            },
            State::Binary if byte == DLE => self.state = State::DataDle,
            State::Binary => self.append(byte)?,
            State::DataDle => match byte {
                DLE => {
                    self.state = State::Binary;
                    self.append(DLE)?;
                }
                ETX => return self.finish(),
                STX => {
                    self.body_len = 0;
                    self.state = State::Binary;
                    return Err(FrameError::UnexpectedStart);
                }
                _ => {
                    self.reset();
                    return Err(FrameError::InvalidEscape);
                }
            },
            State::Ascii { .. } | State::Cr if byte == b':' => {
                self.body_len = 0;
                self.state = State::Ascii { high: None };
                return Err(FrameError::UnexpectedStart);
            }
            State::Ascii { high: None } if byte == b'\r' => self.state = State::Cr,
            State::Ascii { high } => {
                let nibble = match byte {
                    b'0'..=b'9' => byte - b'0',
                    b'A'..=b'F' => byte - b'A' + 10,
                    b'a'..=b'f' => byte - b'a' + 10,
                    _ => {
                        self.reset();
                        return Err(FrameError::InvalidHex);
                    }
                };
                if let Some(upper) = high {
                    self.state = State::Ascii { high: None };
                    self.append(upper << 4 | nibble)?;
                } else {
                    self.state = State::Ascii { high: Some(nibble) };
                }
            }
            State::Cr if byte == b'\n' => return self.finish(),
            State::Cr => {
                self.reset();
                return Err(FrameError::InvalidTerminator);
            }
        }
        Ok(None)
    }

    fn append(&mut self, byte: u8) -> Result<(), FrameError> {
        let max_len = match self.mode {
            Mode::Binary => MAX_BODY_LEN,
            Mode::Ascii => 256,
        };
        if self.body_len == max_len {
            self.reset();
            return Err(FrameError::TooLong);
        }
        self.body[self.body_len] = byte;
        self.body_len += 1;
        Ok(())
    }

    fn finish(&mut self) -> Result<Option<PacketRef<'_>>, FrameError> {
        let len = self.body_len;
        self.reset();
        let packet = match (self.mode, &self.body[..len]) {
            (Mode::Ascii, &[1, code]) => PacketRef::InterfaceError(code),
            (Mode::Binary, &[sequence, address, 0, code]) => PacketRef::BinaryError {
                sequence,
                address,
                code,
            },
            (Mode::Binary, [sequence, address, len, payload @ ..])
                if !payload.is_empty() && usize::from(*len) == payload.len() =>
            {
                PacketRef::Frame(FrameRef {
                    sequence: *sequence,
                    address: *address,
                    payload,
                })
            }
            (Mode::Ascii, [len, address, payload @ ..])
                if !payload.is_empty() && usize::from(*len) == payload.len() + 1 =>
            {
                PacketRef::Frame(FrameRef {
                    sequence: 0,
                    address: *address,
                    payload,
                })
            }
            _ => return Err(FrameError::InvalidLength),
        };
        Ok(Some(packet))
    }
}

#[cfg(test)]
#[path = "frame/tests.rs"]
mod tests;

#[cfg(all(test, feature = "std"))]
#[path = "frame/owned_tests.rs"]
mod owned_tests;
