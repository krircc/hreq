use crate::bw::BandwidthMonitor;
use crate::uninit::UninitBuf;
use crate::AsyncRead;
use bytes::Bytes;
use futures_io::AsyncBufRead;
use futures_util::future::poll_fn;
use futures_util::ready;
use h2::RecvStream as H2RecvStream;
use hreq_h1::RecvStream as H1RecvStream;
use std::fmt;
use std::io;
use std::io::Read;
use std::pin::Pin;
use std::task::{Context, Poll};

#[cfg(feature = "gzip")]
use async_compression::futures::bufread::{GzipDecoder, GzipEncoder};

#[cfg(feature = "gzip")]
use futures_util::io::BufReader;

const START_BUF_SIZE: usize = 16_384;
const MAX_BUF_SIZE: usize = 2 * 1024 * 1024;
const MAX_PREBUFFER: usize = 256 * 1024;

#[allow(clippy::large_enum_variant)]
pub(crate) enum BodyCodec {
    Deferred(Option<BodyReader>),
    Pass(BodyReader),
    #[cfg(feature = "gzip")]
    GzipDecoder(BufReader<GzipDecoder<BodyReader>>),
    #[cfg(feature = "gzip")]
    GzipEncoder(BufReader<GzipEncoder<BodyReader>>),
}

impl BodyCodec {
    pub fn deferred(bimp: BodyImpl, prebuffer: bool) -> Self {
        let reader = BodyReader::new(bimp, prebuffer);
        BodyCodec::Deferred(Some(reader))
    }

    #[cfg(feature = "server")]
    pub fn into_deferred(self) -> Self {
        let reader = self.into_inner();
        BodyCodec::Deferred(Some(reader))
    }

    #[cfg(feature = "server")]
    fn into_inner(self) -> BodyReader {
        match self {
            BodyCodec::Deferred(_) => panic!("into_inner() on Deferred"),
            BodyCodec::Pass(b) => b,
            #[cfg(feature = "gzip")]
            BodyCodec::GzipDecoder(z) => z.into_inner().into_inner(),
            #[cfg(feature = "gzip")]
            BodyCodec::GzipEncoder(z) => z.into_inner().into_inner(),
        }
    }

    pub fn from_encoding(reader: BodyReader, encoding: Option<&str>, is_incoming: bool) -> Self {
        trace!("Body codec from encoding: {:?}", encoding);
        match (encoding, is_incoming) {
            (None, _) => BodyCodec::Pass(reader),
            #[cfg(feature = "gzip")]
            (Some("gzip"), true) => {
                BodyCodec::GzipDecoder(BufReader::new(GzipDecoder::new(reader)))
            }
            #[cfg(feature = "gzip")]
            (Some("gzip"), false) => {
                BodyCodec::GzipEncoder(BufReader::new(GzipEncoder::new(reader)))
            }
            _ => {
                warn!("Unknown content-encoding: {:?}", encoding);
                BodyCodec::Pass(reader)
            }
        }
    }

    fn reader_mut(&mut self) -> Option<&mut BodyReader> {
        match self {
            BodyCodec::Deferred(r) => r.as_mut(),
            BodyCodec::Pass(r) => Some(r),
            #[cfg(feature = "gzip")]
            BodyCodec::GzipDecoder(r) => Some(r.get_mut().get_mut()),
            #[cfg(feature = "gzip")]
            BodyCodec::GzipEncoder(r) => Some(r.get_mut().get_mut()),
        }
    }

    pub fn affects_content_size(&self) -> bool {
        match self {
            BodyCodec::Deferred(_) => false,
            BodyCodec::Pass(_) => false,
            #[cfg(feature = "gzip")]
            BodyCodec::GzipDecoder(_) => true,
            #[cfg(feature = "gzip")]
            BodyCodec::GzipEncoder(_) => true,
        }
    }

    /// Attempt to fully read the underlying content into memory.
    ///
    /// Returns the amount read if the entire contents was read.
    pub async fn attempt_prebuffer(&mut self) -> io::Result<Option<usize>> {
        if let Some(rdr) = self.reader_mut() {
            if let Some(amt) = rdr.attempt_prebuffer().await? {
                // content is fully buffered
                return Ok(Some(amt));
            }
        }
        Ok(None)
    }
}

