#![no_std]
#![forbid(unsafe_code)]
#![doc = include_str!("../README.md")]

use core::{fmt, future::Future, future::poll_fn, pin::pin, task::Poll};
use embedded_hal_async::delay::DelayNs;
use embedded_io_async::{Read, Write};

use frame::{Decoder, FrameError, FrameRef, MAX_FRAME_LEN, Mode, PacketRef};
use message::{
    DecodedMessage, MAX_PAYLOAD_LEN, MessageError, MessageRef, Parameter, ParameterValueRef,
    Parameters, Request, SendMode, ValueRef,
};
pub use propar_core::{DEFAULT_BAUD_RATE, LOCAL_NODE, frame, message};

/// An I/O, framing, protocol, or transaction failure.
#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error<E> {
    Io(E),
    Frame(FrameError),
    Message(MessageError),
    Status {
        code: u8,
        position: u8,
    },
    Interface(u8),
    Timeout,
    /// A previous exchange was interrupted. Resynchronize the transport before reuse.
    Desynchronized,
    UnexpectedReply,
    UnexpectedEof,
    WriteZero,
}

impl<E: fmt::Debug> fmt::Display for Error<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "transport I/O error: {error:?}"),
            Self::Frame(error) => write!(f, "invalid ProPar frame: {error}"),
            Self::Message(error) => write!(f, "invalid ProPar message: {error}"),
            Self::Status { code, position } => {
                write!(f, "node returned status {code} at byte {position}")
            }
            Self::Interface(code) => write!(f, "interface returned error {code}"),
            Self::Timeout => f.write_str("ProPar transaction timed out"),
            Self::Desynchronized => f.write_str("resynchronize the transport before reuse"),
            Self::UnexpectedReply => f.write_str("unexpected ProPar reply"),
            Self::UnexpectedEof => f.write_str("transport ended before a complete reply"),
            Self::WriteZero => f.write_str("transport wrote zero bytes"),
        }
    }
}

impl<E: fmt::Debug> core::error::Error for Error<E> {}

impl<E> From<FrameError> for Error<E> {
    fn from(error: FrameError) -> Self {
        Self::Frame(error)
    }
}

impl<E> From<MessageError> for Error<E> {
    fn from(error: MessageError) -> Self {
        Self::Message(error)
    }
}

/// A single-flight async master with fixed storage and borrowed response values.
///
/// `S` implements the embedded async byte-stream traits. `D` supplies an async
/// delay that can safely be dropped and reused: a completed transaction cancels
/// its timer. No particular executor is required.
///
/// Once transmission begins, a timeout, cancellation, or incomplete I/O leaves
/// the master desynchronized in either wire mode. The stream traits do not
/// guarantee cancellation safety. Recover the stream with [`Self::into_inner`],
/// resynchronize it and the peer, and construct a new master before sending again.
pub struct Master<S, D> {
    session: Session<S>,
    delay: D,
    timeout_ms: u32,
}

impl<S, D> Master<S, D> {
    /// Wrap an already configured UART or other byte stream. Timeout defaults to 500 ms.
    pub fn new(stream: S, delay: D, mode: Mode) -> Self {
        Self {
            session: Session {
                stream,
                mode,
                decoder: Decoder::new(mode),
                sequence: 0,
                in_flight: false,
                input: [0; 64],
                start: 0,
                end: 0,
            },
            delay,
            timeout_ms: 500,
        }
    }

    /// Set the timeout for the entire exchange, including write and flush.
    /// Zero expires before any I/O. Actual timing follows the supplied HAL delay.
    pub fn set_timeout_ms(&mut self, timeout_ms: u32) {
        self.timeout_ms = timeout_ms;
    }

    /// Access the stream for configuration. Direct I/O bypasses protocol tracking.
    pub fn get_mut(&mut self) -> &mut S {
        &mut self.session.stream
    }

    /// Recover the stream and timer. Any unread bytes buffered by the master are lost.
    pub fn into_inner(self) -> (S, D) {
        (self.session.stream, self.delay)
    }
}

