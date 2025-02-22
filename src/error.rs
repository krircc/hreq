use std::fmt;
use std::io;

#[cfg(feature = "server")]
use std::net;
use std::str::Utf8Error;

#[cfg(feature = "tls")]
use rustls::TLSError;

/// Errors from hreq.
#[derive(Debug)]
pub enum Error {
    /// The user of the lib did something to cause an error.
    User(String),
    /// Some protocol level error when talking to a server, not the user's fault.
    Proto(String),
    /// `std::io::Error`, such as connection problems, DNS lookup failures or timeouts.
    Io(io::Error),
    /// Failures to parse incoming HTTP/1.1 responses.
    Http11Parser(httparse::Error),
    /// Errors originating in HTTP/2 (via the `h2` crate).
    H2(h2::Error),
    /// Error from the `http` crate, such as `http::Request`, `http::Response` or URI.
    Http(http::Error),
    /// JSON deserialization errors.
    Json(serde_json::Error),
    /// TLS (https) errors.
    #[cfg(feature = "tls")]
    TlsError(TLSError),
    #[cfg(feature = "tls")]
    /// Failure to convert a domain name as TLS cert name.
    DnsName(webpki::InvalidDNSNameError),
    /// Failure to parse an address that the server will listen to.
    #[cfg(feature = "server")]
    AddrParse(net::AddrParseError),
    /// Failure to convert a string to UTF8.
    Utf8(Utf8Error),
}

impl Error {
    /// Tells whether the wrapper error is `std::io::Error`.
    pub fn is_io(&self) -> bool {
        match self {
            Error::Io(_) => true,
            _ => false,
        }
    }

    /// Converts this error to `std::io::Error`, if that is the wrapped error.
    pub fn into_io(self) -> Option<io::Error> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }

    /// Tells if this error is a timeout. Timeout errors are `std::io::Error`  with
    /// an `ErrorKind::TimedOut`.
    pub fn is_timeout(&self) -> bool {
        if let Error::Io(e) = self {
            if e.kind() == io::ErrorKind::TimedOut {
                return true;
            }
        }
        false
    }

    /// Agent retry function depends on this classifying retryable errors.
    pub(crate) fn is_retryable(&self) -> bool {
        match self {
            Error::Io(e) => match e.kind() {
                io::ErrorKind::BrokenPipe
                | io::ErrorKind::ConnectionAborted
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::Interrupted => true,
                _ => false,
            },
            _ => false,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::User(v) => write!(f, "{}", v),
            Error::Proto(v) => write!(f, "proto: {}", v),
            Error::Io(v) => fmt::Display::fmt(v, f),
            Error::Http11Parser(v) => write!(f, "http11 parser: {}", v),
            Error::H2(v) => write!(f, "http2: {}", v),
            Error::Http(v) => write!(f, "http api: {}", v),
            Error::Json(v) => write!(f, "json: {}", v),
            #[cfg(feature = "tls")]
            Error::TlsError(v) => write!(f, "tls: {}", v),
            #[cfg(feature = "tls")]
            Error::DnsName(v) => write!(f, "dns name: {}", v),
            #[cfg(feature = "server")]
            Error::AddrParse(v) => write!(f, "addr parse: {}", v),
            Error::Utf8(v) => write!(f, "utf-8: {}", v),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::User(_) => None,
            Error::Proto(_) => None,
            Error::Io(e) => Some(e),
            Error::Http11Parser(e) => Some(e),
            Error::H2(e) => Some(e),
            Error::Http(e) => Some(e),
            Error::Json(e) => Some(e),
            Error::TlsError(e) => Some(e),
            Error::DnsName(e) => Some(e),
            Error::AddrParse(e) => Some(e),
            Error::Utf8(e) => Some(e),
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<hreq_h1::Error> for Error {
    fn from(e: hreq_h1::Error) -> Self {
        match e {
            hreq_h1::Error::User(v) => Error::User(v),
            hreq_h1::Error::Io(v) => Error::Io(v),
            hreq_h1::Error::Http11Parser(v) => Error::Http11Parser(v),
            hreq_h1::Error::Http(v) => Error::Http(v),
        }
    }
}

impl From<h2::Error> for Error {
    fn from(e: h2::Error) -> Self {
        if e.is_io() {
            Error::Io(e.into_io().unwrap())
        } else {
            Error::H2(e)
        }
    }
}

impl From<http::Error> for Error {
    fn from(e: http::Error) -> Self {
        Error::Http(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e)
    }
}

#[cfg(feature = "tls")]
impl From<TLSError> for Error {
    fn from(e: TLSError) -> Self {
        Error::TlsError(e)
    }
}

#[cfg(feature = "tls")]
impl From<webpki::InvalidDNSNameError> for Error {
    fn from(e: webpki::InvalidDNSNameError) -> Self {
        Error::DnsName(e)
    }
}

#[cfg(feature = "server")]
impl From<net::AddrParseError> for Error {
    fn from(e: net::AddrParseError) -> Self {
        Error::AddrParse(e)
    }
}

impl From<Utf8Error> for Error {
    fn from(e: Utf8Error) -> Self {
        Error::Utf8(e)
    }
}
