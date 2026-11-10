use std::{fmt, io, time::Duration};

use crate::frame::{FrameError, Mode};
use crate::message::{Message, MessageError, ParameterValue, SendMode};

const DEFAULT_TIMEOUT: Duration = Duration::from_millis(500);

/// Failure to send a transaction or obtain its expected reply.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    Io(io::Error),
    Frame(FrameError),
    Message(MessageError),
    /// A status response from the node, including the offending byte position.
    Status {
        code: u8,
        position: u8,
    },
    /// An error reported by the RS232/FLOW-BUS interface.
    Interface(u8),
    Timeout,
    /// An interrupted write, or an outstanding ASCII reply, makes reuse ambiguous.
    Desynchronized,
    UnexpectedReply(&'static str),
    InvalidChannel(u8),
    UnknownParameter(u16),
    InvalidWinkDuration(u8),
    InvalidTimeout,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "transport I/O error: {e}"),
            Self::Frame(e) => write!(f, "invalid ProPar frame: {e}"),
            Self::Message(e) => write!(f, "invalid ProPar message: {e}"),
            Self::Status { code, position } => {
                let description =
                    crate::message::status_description(*code).unwrap_or("unknown status");
                write!(
                    f,
                    "node returned status {code} ({description}) at byte {position}"
                )
            }
            Self::Interface(code) => write!(f, "interface returned error {code}"),
            Self::Timeout => f.write_str("ProPar transaction timed out"),
            Self::Desynchronized => f.write_str(
                "transaction interrupted; reopen or resynchronize the transport before reuse",
            ),
            Self::UnexpectedReply(reason) => write!(f, "unexpected ProPar reply: {reason}"),
            Self::InvalidChannel(n) => write!(f, "channel {n} is outside 1..=16"),
            Self::UnknownParameter(n) => write!(f, "unknown FlowDDE parameter {n}"),
            Self::InvalidWinkDuration(n) => write!(f, "wink duration {n} is outside 1..=9 seconds"),
            Self::InvalidTimeout => f.write_str("timeout exceeds the representable deadline"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Frame(e) => Some(e),
            Self::Message(e) => Some(e),
            _ => None,
        }
    }
}
impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        if e.kind() == io::ErrorKind::TimedOut {
            Self::Timeout
        } else {
            Self::Io(e)
        }
    }
}
impl From<FrameError> for Error {
    fn from(e: FrameError) -> Self {
        Self::Frame(e)
    }
}
impl From<MessageError> for Error {
    fn from(e: MessageError) -> Self {
        Self::Message(e)
    }
}

/// One unsolicited parameter broadcast. Known wire values are interpreted using the catalog.
#[derive(Debug, Clone, PartialEq)]
pub struct Broadcast {
    pub address: u8,
    pub parameters: Vec<ParameterValue>,
}

impl Broadcast {
    fn from_message(address: u8, message: Message) -> Result<Self, Error> {
        let Message::Send {
            mode: SendMode::Broadcast,
            mut parameters,
        } = message
        else {
            return Err(Error::UnexpectedReply("expected broadcast"));
        };
        for entry in &mut parameters {
            let p = entry.parameter;
            for def in crate::catalog::by_address(p.process, p.number) {
                if let Ok(value) = entry.clone().into_value_for(def.parameter) {
                    *entry = ParameterValue::new(def.parameter, value);
                    break;
                }
            }
        }
        Ok(Self {
            address,
            parameters,
        })
    }
}

/// Information returned by FLOW-BUS node discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub address: u8,
    pub identification: Vec<u8>,
    pub device_type: String,
    pub channels: u8,
}

