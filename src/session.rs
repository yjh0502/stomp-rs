use codec::Codec;
use connection::{self, Connection};
use frame::Transmission::{self, CompleteFrame, HeartBeat};
use frame::{Command, Frame, ToFrameBody};
use futures::*;
use header::{self, Header};
use message_builder::MessageBuilder;
use session_builder::SessionConfig;
use std::collections::hash_map::HashMap;
use std::collections::VecDeque;
use std::io::Error as IoError;
use std::io::ErrorKind;
use std::io::Result;
use std::time::{Duration, Instant};
use subscription::{AckMode, AckOrNack, Subscription};
use subscription_builder::SubscriptionBuilder;
use tokio::net::TcpStream;
use tokio_codec::Decoder;
use tokio_codec::Framed;
use tokio_io;
use tokio_timer::Delay;
use transaction::Transaction;

const GRACE_PERIOD_MULTIPLIER: f32 = 2.0;

pub struct OutstandingReceipt {
    pub original_frame: Frame,
}

impl OutstandingReceipt {
    pub fn new(original_frame: Frame) -> Self {
        OutstandingReceipt { original_frame }
    }
}

pub struct GenerateReceipt;

pub struct ReceiptRequest {
    pub id: String,
}

impl ReceiptRequest {
    pub fn new(id: String) -> Self {
        ReceiptRequest { id }
    }
}

fn poll_timeout(mut delay: Option<&mut Delay>) -> Poll<(), IoError> {
    match delay {
        None => Ok(Async::NotReady),
        Some(ref mut delay) => match delay.poll() {
            Err(_e) => Err(IoError::new(ErrorKind::Other, "timer")),
            Ok(res) => Ok(res),
        },
    }
}

pub struct SessionState {
    next_transaction_id: u32,
    next_subscription_id: u32,
    next_receipt_id: u32,
    pub rx_heartbeat_ms: Option<u32>,
    pub tx_heartbeat_ms: Option<u32>,
    pub rx_heartbeat_timeout: Option<Delay>,
    pub tx_heartbeat_timeout: Option<Delay>,
    pub subscriptions: HashMap<String, Subscription>,
    pub outstanding_receipts: HashMap<String, OutstandingReceipt>,
}

impl SessionState {
    pub fn new() -> SessionState {
        SessionState {
            next_transaction_id: 0,
            next_subscription_id: 0,
            next_receipt_id: 0,
            rx_heartbeat_ms: None,
            rx_heartbeat_timeout: None,
            tx_heartbeat_ms: None,
            tx_heartbeat_timeout: None,
            subscriptions: HashMap::new(),
            outstanding_receipts: HashMap::new(),
        }
    }
}

impl Session<TcpStream> {
    pub fn reconnect(&mut self) -> ::std::io::Result<()> {
        use std::io;
        use std::net::ToSocketAddrs;

        info!("Reconnecting...");

        let address = (&self.config.host as &str, self.config.port)
            .to_socket_addrs()?
            .nth(0)
            .ok_or(io::Error::new(
                io::ErrorKind::Other,
                "address provided resolved to nothing",
            ))?;

        let f = Box::new(TcpStream::connect(&address));
        self.stream = StreamState::Connecting(f);
        task::current().notify();
        Ok(())
    }
}

// *** Public API ***
impl<T> Session<T>
where
    T: tokio_io::AsyncWrite + tokio_io::AsyncRead + 'static,
{
    pub fn send_frame(&mut self, fr: Frame) {
        self.send(Transmission::CompleteFrame(fr))
    }

    pub fn message<'builder, O: ToFrameBody>(
        &'builder mut self,
        destination: &str,
        body_convertible: O,
    ) -> MessageBuilder<'builder, T> {
        let send_frame = Frame::send(destination, body_convertible.to_frame_body());
        MessageBuilder::new(self, send_frame)
    }

    pub fn subscription<'builder>(
        &'builder mut self,
        destination: &str,
    ) -> SubscriptionBuilder<'builder, T> {
        SubscriptionBuilder::new(self, destination.to_owned())
    }

    pub fn begin_transaction<'b>(&'b mut self) -> Transaction<'b, T> {
        let mut transaction = Transaction::new(self);
        let _ = transaction.begin();
        transaction
    }

    pub fn unsubscribe(&mut self, sub_id: &str) {
        self.state.subscriptions.remove(sub_id);
        let unsubscribe_frame = Frame::unsubscribe(sub_id.as_ref());
        self.send(CompleteFrame(unsubscribe_frame))
    }

    pub fn disconnect(&mut self) {
        self.send_frame(Frame::disconnect());
    }

    pub fn acknowledge_frame(&mut self, frame: &Frame, which: AckOrNack) {
        if let Some(header::Ack(ack_id)) = frame.headers.get_ack() {
            let ack_frame = if let AckOrNack::Ack = which {
                Frame::ack(ack_id)
            } else {
                Frame::nack(ack_id)
            };
            self.send_frame(ack_frame);
        }
    }
}

