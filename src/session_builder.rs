use connection::{HeartBeat, OwnedCredentials};
use header::{Header, HeaderList};
use option_setter::OptionSetter;

use header::*;
use session::{ConnectFuture, Session};
use std::io;
use std::net::ToSocketAddrs;
use tokio::net::TcpStream;
use tokio_io;

#[derive(Clone)]
pub struct SessionConfig {
    pub credentials: Option<OwnedCredentials>,
    pub heartbeat: HeartBeat,
    pub headers: HeaderList,
}

pub struct SessionBuilder {
    pub config: SessionConfig,
}

impl SessionBuilder {
    pub fn new() -> SessionBuilder {
        let config = SessionConfig {
            credentials: None,
            heartbeat: HeartBeat(0, 0),
            headers: header_list![
                ACCEPT_VERSION => "1.2",
                CONTENT_LENGTH => "0"
            ],
        };
        SessionBuilder { config: config }
    }

    pub fn build<T>(self, conn: ConnectFuture<T>) -> Session<T>
    where
        T: tokio_io::AsyncWrite + tokio_io::AsyncRead + Send + 'static,
    {
        Session::new(self.config, conn)
    }

    pub fn with<'b, O>(self, option_setter: O) -> SessionBuilder
    where
        O: OptionSetter<SessionBuilder>,
    {
        option_setter.set_option(self)
    }
}