impl Node {
    /// The identification field following its three-byte device prefix.
    pub fn serial_number(&self) -> &[u8] {
        self.identification.get(3..).unwrap_or_default()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Sending,
    Waiting,
}

/// Both transports use this same transaction logic. Only the stream operations and
/// clock in the transport adapter differ between the generated modules.
#[maybe_async_cfg::maybe(
    idents(interface(mod, sync = "blocking", async = "asynchronous")),
    sync(feature = "blocking"),
    async(feature = "async")
)]
pub mod interface {
    use super::*;
    #[maybe_async_cfg::only_if(async)]
    use crate::async_io as transport;
    #[maybe_async_cfg::only_if(sync)]
    use crate::blocking_io as transport;
    use crate::frame::{Decoder, Frame, Packet};
    use crate::message::{Parameter, Request, Value};
    use crate::{LOCAL_NODE, catalog};
    use transport::Instant;
    pub use transport::Stream;

    /// A single-flight ProPar master. Share one master across nodes on the same bus.
    pub struct Master<S> {
        stream: S,
        pub(crate) mode: Mode,
        decoder: Decoder,
        sequence: u8,
        phase: Phase,
        timeout: Duration,
        input: [u8; 256],
        start: usize,
        end: usize,
        broadcast_handler: Option<Box<dyn FnMut(Broadcast) + Send>>,
    }

    impl<S> Master<S> {
        /// Wrap an open transport. For blocking I/O, configure a finite stream read/write timeout.
        pub fn new(stream: S, mode: Mode) -> Self {
            Self {
                stream,
                mode,
                decoder: Decoder::new(mode),
                sequence: 0,
                phase: Phase::Idle,
                timeout: DEFAULT_TIMEOUT,
                input: [0; 256],
                start: 0,
                end: 0,
                broadcast_handler: None,
            }
        }

        /// Access the stream to configure transport settings such as baud rate. Direct I/O
        /// here bypasses framing and transaction tracking.
        pub fn get_mut(&mut self) -> &mut S {
            &mut self.stream
        }

        /// Consume the master and recover its stream. The master may have read ahead.
        pub fn into_inner(self) -> S {
            self.stream
        }

        /// Set the transaction deadline. The default is 500 ms.
        pub fn set_timeout(&mut self, timeout: Duration) {
            self.timeout = timeout;
        }

        /// Receive broadcasts encountered while waiting for a transaction. Runs on the caller's task.
        pub fn on_broadcast(&mut self, handler: impl FnMut(Broadcast) + Send + 'static) {
            self.broadcast_handler = Some(Box::new(handler));
        }

        /// Borrow this master for an instrument on the given node and channel (1–16).
        pub fn instrument(&mut self, address: u8, channel: u8) -> Result<Instrument<'_, S>, Error> {
            if !(1..=16).contains(&channel) {
                return Err(Error::InvalidChannel(channel));
            }
            Ok(Instrument {
                master: self,
                address,
                channel,
            })
        }