// *** pub(crate) API ***
impl<T> Session<T> {
    pub(crate) fn new(
        config: SessionConfig,
        stream: Box<Future<Item = T, Error = IoError>>,
    ) -> Self {
        Self {
            config,
            state: SessionState::new(),
            events: VecDeque::new(),
            stream: StreamState::Connecting(stream),
        }
    }

    pub(crate) fn generate_transaction_id(&mut self) -> u32 {
        let id = self.state.next_transaction_id;
        self.state.next_transaction_id += 1;
        id
    }

    pub(crate) fn generate_subscription_id(&mut self) -> u32 {
        let id = self.state.next_subscription_id;
        self.state.next_subscription_id += 1;
        id
    }

    pub(crate) fn generate_receipt_id(&mut self) -> u32 {
        let id = self.state.next_receipt_id;
        self.state.next_receipt_id += 1;
        id
    }
}

pub struct Session<T> {
    config: SessionConfig,
    pub(crate) state: SessionState,
    stream: StreamState<T>,
    events: VecDeque<SessionEvent>,
}

// *** Internal API ***
impl<T> Session<T>
where
    T: tokio_io::AsyncWrite + tokio_io::AsyncRead + 'static,
{
    fn _send(&mut self, tx: Transmission) -> Result<()> {
        if let StreamState::Connected(ref mut st) = self.stream {
            st.start_send(tx)?;
            st.poll_complete()?;
        } else {
            warn!("sending {:?} whilst disconnected", tx);
        }
        Ok(())
    }

    fn send(&mut self, tx: Transmission) {
        if let Err(e) = self._send(tx) {
            self.on_disconnect(DisconnectionReason::SendFailed(e));
        }
    }

    fn register_tx_heartbeat_timeout(&mut self) -> Result<()> {
        if self.state.tx_heartbeat_ms.is_none() {
            warn!("Trying to register TX heartbeat timeout, but not set!");
            return Ok(());
        }
        let tx_heartbeat_ms = self.state.tx_heartbeat_ms.unwrap();
        if tx_heartbeat_ms <= 0 {
            debug!(
                "Heartbeat transmission ms is {}, no need to register a callback.",
                tx_heartbeat_ms
            );
            return Ok(());
        }
        let timeout = Delay::new(Instant::now() + Duration::from_millis(tx_heartbeat_ms as _));
        self.state.tx_heartbeat_timeout = Some(timeout);
        Ok(())
    }

    fn register_rx_heartbeat_timeout(&mut self) -> Result<()> {
        let rx_heartbeat_ms = self.state.rx_heartbeat_ms.unwrap_or_else(|| {
            debug!(
                "Trying to register RX heartbeat timeout but no \
                 rx_heartbeat_ms was set. This is expected for receipt \
                 of CONNECTED."
            );
            0
        });
        if rx_heartbeat_ms <= 0 {
            debug!(
                "Heartbeat receipt ms is {}, no need to register a callback.",
                rx_heartbeat_ms
            );
            return Ok(());
        }

        let timeout = Delay::new(Instant::now() + Duration::from_millis(rx_heartbeat_ms as _));
        self.state.rx_heartbeat_timeout = Some(timeout);
        Ok(())
    }

    fn on_recv_data(&mut self) -> Result<()> {
        if self.state.rx_heartbeat_ms.is_some() {
            self.register_rx_heartbeat_timeout()?;
        }
        Ok(())
    }

    fn reply_to_heartbeat(&mut self) -> Result<()> {
        debug!("Sending heartbeat");
        self.send(HeartBeat);
        self.register_tx_heartbeat_timeout()?;
        Ok(())
    }

    fn on_disconnect(&mut self, reason: DisconnectionReason) {
        info!("Disconnected.");
        self.events.push_back(SessionEvent::Disconnected(reason));
        self.stream = StreamState::Failed;
        /*
        if let StreamState::Connected(ref mut strm) = self.stream {
            let _ = strm.get_mut().shutdown(::std::net::Shutdown::Both);
        }
        */
        self.stream = StreamState::Failed;
        self.state.tx_heartbeat_timeout = None;
        self.state.rx_heartbeat_timeout = None;
    }

    fn on_stream_ready(&mut self) {
        debug!("Stream ready!");
        // Add credentials to the header list if specified
        match self.config.credentials.clone() {
            // TODO: Refactor to avoid clone
            Some(credentials) => {
                debug!(
                    "Using provided credentials: login '{}', passcode '{}'",
                    credentials.login, credentials.passcode
                );
                let mut headers = &mut self.config.headers;
                headers.push(Header::new("login", &credentials.login));
                headers.push(Header::new("passcode", &credentials.passcode));
            }
            None => debug!("No credentials supplied."),
        }

        let connection::HeartBeat(client_tx_ms, client_rx_ms) = self.config.heartbeat;
        let heart_beat_string = format!("{},{}", client_tx_ms, client_rx_ms);
        debug!("Using heartbeat: {},{}", client_tx_ms, client_rx_ms);
        self.config
            .headers
            .push(Header::new("heart-beat", heart_beat_string.as_ref()));

        let connect_frame = Frame {
            command: Command::Connect,
            headers: self.config.headers.clone(), /* Cloned to allow this to be re-used */
            body: Vec::new(),
        };

        self.send_frame(connect_frame);
    }
    fn on_message(&mut self, frame: Frame) {
        let mut sub_data = None;
        if let Some(header::Subscription(sub_id)) = frame.headers.get_subscription() {
            if let Some(ref sub) = self.state.subscriptions.get(sub_id) {
                sub_data = Some((sub.destination.clone(), sub.ack_mode));
            }
        }
        if let Some((destination, ack_mode)) = sub_data {
            self.events.push_back(SessionEvent::Message {
                destination,
                ack_mode,
                frame,
            });
        } else {
            self.events
                .push_back(SessionEvent::SubscriptionlessFrame(frame));
        }
    }

    fn on_connected_frame_received(&mut self, connected_frame: Frame) -> Result<()> {
        // The Client's requested tx/rx HeartBeat timeouts
        let connection::HeartBeat(client_tx_ms, client_rx_ms) = self.config.heartbeat;

        // The timeouts the server is willing to provide
        let (server_tx_ms, server_rx_ms) = match connected_frame.headers.get_heart_beat() {
            Some(header::HeartBeat(tx_ms, rx_ms)) => (tx_ms, rx_ms),
            None => (0, 0),
        };

        let (agreed_upon_tx_ms, agreed_upon_rx_ms) =
            Connection::select_heartbeat(client_tx_ms, client_rx_ms, server_tx_ms, server_rx_ms);
        self.state.rx_heartbeat_ms =
            Some((agreed_upon_rx_ms as f32 * GRACE_PERIOD_MULTIPLIER) as u32);
        self.state.tx_heartbeat_ms = Some(agreed_upon_tx_ms);

        self.register_tx_heartbeat_timeout()?;
        self.register_rx_heartbeat_timeout()?;

        self.events.push_back(SessionEvent::Connected);

        Ok(())
    }
    fn handle_receipt(&mut self, frame: Frame) {
        let receipt_id = {
            if let Some(header::ReceiptId(receipt_id)) = frame.headers.get_receipt_id() {
                Some(receipt_id.to_owned())
            } else {
                None
            }
        };
        if let Some(receipt_id) = receipt_id {
            if receipt_id == "msg/disconnect" {
                self.on_disconnect(DisconnectionReason::Requested);
            }
            if let Some(entry) = self.state.outstanding_receipts.remove(&receipt_id) {
                let original_frame = entry.original_frame;
                self.events.push_back(SessionEvent::Receipt {
                    id: receipt_id,
                    original: original_frame,
                    receipt: frame,
                });
            }
        }
    }

    fn poll_stream_complete(&mut self) {
        let res = {
            if let StreamState::Connected(ref mut fr) = self.stream {
                fr.poll_complete()
            } else {
                Ok(Async::NotReady)
            }
        };
        if let Err(e) = res {
            self.on_disconnect(DisconnectionReason::SendFailed(e));
        }
    }
    fn poll_stream(&mut self) -> Async<Option<Transmission>> {
        use self::StreamState::*;
        loop {
            match ::std::mem::replace(&mut self.stream, Failed) {
                Connected(mut fr) => match fr.poll() {
                    Ok(Async::Ready(Some(r))) => {
                        self.stream = Connected(fr);
                        return Async::Ready(Some(r));
                    }
                    Ok(Async::Ready(None)) => {
                        self.on_disconnect(DisconnectionReason::ClosedByOtherSide);
                        return Async::NotReady;
                    }
                    Ok(Async::NotReady) => {
                        self.stream = Connected(fr);
                        return Async::NotReady;
                    }
                    Err(e) => {
                        self.on_disconnect(DisconnectionReason::RecvFailed(e));
                        return Async::NotReady;
                    }
                },
                Connecting(mut tsn) => match tsn.poll() {
                    Ok(Async::Ready(s)) => {
                        let fr = Codec.framed(s);
                        self.stream = Connected(fr);
                        self.on_stream_ready();
                    }
                    Ok(Async::NotReady) => {
                        self.stream = Connecting(tsn);
                        return Async::NotReady;
                    }
                    Err(e) => {
                        self.on_disconnect(DisconnectionReason::ConnectFailed(e));
                        return Async::NotReady;
                    }
                },
                Failed => {
                    return Async::NotReady;
                }
            }
        }
    }
}

