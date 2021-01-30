use std::io;
use tokio::time::Elapsed;

pub enum IOReadErrorValue {
    NotEnoughBytes,
    IO(io::Error),
    TimeoutError(Elapsed),
}

pub struct IOReadError {
    pub value: IOReadErrorValue,
}

impl From<IOReadErrorValue> for IOReadError {
    fn from(val: IOReadErrorValue) -> Self {
        IOReadError { value: val }
    }
}

impl From<io::Error> for IOReadError {
    fn from(error: io::Error) -> Self {
        IOReadError {
            value: IOReadErrorValue::IO(error),
        }
    }
}

impl From<Elapsed> for IOReadError {
    fn from(error: Elapsed) -> Self {
        IOReadError {
            value: IOReadErrorValue::TimeoutError(error),
        }
    }
}