        /// Borrow the directly connected instrument on channel 1.
        pub fn local(&mut self) -> Instrument<'_, S> {
            Instrument {
                master: self,
                address: LOCAL_NODE,
                channel: 1,
            }
        }
    }

    impl<S: Stream> Master<S> {
        async fn packet(&mut self, deadline: Instant) -> Result<Packet, Error> {
            loop {
                if Instant::now() >= deadline {
                    return Err(Error::Timeout);
                }
                if self.start == self.end {
                    self.end = transport::read(&mut self.stream, &mut self.input, deadline).await?;
                    self.start = 0;
                    if self.end == 0 {
                        return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into());
                    }
                }
                let byte = self.input[self.start];
                self.start += 1;
                if let Some(packet) = self.decoder.push(byte)? {
                    return Ok(packet.to_owned());
                }
            }
        }

        fn ready(&self) -> Result<(), Error> {
            match (self.phase, self.mode) {
                (Phase::Idle, _) | (Phase::Waiting, Mode::Binary) => Ok(()),
                _ => Err(Error::Desynchronized),
            }
        }

        async fn exchange(
            &mut self,
            address: u8,
            message: Message,
            timeout: Duration,
        ) -> Result<Option<Message>, Error> {
            self.ready()?;
            let expect_reply = !matches!(
                message,
                Message::Send {
                    mode: SendMode::NoAck | SendMode::Broadcast,
                    ..
                }
            );
            let sequence = self.sequence.wrapping_add(1);
            let frame = Frame {
                sequence,
                address,
                payload: message.encode_for_master()?,
            };
            let bytes = frame.encode(self.mode)?;
            let deadline = Instant::now()
                .checked_add(timeout)
                .ok_or(Error::InvalidTimeout)?;
            if self.phase == Phase::Waiting {
                // A previous binary reply may have timed out midway through a frame.
                self.decoder.reset();
            }
            self.sequence = sequence;
            self.phase = Phase::Sending;
            transport::write(&mut self.stream, &bytes, deadline).await?;
            if !expect_reply {
                self.phase = Phase::Idle;
                return Ok(None);
            }
            self.phase = Phase::Waiting;
            loop {
                let reply = match self.packet(deadline).await? {
                    Packet::InterfaceError(code) => Err(Error::Interface(code)),
                    Packet::BinaryError {
                        sequence: seq,
                        address: node,
                        code,
                    } if seq == sequence && node == address => Err(Error::Interface(code)),
                    Packet::BinaryError { .. } => continue,
                    Packet::Frame(frame)
                        if frame.payload.len() > 1
                            && frame.payload[0] == SendMode::Broadcast as u8 =>
                    {
                        if let Some(handler) = &mut self.broadcast_handler {
                            handler(Broadcast::from_message(
                                frame.address,
                                Message::decode(&frame.payload)?,
                            )?);
                        }
                        continue;
                    }
                    Packet::Frame(frame)
                        if frame.address == address
                            && (self.mode == Mode::Ascii || frame.sequence == sequence) =>
                    {
                        match Message::decode(&frame.payload)? {
                            Message::Error(code) => Err(Error::Interface(code)),
                            Message::Status { code, position } if code != 0 => {
                                Err(Error::Status { code, position })
                            }
                            other => Ok(Some(other)),
                        }
                    }
                    Packet::Frame(_) => continue,
                };
                self.phase = Phase::Idle;
                return reply;
            }
        }

        /// Wait for the next unsolicited broadcast using the configured timeout.
        pub async fn next_broadcast(&mut self) -> Result<Broadcast, Error> {
            self.ready()?;
            let deadline = Instant::now()
                .checked_add(self.timeout)
                .ok_or(Error::InvalidTimeout)?;
            loop {
                if let Packet::Frame(frame) = self.packet(deadline).await? {
                    if frame.payload.len() > 1 && frame.payload[0] == SendMode::Broadcast as u8 {
                        return Broadcast::from_message(
                            frame.address,
                            Message::decode(&frame.payload)?,
                        );
                    }
                }
            }
        }

        /// Read a raw process/parameter address from a node.
        pub async fn read(&mut self, address: u8, parameter: Parameter) -> Result<Value, Error> {
            let mut values = self.read_many(address, &[parameter]).await?;
            Ok(values.remove(0))
        }

        /// Read all parameters in one chain. Values follow the order of the request.
        pub async fn read_many(
            &mut self,
            address: u8,
            parameters: &[Parameter],
        ) -> Result<Vec<Value>, Error> {
            self.read_with_timeout(address, parameters, self.timeout)
                .await
        }

        pub(crate) async fn read_with_timeout(
            &mut self,
            address: u8,
            parameters: &[Parameter],
            timeout: Duration,
        ) -> Result<Vec<Value>, Error> {
            let requests = parameters.iter().copied().map(Request::new).collect();
            let reply = self
                .exchange(address, Message::Request(requests), timeout)
                .await?;
            let Some(Message::Send {
                mode: SendMode::NoAck,
                parameters: received,
            }) = reply
            else {
                return Err(Error::UnexpectedReply("expected parameter values"));
            };
            if received.len() != parameters.len() {
                return Err(Error::UnexpectedReply("parameter count differs"));
            }
            received
                .into_iter()
                .zip(parameters)
                .map(|(entry, &expected)| Ok(entry.into_value_for(expected)?))
                .collect()
        }

        /// Write a single parameter and wait for the node's acknowledgement.
        pub async fn write(
            &mut self,
            address: u8,
            parameter: Parameter,
            value: Value,
        ) -> Result<(), Error> {
            self.write_many(
                address,
                &[ParameterValue::new(parameter, value)],
                SendMode::WithAck,
            )
            .await
        }

        /// Write a chain with acknowledgement, without acknowledgement, or as a broadcast.
        pub async fn write_many(
            &mut self,
            address: u8,
            parameters: &[ParameterValue],
            mode: SendMode,
        ) -> Result<(), Error> {
            let reply = self
                .exchange(
                    address,
                    Message::Send {
                        mode,
                        parameters: parameters.to_vec(),
                    },
                    self.timeout,
                )
                .await?;
            match reply {
                None | Some(Message::Status { code: 0, .. }) => Ok(()),
                _ => Err(Error::UnexpectedReply("expected a write acknowledgement")),
            }
        }
    }

    /// A borrowed view of one node/channel. Raw addresses are available directly on the master.
    pub struct Instrument<'a, S> {
        master: &'a mut Master<S>,
        address: u8,
        channel: u8,
    }

    impl<S: Stream> Instrument<'_, S> {
        fn resolve(&self, dde: u16) -> Result<Parameter, Error> {
            let def = catalog::by_dde(dde).ok_or(Error::UnknownParameter(dde))?;
            catalog::for_channel(def.parameter, self.channel)
                .ok_or(Error::InvalidChannel(self.channel))
        }

        /// Read by FlowDDE number, applying the instrument's channel.
        pub async fn read(&mut self, dde: u16) -> Result<Value, Error> {
            self.master.read(self.address, self.resolve(dde)?).await
        }
        /// Write by FlowDDE number and wait for an acknowledgement.
        pub async fn write(&mut self, dde: u16, value: Value) -> Result<(), Error> {
            self.master
                .write(self.address, self.resolve(dde)?, value)
                .await
        }
        /// Read multiple FlowDDE numbers in a single request.
        pub async fn read_many(&mut self, ddes: &[u16]) -> Result<Vec<Value>, Error> {
            let parameters = ddes
                .iter()
                .map(|&dde| self.resolve(dde))
                .collect::<Result<Vec<_>, _>>()?;
            self.master.read_many(self.address, &parameters).await
        }
        /// Write multiple FlowDDE parameters in a single request.
        pub async fn write_many(
            &mut self,
            values: &[(u16, Value)],
            mode: SendMode,
        ) -> Result<(), Error> {
            let parameters = values
                .iter()
                .map(|(dde, value)| Ok(ParameterValue::new(self.resolve(*dde)?, value.clone())))
                .collect::<Result<Vec<_>, Error>>()?;
            self.master
                .write_many(self.address, &parameters, mode)
                .await
        }
        /// Measure in raw scaled units (32000 = 100% of full scale).
        pub async fn measure(&mut self) -> Result<i32, Error> {
            match self.read(8).await? {
                Value::Bht(v) => Ok(v),
                _ => Err(Error::UnexpectedReply("expected measure")),
            }
        }
        /// Setpoint in raw scaled units (32000 = 100% of full scale).
        pub async fn setpoint(&mut self) -> Result<u16, Error> {
            match self.read(9).await? {
                Value::U16(v) => Ok(v),
                _ => Err(Error::UnexpectedReply("expected setpoint")),
            }
        }
        /// Write the raw scaled setpoint.
        pub async fn set_setpoint(&mut self, value: u16) -> Result<(), Error> {
            self.write(9, Value::U16(value)).await
        }
        /// Read the instrument identification bytes.
        pub async fn id(&mut self) -> Result<Vec<u8>, Error> {
            match self.read(1).await? {
                Value::Bytes(v) => Ok(v),
                _ => Err(Error::UnexpectedReply("expected identification")),
            }
        }
        /// Flash the instrument LEDs for 1–9 seconds.
        pub async fn wink(&mut self, seconds: u8) -> Result<(), Error> {
            if !(1..=9).contains(&seconds) {
                return Err(Error::InvalidWinkDuration(seconds));
            }
            self.write(1, Value::Bytes(vec![b'0' + seconds])).await
        }
    }
}