#[derive(Debug)]
pub enum DisconnectionReason {
    RecvFailed(IoError),
    ConnectFailed(IoError),
    SendFailed(IoError),
    ClosedByOtherSide,
    HeartbeatTimeout,
    Requested,
}

pub enum SessionEvent {
    Connected,
    ErrorFrame(Frame),
    Receipt {
        id: String,
        original: Frame,
        receipt: Frame,
    },
    Message {
        destination: String,
        ack_mode: AckMode,
        frame: Frame,
    },
    SubscriptionlessFrame(Frame),
    UnknownFrame(Frame),
    Disconnected(DisconnectionReason),
}

pub(crate) enum StreamState<T> {
    Connected(Framed<T, Codec>),
    Connecting(Box<Future<Item = T, Error = IoError>>),
    Failed,
}

impl<T> Stream for Session<T>
where
    T: tokio_io::AsyncWrite + tokio_io::AsyncRead + 'static,
{
    type Item = SessionEvent;
    type Error = IoError;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        use frame::Transmission::*;

        while let Async::Ready(Some(val)) = self.poll_stream() {
            match val {
                HeartBeat => {
                    debug!("Received heartbeat.");
                    self.on_recv_data()?;
                }
                CompleteFrame(frame) => {
                    debug!("Received frame: {:?}", frame);
                    self.on_recv_data()?;
                    match frame.command {
                        Command::Error => self.events.push_back(SessionEvent::ErrorFrame(frame)),
                        Command::Receipt => self.handle_receipt(frame),
                        Command::Connected => self.on_connected_frame_received(frame)?,
                        Command::Message => self.on_message(frame),
                        _ => self.events.push_back(SessionEvent::UnknownFrame(frame)),
                    };
                }
            }
        }

        if let Async::Ready(_) = poll_timeout(self.state.rx_heartbeat_timeout.as_mut())? {
            self.on_disconnect(DisconnectionReason::HeartbeatTimeout);
        }

        if let Async::Ready(_) = poll_timeout(self.state.tx_heartbeat_timeout.as_mut())? {
            self.reply_to_heartbeat()?;
        }

        self.poll_stream_complete();

        match self.events.pop_front() {
            None => Ok(Async::NotReady),
            Some(ev) => {
                task::current().notify();
                Ok(Async::Ready(Some(ev)))
            }
        }
    }
}