pub struct BodyReader {
    imp: BodyImpl,
    prebuffer_to: usize,
    buffer: UninitBuf,
    consumed: usize,
    h2_leftover_bytes: Option<H2BytesReader>,
    is_finished: bool,
    bw: Option<BandwidthMonitor>,
}

pub(crate) enum BodyImpl {
    RequestEmpty,
    RequestAsyncRead(Box<dyn AsyncRead + Unpin + Send + Sync>),
    RequestRead(Box<dyn io::Read + Send + Sync>),
    Http1(H1RecvStream),
    Http2(H2RecvStream),
}

impl BodyReader {
    fn new(imp: BodyImpl, prebuffer: bool) -> Self {
        BodyReader {
            imp,
            prebuffer_to: if prebuffer { MAX_PREBUFFER } else { 0 },
            h2_leftover_bytes: None,
            buffer: UninitBuf::with_capacity(START_BUF_SIZE, MAX_BUF_SIZE),
            consumed: 0,
            is_finished: false,
            bw: None,
        }
    }

    pub(crate) fn set_bw_monitor(&mut self, bw: Option<BandwidthMonitor>) {
        self.bw = bw;
    }

    /// Fills the internal buffer from the underlying reader. If prebuffer_to is > 0 will
    /// try to fill to that level.
    ///
    /// Returns the number of bytes read if the underlying reader is read to end, which
    /// means we got all contents in memory.
    async fn attempt_prebuffer(&mut self) -> io::Result<Option<usize>> {
        poll_fn(|cx| Pin::new(&mut *self).poll_refill_buf(cx)).await?;

        Ok(if self.is_finished {
            Some(self.buffer.len())
        } else {
            None
        })
        .into()
    }

    fn poll_read_underlying(
        &mut self,
        cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        if self.is_finished {
            return Ok(0).into();
        }

        // h2 streams might have leftovers to use up before reading any more.
        if let Some(br) = &mut self.h2_leftover_bytes {
            let amt = br.read(buf)?;

            if br.len() == 0 {
                self.h2_leftover_bytes = None;
            }

            return Ok(amt).into();
        }

        let amount = match &mut self.imp {
            BodyImpl::RequestEmpty => 0,
            BodyImpl::RequestAsyncRead(reader) => ready!(Pin::new(reader).poll_read(cx, buf))?,
            BodyImpl::RequestRead(reader) => match reader.read(buf) {
                Ok(v) => v,
                Err(e) => {
                    if e.kind() == io::ErrorKind::WouldBlock {
                        panic!("Body::from_sync_read() failed with ErrorKind::WouldBlock. Use Body::from_async_read()");
                    }
                    return Err(e).into();
                }
            },
            BodyImpl::Http1(recv) => ready!(Pin::new(recv).poll_read(cx, buf))?,
            BodyImpl::Http2(recv) => {
                if let Some(data) = ready!(recv.poll_data(cx)) {
                    let data = data.map_err(|e| {
                        let other = format!("Other h2 error (poll_data): {}", e);
                        e.into_io()
                            .unwrap_or_else(|| io::Error::new(io::ErrorKind::Other, other))
                    })?;

                    recv.flow_control()
                        .release_capacity(data.len())
                        .map_err(|e| {
                            let other = format!("Other h2 error (release_capacity): {}", e);
                            e.into_io()
                                .unwrap_or_else(|| io::Error::new(io::ErrorKind::Other, other))
                        })?;

                    let mut br = H2BytesReader(data, 0);
                    let amt = br.read(buf)?;

                    // if any is left, leave it for later.
                    if br.len() > 0 {
                        self.h2_leftover_bytes = Some(br);
                    }

                    amt
                } else {
                    0
                }
            }
        };

        if amount == 0 {
            self.is_finished = true;
        }

        if let Some(bw) = &self.bw {
            bw.append_read_bytes(amount);
        }

        Ok(amount).into()
    }

    fn poll_refill_buf(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.unconsumed_len() > 0 {
            return Ok(()).into();
        }

        // reading resets the consume index.
        self.consumed = 0;
        self.buffer.clear();

        loop {
            let buffer_len = self.buffer.len();

            let read_enough =
                // when prebuffering, we are reading until the buffer len() is as much as allowed.
                self.prebuffer_to > 0 && buffer_len == self.prebuffer_to
                // when not prebuffering, any content is enough.
                || self.prebuffer_to == 0 && self.buffer.len() > 0;

            if self.is_finished || read_enough {
                // only first poll_fill_buf is prebuffering.
                self.prebuffer_to = 0;

                return Ok(()).into();
            }

            // this is safe cause self.poll_read_underlying is not touching self.buffer
            let ptr = &mut self.buffer as *mut UninitBuf;
            let buf = unsafe { &mut *ptr };

            ready!(buf.poll_delegate(|buf| self.poll_read_underlying(cx, buf)))?;
        }
    }

