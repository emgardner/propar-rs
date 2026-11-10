use crate::Error;
use std::io::{self, Read, Write};
pub(crate) use std::time::Instant;

/// A standard I/O byte stream suitable for the blocking master.
pub trait Stream: Read + Write {}
impl<T: Read + Write> Stream for T {}

pub(crate) fn read(
    stream: &mut impl Stream,
    buffer: &mut [u8],
    deadline: Instant,
) -> Result<usize, Error> {
    while Instant::now() < deadline {
        match stream.read(buffer) {
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            result => return Ok(result?),
        }
    }
    Err(Error::Timeout)
}

pub(crate) fn write(
    stream: &mut impl Stream,
    bytes: &[u8],
    deadline: Instant,
) -> Result<(), Error> {
    if Instant::now() >= deadline {
        return Err(Error::Timeout);
    }
    stream.write_all(bytes)?;
    stream.flush()?;
    Ok(())
}

#[cfg(feature = "serial")]
impl crate::blocking::Master<Box<dyn serialport::SerialPort>> {
    /// Open an 8N1 serial port in binary mode at 38400 baud.
    pub fn open(path: &str) -> Result<Self, Error> {
        Self::open_with(path, crate::DEFAULT_BAUD_RATE, crate::frame::Mode::Binary)
    }

    /// Open an 8N1 serial port with a specific baud rate and framing mode.
    pub fn open_with(path: &str, baud_rate: u32, mode: crate::frame::Mode) -> Result<Self, Error> {
        // Poll often enough to service the shorter node-discovery deadline.
        let port = serialport::new(path, baud_rate)
            .timeout(std::time::Duration::from_millis(50))
            .open()
            .map_err(io::Error::from)?;
        Ok(Self::new(port, mode))
    }
}