impl<S: Read + Write, D: DelayNs> Master<S, D> {
    /// Read one raw parameter. Byte/string values borrow `response`, not the master.
    /// A [`message::MAX_PAYLOAD_LEN`]-byte buffer accommodates every legal reply.
    pub async fn read<'a>(
        &mut self,
        address: u8,
        parameter: Parameter,
        response: &'a mut [u8],
    ) -> Result<ValueRef<'a>, Error<S::Error>> {
        let mut values = self
            .read_many(address, &[Request::new(parameter)], response)
            .await?;
        Ok(values
            .next()
            .ok_or(Error::UnexpectedReply)??
            .into_value_for(parameter)?)
    }

    /// Read a chain, validating the response count, addresses, and types.
    ///
    /// Returned entries retain their unsigned wire types. Apply
    /// [`ParameterValueRef::into_value_for`] with the corresponding
    /// [`Request::response_parameter`] to interpret signed or floating-point data.
    /// Alternate response addresses in each request are supported.
    pub async fn read_many<'a>(
        &mut self,
        address: u8,
        requests: &[Request],
        response: &'a mut [u8],
    ) -> Result<Parameters<'a>, Error<S::Error>> {
        let len = self
            .exchange(address, MessageRef::Request(requests), response)
            .await?
            .ok_or(Error::UnexpectedReply)?;
        let DecodedMessage::Send {
            mode: SendMode::NoAck,
            parameters,
        } = DecodedMessage::decode(&response[..len])?
        else {
            return Err(Error::UnexpectedReply);
        };
        let mut checked = parameters.clone();
        for request in requests {
            checked
                .next()
                .ok_or(Error::UnexpectedReply)??
                .into_value_for(request.response_parameter())?;
        }
        if checked.next().is_some() {
            return Err(Error::UnexpectedReply);
        }
        Ok(parameters)
    }

    /// Write one raw parameter and wait for its acknowledgement.
    pub async fn write(
        &mut self,
        address: u8,
        parameter: Parameter,
        value: ValueRef<'_>,
    ) -> Result<(), Error<S::Error>> {
        self.write_many(
            address,
            &[ParameterValueRef::new(parameter, value)],
            SendMode::WithAck,
        )
        .await
    }

    /// Write a chain with acknowledgement, without acknowledgement, or as a broadcast.
    pub async fn write_many(
        &mut self,
        address: u8,
        parameters: &[ParameterValueRef<'_>],
        mode: SendMode,
    ) -> Result<(), Error<S::Error>> {
        let mut response = [0; MAX_PAYLOAD_LEN];
        match self
            .exchange(
                address,
                MessageRef::Send { mode, parameters },
                &mut response,
            )
            .await?
        {
            None => Ok(()),
            Some(len) => match DecodedMessage::decode(&response[..len])? {
                DecodedMessage::Status { code: 0, .. } => Ok(()),
                _ => Err(Error::UnexpectedReply),
            },
        }
    }

    async fn exchange(
        &mut self,
        address: u8,
        message: MessageRef<'_>,
        response: &mut [u8],
    ) -> Result<Option<usize>, Error<S::Error>> {
        if self.session.in_flight {
            return Err(Error::Desynchronized);
        }
        let expect_reply = !matches!(
            message,
            MessageRef::Send {
                mode: SendMode::NoAck | SendMode::Broadcast,
                ..
            }
        );
        let sequence = self.session.sequence.wrapping_add(1);
        let mut wire = [0; MAX_FRAME_LEN];
        let wire_len = {
            let mut payload = [0; MAX_PAYLOAD_LEN];
            let len = message.encode_for_master_into(&mut payload)?;
            FrameRef {
                sequence,
                address,
                payload: &payload[..len],
            }
            .encode_into(self.session.mode, &mut wire)?
        };
        with_timeout(
            &mut self.delay,
            self.timeout_ms,
            self.session
                .exchange(address, sequence, &wire[..wire_len], expect_reply, response),
        )
        .await
    }
}

struct Session<S> {
    stream: S,
    mode: Mode,
    decoder: Decoder,
    sequence: u8,
    in_flight: bool,
    input: [u8; 64],
    start: usize,
    end: usize,
}

impl<S: Read + Write> Session<S> {
    async fn exchange(
        &mut self,
        address: u8,
        sequence: u8,
        mut wire: &[u8],
        expect_reply: bool,
        response: &mut [u8],
    ) -> Result<Option<usize>, Error<S::Error>> {
        self.in_flight = true;
        self.sequence = sequence;
        while !wire.is_empty() {
            let len = self.stream.write(wire).await.map_err(Error::Io)?;
            if len == 0 {
                return Err(Error::WriteZero);
            }
            wire = &wire[len..];
        }
        self.stream.flush().await.map_err(Error::Io)?;
        if !expect_reply {
            self.in_flight = false;
            return Ok(None);
        }
        loop {
            if self.start == self.end {
                self.end = self.stream.read(&mut self.input).await.map_err(Error::Io)?;
                self.start = 0;
                if self.end == 0 {
                    return Err(Error::UnexpectedEof);
                }
                // Bound work per poll, even if the UART continuously yields noise
                // or unrelated packets without ever returning Pending itself.
                cooperate().await;
            }
            let byte = self.input[self.start];
            self.start += 1;
            let Some(packet) = self.decoder.push(byte)? else {
                continue;
            };
            match packet {
                PacketRef::InterfaceError(code) => {
                    self.in_flight = false;
                    return Err(Error::Interface(code));
                }
                PacketRef::BinaryError {
                    sequence: seq,
                    address: node,
                    code,
                } if seq == sequence && node == address => {
                    self.in_flight = false;
                    return Err(Error::Interface(code));
                }
                PacketRef::Frame(frame)
                    if frame.payload.len() > 1 && frame.payload[0] == SendMode::Broadcast as u8 => {
                }
                PacketRef::Frame(frame)
                    if frame.address == address
                        && (self.mode == Mode::Ascii || frame.sequence == sequence) =>
                {
                    // A complete matched reply ends the exchange, including an
                    // error reply or a response that does not fit caller storage.
                    self.in_flight = false;
                    match DecodedMessage::decode(frame.payload)? {
                        DecodedMessage::Error(code) => return Err(Error::Interface(code)),
                        DecodedMessage::Status { code, position } if code != 0 => {
                            return Err(Error::Status { code, position });
                        }
                        _ => (),
                    }
                    let target = response
                        .get_mut(..frame.payload.len())
                        .ok_or(FrameError::BufferTooSmall)?;
                    target.copy_from_slice(frame.payload);
                    return Ok(Some(target.len()));
                }
                _ => (),
            }
        }
    }
}

async fn with_timeout<D, F, T, E>(
    delay: &mut D,
    timeout_ms: u32,
    operation: F,
) -> Result<T, Error<E>>
where
    D: DelayNs,
    F: Future<Output = Result<T, Error<E>>>,
{
    if timeout_ms == 0 {
        return Err(Error::Timeout);
    }
    let mut timer = pin!(delay.delay_ms(timeout_ms));
    let mut operation = pin!(operation);
    poll_fn(|cx| {
        if timer.as_mut().poll(cx).is_ready() {
            Poll::Ready(Err(Error::Timeout))
        } else {
            operation.as_mut().poll(cx)
        }
    })
    .await
}

async fn cooperate() {
    let mut yielded = false;
    poll_fn(|cx| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;
}