    fn unconsumed(&self) -> &[u8] {
        &self.buffer[self.consumed..]
    }

    fn unconsumed_len(&self) -> usize {
        self.buffer.len() - self.consumed
    }
}

impl AsyncBufRead for BodyReader {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<&[u8]>> {
        let this = self.get_mut();

        ready!(this.poll_refill_buf(cx))?;

        return Ok(this.unconsumed()).into();
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        let this = self.get_mut();

        this.consumed += amt;

        assert!(this.consumed <= this.buffer.len());
    }
}

impl AsyncRead for BodyReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();

        if this.unconsumed_len() == 0 {
            if this.is_finished {
                return Ok(0).into();
            } else {
                // read more bytes into the inner buffer
                ready!(this.poll_refill_buf(cx))?;
            }
        }

        let amount = this.unconsumed().read(buf)?;

        Pin::new(this).consume(amount);

        Ok(amount).into()
    }
}

impl AsyncRead for BodyCodec {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        match this {
            BodyCodec::Deferred(_) => panic!("poll_read on BodyCodec::Deferred"),
            BodyCodec::Pass(r) => Pin::new(r).poll_read(cx, buf),
            #[cfg(feature = "gzip")]
            BodyCodec::GzipDecoder(r) => Pin::new(r).poll_read(cx, buf),
            #[cfg(feature = "gzip")]
            BodyCodec::GzipEncoder(r) => Pin::new(r).poll_read(cx, buf),
        }
    }
}

impl AsyncBufRead for BodyCodec {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<&[u8]>> {
        match self.get_mut() {
            BodyCodec::Deferred(_) => panic!("poll_fill_buf on Deferred"),
            BodyCodec::Pass(r) => Pin::new(r).poll_fill_buf(cx),
            #[cfg(feature = "gzip")]
            BodyCodec::GzipDecoder(r) => Pin::new(r).poll_fill_buf(cx),
            #[cfg(feature = "gzip")]
            BodyCodec::GzipEncoder(r) => Pin::new(r).poll_fill_buf(cx),
        }
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        match self.get_mut() {
            BodyCodec::Deferred(_) => panic!("consume on Deferred"),
            BodyCodec::Pass(r) => Pin::new(r).consume(amt),
            #[cfg(feature = "gzip")]
            BodyCodec::GzipDecoder(r) => Pin::new(r).consume(amt),
            #[cfg(feature = "gzip")]
            BodyCodec::GzipEncoder(r) => Pin::new(r).consume(amt),
        }
    }
}
impl fmt::Debug for BodyCodec {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            BodyCodec::Deferred(_) => write!(f, "defer"),
            BodyCodec::Pass(_) => write!(f, "pass"),
            #[cfg(feature = "gzip")]
            BodyCodec::GzipDecoder(_) => write!(f, "gzip_dec"),
            #[cfg(feature = "gzip")]
            BodyCodec::GzipEncoder(_) => write!(f, "gzip_enc"),
        }
    }
}

impl fmt::Debug for BodyReader {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self.imp)
    }
}

impl fmt::Debug for BodyImpl {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            BodyImpl::RequestEmpty => write!(f, "empty"),
            BodyImpl::RequestAsyncRead(_) => write!(f, "async"),
            BodyImpl::RequestRead(_) => write!(f, "sync"),
            BodyImpl::Http1(_) => write!(f, "http1"),
            BodyImpl::Http2(_) => write!(f, "http2"),
        }
    }
}

/// Helper to deal with bytes::Bytes not being fully read.
struct H2BytesReader(Bytes, usize);

impl H2BytesReader {
    fn len(&self) -> usize {
        self.0.len() - self.1
    }
}

impl Read for H2BytesReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.len() == 0 {
            return Ok(0);
        }

        let amt = (&self.0[self.1..]).read(buf)?;
        self.1 += amt;

        Ok(amt)
    }
}
