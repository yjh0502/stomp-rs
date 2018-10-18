extern crate env_logger;
extern crate stomp;
extern crate tokio;
extern crate tokio_io;
#[macro_use]
extern crate futures;

use futures::future::join_all;
use stomp::connection::{Credentials, HeartBeat};
use stomp::frame::Frame;
use stomp::header::*;
use stomp::session_builder::SessionBuilder;
use stomp::subscription::AckMode;
use stomp::Session;
use tokio::net::TcpStream;
use tokio::prelude::*;

struct ExampleSession {
    session: Session<TcpStream>,
    destination: String,
    session_number: u32,

    subscription_id: Option<String>,
}

impl ExampleSession {
    fn on_connected(&mut self) {
        println!("Example session established.");
        let destination = &self.destination;
        println!("Subscribing to '{}'.", destination);

        let header_name = HeaderName::from_str("custom-subscription-header");
        let subscription_id = self
            .session
            .subscription(destination)
            .with(AckMode::Auto)
            .with(Header::new(header_name, "lozenge"))
            .start();

        self.subscription_id = Some(subscription_id);

        self.session.message(destination, "Animal").send();
        self.session.message(destination, "Vegetable").send();
        self.session.message(destination, "Mineral").send();
    }

    fn on_gilbert_and_sullivan_reference(&mut self, frame: Frame) {
        println!(
            "Another droll reference!: '{}'",
            std::str::from_utf8(&frame.body).expect("Non-utf8 bytes")
        );
    }
}

impl Future for ExampleSession {
    type Item = ();
    type Error = std::io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        use stomp::session::SessionEvent::*;

        let msg = match try_ready!(self.session.poll()) {
            None => {
                return Ok(Async::Ready(()));
            }
            Some(msg) => msg,
        };

        println!("msg: {:?}", msg);
        match msg {
            Connected => {
                self.on_connected();
            }

            Receipt {
                id: _id,
                original,
                receipt,
            } => {
                let destination = original.headers.get(DESTINATION).unwrap();
                println!(
                    "Received a Receipt for our subscription to '{}':\n{}",
                    destination, receipt
                );
            }

            Message {
                destination: _destination,
                ack_mode: _ack_mode,
                frame,
            } => {
                let subscribed = if let Some(subscription) = frame.headers.get(SUBSCRIPTION) {
                    self.subscription_id == Some(subscription.to_owned())
                } else {
                    false
                };

                if subscribed {
                    self.on_gilbert_and_sullivan_reference(frame)
                }
            }

            Subscriptionless(_frame) | Unknown(_frame) => {
                //
            }

            Error(frame) => {
                println!("Something went horribly wrong: {}", frame);
            }

            Disconnected(reason) => {
                println!(
                    "Session #{} disconnected, reason={:?}",
                    self.session_number, reason
                );
            }
        }

        Ok(Async::NotReady)
    }
}

fn main() {
    env_logger::init();

    println!("Setting up client.");
    let mut sessions = Vec::new();
    for session_number in 0..1 {
        let header = HeaderName::from_str("custom-client-id");

        let addr = "127.0.0.1:61613".parse().unwrap();
        let f = Box::new(TcpStream::connect(&addr));

        let session = SessionBuilder::new()
            .with(Header::new(header, "hmspna4"))
            .with(SuppressedHeader("content-length"))
            .with(HeartBeat(5_000, 2_000))
            .with(Credentials("admin", "admin"))
            .build(f);

        let session = ExampleSession {
            session,
            session_number: 0,
            destination: format!("/topic/modern_major_general_{}", session_number),

            subscription_id: None,
        };
        sessions.push(session);
    }

    println!("Starting session.");
    tokio::run(
        join_all(sessions)
            .map(|_| println!("all sessions are finished"))
            .map_err(|e| {
                println!("error: {:?}", e);
            }),
    );
}
