use crate::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
pub(crate) use tokio::time::Instant;
use tokio::time::timeout_at;

/// A Tokio byte stream suitable for the asynchronous master.
pub trait Stream: AsyncRead + AsyncWrite + Unpin {}
impl<T: AsyncRead + AsyncWrite + Unpin> Stream for T {}

pub(crate) async fn read(
    stream: &mut impl Stream,
    buffer: &mut [u8],
    deadline: Instant,
) -> Result<usize, Error> {
    Ok(timeout_at(deadline, stream.read(buffer))
        .await
        .map_err(|_| Error::Timeout)??)
}

pub(crate) async fn write(
    stream: &mut impl Stream,
    bytes: &[u8],
    deadline: Instant,
) -> Result<(), Error> {
    timeout_at(deadline, async {
        stream.write_all(bytes).await?;
        stream.flush().await
    })
    .await
    .map_err(|_| Error::Timeout)??;
    Ok(())
}

#[cfg(feature = "async-serial")]
impl crate::asynchronous::Master<tokio_serial::SerialStream> {
    /// Open an 8N1 serial port in binary mode at 38400 baud inside a Tokio runtime.
    pub fn open(path: &str) -> Result<Self, Error> {
        Self::open_with(path, crate::DEFAULT_BAUD_RATE, crate::frame::Mode::Binary)
    }

    /// Open an 8N1 serial port with a specific baud rate and framing mode.
    pub fn open_with(path: &str, baud_rate: u32, mode: crate::frame::Mode) -> Result<Self, Error> {
        use tokio_serial::SerialPortBuilderExt;
        let port = tokio_serial::new(path, baud_rate)
            .open_native_async()
            .map_err(std::io::Error::from)?;
        Ok(Self::new(port, mode))
    }
}
