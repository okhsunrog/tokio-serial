//! A unified [`Stream`] and [`Sink`] interface to an underlying `SerialStream`, using
//! the `Encoder` and `Decoder` traits to encode and decode frames.
use super::SerialStream;

use tokio_util::codec::{Decoder, Encoder};

use futures_core::Stream;
use futures_sink::Sink;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use bytes::{BufMut, BytesMut};
use std::pin::Pin;
use std::task::{ready, Context, Poll};
use std::{io, mem::MaybeUninit};

/// A unified [`Stream`] and [`Sink`] interface to an underlying `SerialStream`, using
/// the `Encoder` and `Decoder` traits to encode and decode frames.
///
/// Raw serial ports work with bytes, but higher-level code usually wants to
/// batch these into meaningful chunks, called "frames". This method layers
/// framing on top of this socket by using the `Encoder` and `Decoder` traits to
/// handle encoding and decoding of messages frames. Note that the incoming and
/// outgoing frame types may be distinct.
///
/// This function returns a *single* object that is both [`Stream`] and [`Sink`];
/// grouping this into a single object is often useful for layering things which
/// require both read and write access to the underlying object.
///
/// If you want to work more directly with the streams and sink, consider
/// calling [`split`] on the `SerialFramed` returned by this method, which will break
/// them into separate objects, allowing them to interact more easily.
///
/// [`Stream`]: futures_core::Stream
/// [`Sink`]: futures_sink::Sink
/// [`split`]: https://docs.rs/futures/0.3/futures/stream/trait.StreamExt.html#method.split
#[must_use = "sinks do nothing unless polled"]
#[derive(Debug)]
pub struct SerialFramed<C> {
    port: SerialStream,
    codec: C,
    rd: BytesMut,
    wr: BytesMut,
    flushed: bool,
    is_readable: bool,
}

const INITIAL_RD_CAPACITY: usize = 64 * 1024;
const INITIAL_WR_CAPACITY: usize = 8 * 1024;

impl<C: Decoder + Unpin> Stream for SerialFramed<C> {
    type Item = Result<C::Item, C::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let pin = self.get_mut();

        pin.rd.reserve(INITIAL_RD_CAPACITY);

        loop {
            // Are there still bytes left in the read buffer to decode?
            if pin.is_readable {
                if let Some(frame) = pin.codec.decode_eof(&mut pin.rd)? {
                    return Poll::Ready(Some(Ok(frame)));
                }

                // if this line has been reached then decode has returned `None`.
                pin.is_readable = false;
                pin.rd.clear();
            }

            // We're out of data. Try and fetch more data to decode
            unsafe {
                // Convert `&mut [MaybeUnit<u8>]` to `&mut [u8]` because we will be
                // writing to it via `poll_recv_from` and therefore initializing the memory.
                let buf = &mut *(pin.rd.chunk_mut() as *mut _ as *mut [MaybeUninit<u8>]);
                let mut read = ReadBuf::uninit(buf);
                let ptr = read.filled().as_ptr();
                ready!(Pin::new(&mut pin.port).poll_read(cx, &mut read))?;

                assert_eq!(ptr, read.filled().as_ptr());
                pin.rd.advance_mut(read.filled().len());
            };

            pin.is_readable = true;
        }
    }
}

impl<I, C: Encoder<I> + Unpin> Sink<I> for SerialFramed<C> {
    type Error = C::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if !self.flushed {
            match self.poll_flush(cx)? {
                Poll::Ready(()) => {}
                Poll::Pending => return Poll::Pending,
            }
        }

        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: I) -> Result<(), Self::Error> {
        let pin = self.get_mut();

        pin.codec.encode(item, &mut pin.wr)?;
        pin.flushed = false;

        Ok(())
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.flushed {
            return Poll::Ready(Ok(()));
        }

        let Self {
            ref mut port,
            ref mut wr,
            ..
        } = *self;

        let pinned = Pin::new(port);
        let n = ready!(pinned.poll_write(cx, &wr))?;

        let wrote_all = n == self.wr.len();
        self.wr.clear();
        self.flushed = true;

        let res = if wrote_all {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::Other,
                "failed to write entire datagram to socket",
            )
            .into())
        };

        Poll::Ready(res)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        ready!(self.poll_flush(cx))?;
        Poll::Ready(Ok(()))
    }
}

impl<C> SerialFramed<C> {
    /// Create a new `SerialFramed` backed by the given socket and codec.
    ///
    /// See struct level documentation for more details.
    #[allow(dead_code)]
    pub fn new(port: SerialStream, codec: C) -> SerialFramed<C> {
        Self {
            port,
            codec,
            rd: BytesMut::with_capacity(INITIAL_RD_CAPACITY),
            wr: BytesMut::with_capacity(INITIAL_WR_CAPACITY),
            flushed: true,
            is_readable: false,
        }
    }

    /// Returns a reference to the underlying I/O stream wrapped by `Framed`.
    ///
    /// # Note
    ///
    /// Care should be taken to not tamper with the underlying stream of data
    /// coming in as it may corrupt the stream of frames otherwise being worked
    /// with.
    #[allow(dead_code)]
    pub fn get_ref(&self) -> &SerialStream {
        &self.port
    }

    /// Returns a mutable reference to the underlying I/O stream wrapped by
    /// `Framed`.
    ///
    /// # Note
    ///
    /// Care should be taken to not tamper with the underlying stream of data
    /// coming in as it may corrupt the stream of frames otherwise being worked
    /// with.
    #[allow(dead_code)]
    pub fn get_mut(&mut self) -> &mut SerialStream {
        &mut self.port
    }

    /// Consumes the `Framed`, returning its underlying I/O stream.
    #[allow(dead_code)]
    pub fn into_inner(self) -> SerialStream {
        self.port
    }

    /// Returns a reference to the underlying codec wrapped by
    /// `Framed`.
    ///
    /// Note that care should be taken to not tamper with the underlying codec
    /// as it may corrupt the stream of frames otherwise being worked with.
    #[allow(dead_code)]
    pub fn codec(&self) -> &C {
        &self.codec
    }

    /// Returns a mutable reference to the underlying codec wrapped by
    /// `SerialFramed`.
    ///
    /// Note that care should be taken to not tamper with the underlying codec
    /// as it may corrupt the stream of frames otherwise being worked with.
    #[allow(dead_code)]
    pub fn codec_mut(&mut self) -> &mut C {
        &mut self.codec
    }

    /// Returns a reference to the read buffer.
    #[allow(dead_code)]
    pub fn read_buffer(&self) -> &BytesMut {
        &self.rd
    }

    /// Returns a mutable reference to the read buffer.
    #[allow(dead_code)]
    pub fn read_buffer_mut(&mut self) -> &mut BytesMut {
        &mut self.rd
    }
}
