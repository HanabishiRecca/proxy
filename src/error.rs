use std::{
    error::Error,
    fmt::{Display, Formatter, Result},
    io::Error as IOError,
    str::Utf8Error,
};

#[macro_export]
macro_rules! E {
    ($e: expr) => {
        return Err($e.into())
    };
}

pub fn err(e: impl Error) {
    eprintln!("Error: {e}");
}

#[derive(Debug)]
#[non_exhaustive]
pub enum MainError {
    ArgError(ArgError),
    AppError(AppError),
}

impl Error for MainError {}

impl Display for MainError {
    fn fmt(&self, f: &mut Formatter) -> Result {
        use MainError::*;
        match self {
            ArgError(e) => e.fmt(f),
            AppError(e) => e.fmt(f),
        }
    }
}

impl From<AppError> for MainError {
    fn from(e: AppError) -> Self {
        Self::AppError(e)
    }
}

impl From<ArgError> for MainError {
    fn from(e: ArgError) -> Self {
        Self::ArgError(e)
    }
}

#[derive(Debug)]
#[non_exhaustive]
pub enum AppError {
    Unknown,
    IOError(IOError),
}

impl Error for AppError {}

impl Display for AppError {
    fn fmt(&self, f: &mut Formatter) -> Result {
        use AppError::*;
        match self {
            Unknown => write!(f, "an unknown error occured"),
            IOError(e) => e.fmt(f),
        }
    }
}

impl From<IOError> for AppError {
    fn from(e: IOError) -> Self {
        Self::IOError(e)
    }
}

#[derive(Debug)]
#[non_exhaustive]
pub enum ArgError {
    NoProxy,
    NoHosts,
    NoValue(String),
    WrongValue(String),
    Unknown(String),
}

impl Error for ArgError {}

impl Display for ArgError {
    fn fmt(&self, f: &mut Formatter) -> Result {
        use ArgError::*;
        match self {
            NoProxy => write!(f, "proxy server not specified"),
            NoHosts => write!(f, "target hosts not specified"),
            NoValue(arg) => write!(f, "option '{arg}' requires value"),
            WrongValue(arg) => write!(f, "wrong value for option '{arg}'"),
            Unknown(arg) => write!(f, "unknown option '{arg}'"),
        }
    }
}

#[derive(Debug)]
#[non_exhaustive]
pub enum ConnError {
    NotHttp,
    ParseError,
    DnsError,
    Unknown,
    IOError(IOError),
}

impl Error for ConnError {}

impl Display for ConnError {
    fn fmt(&self, f: &mut Formatter) -> Result {
        use ConnError::*;
        match self {
            NotHttp => write!(f, "not HTTP GET request"),
            ParseError => write!(f, "unable to parse request"),
            DnsError => write!(f, "unable to resolve host"),
            Unknown => write!(f, "an unknown error occured"),
            IOError(e) => e.fmt(f),
        }
    }
}

impl From<Utf8Error> for ConnError {
    fn from(_: Utf8Error) -> Self {
        Self::ParseError
    }
}

impl From<IOError> for ConnError {
    fn from(e: IOError) -> Self {
        Self::IOError(e)
    }
}
