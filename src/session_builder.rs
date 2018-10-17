use connection::{HeartBeat, OwnedCredentials};
use header::{Header, HeaderList};
use option_setter::OptionSetter;

use header::*;
use session::Session;
use std::io;
use std::net::ToSocketAddrs;
use tokio::net::TcpStream;

#[derive(Clone)]
pub struct SessionConfig {
    pub host: String,
    pub port: u16,
    pub credentials: Option<OwnedCredentials>,
    pub heartbeat: HeartBeat,
    pub headers: HeaderList,
}

pub struct SessionBuilder {
    pub config: SessionConfig,
}

impl SessionBuilder {
    pub fn start<'b, 'c>(self) -> ::std::io::Result<Session<TcpStream>> {
        let address = (&self.config.host as &str, self.config.port)
            .to_socket_addrs()?
            .nth(0)
            .ok_or(io::Error::new(
                io::ErrorKind::Other,
                "address provided resolved to nothing",
            ))?;
        let f = TcpStream::connect(&address);
        Ok(Session::new(self.config, Box::new(f)))
    }
}

impl SessionBuilder {
    pub fn new(host: &str, port: u16) -> SessionBuilder {
        let config = SessionConfig {
            host: host.to_owned(),
            port: port,
            credentials: None,
            heartbeat: HeartBeat(0, 0),
            headers: header_list![
           HOST => host,
           ACCEPT_VERSION => "1.2",
           CONTENT_LENGTH => "0"
          ],
        };
        SessionBuilder { config: config }
    }

    pub fn with<'b, O>(self, option_setter: O) -> SessionBuilder
    where
        O: OptionSetter<SessionBuilder>,
    {
        option_setter.set_option(self)
    }
}
