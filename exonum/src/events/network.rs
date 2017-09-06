// Copyright 2017 The Exonum Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use futures::{unsync, Future, IntoFuture, Sink, Stream};
use futures::future::Either;
use futures::sync::mpsc;
use tokio_core::net::{TcpListener, TcpStream, TcpStreamNew};
use tokio_core::reactor::{Core, Handle};
use tokio_io::AsyncRead;
use tokio_retry::{Action, Retry};
use tokio_retry::strategy::{jitter, FixedInterval};

use std::io;
use std::net::SocketAddr;
use std::time::Duration;
use std::collections::HashMap;
use std::rc::Rc;
use std::cell::RefCell;

use messages::{Any, Connect, RawMessage};
use node::{ExternalMessage, NodeTimeout};
use helpers::Milliseconds;

use super::error::{into_other, log_error, other_error, result_ok};
use super::codec::MessagesCodec;
use super::{EventsAggregator, EventHandler};

#[derive(Debug)]
pub enum NetworkEvent {
    MessageReceived(SocketAddr, RawMessage),
    PeerConnected(SocketAddr, Connect),
    PeerDisconnected(SocketAddr),
}

#[derive(Debug, Clone)]
pub enum NetworkRequest {
    SendMessage(SocketAddr, RawMessage),
    DisconnectWithPeer(SocketAddr),
    Shutdown,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub struct NetworkConfiguration {
    // TODO: think more about config parameters
    pub max_incoming_connections: usize,
    pub max_outgoing_connections: usize,
    pub tcp_nodelay: bool,
    pub tcp_keep_alive: Option<u64>,
    pub tcp_connect_retry_timeout: Milliseconds,
    pub tcp_connect_max_retries: u64,
}

impl Default for NetworkConfiguration {
    fn default() -> NetworkConfiguration {
        NetworkConfiguration {
            max_incoming_connections: 128,
            max_outgoing_connections: 128,
            tcp_keep_alive: None,
            tcp_nodelay: false,
            tcp_connect_retry_timeout: 15_000,
            tcp_connect_max_retries: 10,
        }
    }
}

#[derive(Debug)]
pub struct HandlerPart<H: EventHandler> {
    pub core: Core,
    pub handler: H,
    pub timeout_rx: mpsc::Receiver<NodeTimeout>,
    pub network_rx: mpsc::Receiver<NetworkEvent>,
    pub api_rx: mpsc::Receiver<ExternalMessage>,
}

#[derive(Debug)]
pub struct NetworkPart {
    pub listen_address: SocketAddr,
    pub network_config: NetworkConfiguration,
    pub network_requests: (mpsc::Sender<NetworkRequest>, mpsc::Receiver<NetworkRequest>),
    pub network_tx: mpsc::Sender<NetworkEvent>,
}

#[derive(Debug, Default, Clone)]
struct ConnectionsPool {
    inner: Rc<RefCell<HashMap<SocketAddr, mpsc::Sender<RawMessage>>>>,
}

impl ConnectionsPool {
    fn new() -> ConnectionsPool {
        ConnectionsPool::default()
    }

    fn insert(&self, peer: SocketAddr, sender: &mpsc::Sender<RawMessage>) {
        self.inner.borrow_mut().insert(peer, sender.clone());
    }

    fn remove(&self, peer: &SocketAddr) {
        self.inner.borrow_mut().remove(peer);
    }

    fn get(&self, peer: SocketAddr) -> Option<mpsc::Sender<RawMessage>> {
        self.inner.borrow_mut().get(&peer).cloned()
    }

    fn len(&self) -> usize {
        self.inner.borrow_mut().len()
    }
}

impl<H: EventHandler> HandlerPart<H> {
    pub fn run(self) -> Result<(), ()> {
        let mut core = self.core;
        let mut handler = self.handler;

        let events_handle = EventsAggregator::new(self.timeout_rx, self.network_rx, self.api_rx)
            .for_each(move |event| {
                handler.handle_event(event);
                Ok(())
            });
        core.run(events_handle)
    }
}

impl NetworkPart {
    pub fn run(self) -> Result<(), io::Error> {
        let network_config = self.network_config;
        // Cancelation token
        let (cancel_sender, cancel_handler) = unsync::mpsc::channel(1);
        // Outgoing connections
        let outgoing_connections = ConnectionsPool::new();
        // Requests handler
        let mut core = Core::new()?;
        let handle = core.handle();
        let network_tx = self.network_tx.clone();
        // Outgoing connections limiter
        let outgoing_connections_limit = network_config.max_outgoing_connections;
        let requests_handle = self.network_requests.1.for_each(move |request| {
            let network_tx = network_tx.clone();
            let cancel_sender = cancel_sender.clone();
            let outgoing_connections = outgoing_connections.clone();
            match request {
                NetworkRequest::SendMessage(peer, msg) => {
                    let conn_tx = if let Some(conn_tx) = outgoing_connections.get(peer) {
                        conn_tx
                    } else {
                        // Check limit
                        if outgoing_connections.len() >= outgoing_connections_limit {
                            warn!(
                                "Rejected outgoing connection with peer={}, \
                                 connections limit reached.",
                                peer
                            );
                            Self::send_event(
                                &handle,
                                &network_tx,
                                NetworkEvent::PeerDisconnected(peer),
                            );
                            return Ok(());
                        }
                        // Register outgoing channel.
                        let (conn_tx, conn_rx) = mpsc::channel(10);
                        outgoing_connections.insert(peer, &conn_tx);
                        // Enable retry feature for outgoing connection.
                        let timeout = network_config.tcp_connect_retry_timeout;
                        let max_tries = network_config.tcp_connect_max_retries as usize;
                        let connect_handle = Retry::spawn(
                            handle.clone(),
                            FixedInterval::from_millis(timeout).map(jitter).take(
                                max_tries,
                            ),
                            TcpStreamConnectAction {
                                handle: handle.clone(),
                                peer,
                            },
                        ).map_err(into_other)
                            // Configure socket
                            .and_then(move |sock| {
                                sock.set_nodelay(network_config.tcp_nodelay)?;
                                Ok(sock)
                            })
                            .and_then(move |sock| {
                                let duration =
                                    network_config.tcp_keep_alive.map(Duration::from_millis);
                                sock.set_keepalive(duration)?;
                                Ok(sock)
                            })
                            // Connect socket with the outgoing channel
                            .and_then(move |sock| {
                                trace!("Established connection with peer={}", peer);

                                let stream = sock.framed(MessagesCodec);
                                let (sink, stream) = stream.split();

                                let writer = conn_rx
                                    .map_err(|_| other_error("Can't send data into socket"))
                                    .forward(sink);
                                let reader = stream.for_each(result_ok);

                                reader
                                    .select2(writer)
                                    .map_err(|_| other_error("Socket error"))
                                    .and_then(|res| match res {
                                        Either::A((_, _reader)) => Ok(()).into_future(),
                                        Either::B((_, _writer)) => Ok(()).into_future(),
                                    })
                            })
                            .then(move |res| {
                                trace!("Connection with peer={} closed, reason={:?}", peer, res);
                                outgoing_connections.clone().remove(&peer);
                                network_tx
                                    .clone()
                                    .send(NetworkEvent::PeerDisconnected(peer))
                                    .map(drop)
                            })
                            .map_err(log_error);
                        handle.spawn(connect_handle);
                        conn_tx
                    };

                    let send_handle = conn_tx.send(msg).map_err(log_error).map(drop);
                    handle.spawn(send_handle);
                }
                NetworkRequest::DisconnectWithPeer(peer) => {
                    outgoing_connections.remove(&peer);
                    Self::send_event(&handle, &network_tx, NetworkEvent::PeerDisconnected(peer));
                }
                // Immediately stop the event loop.
                NetworkRequest::Shutdown => {
                    cancel_sender
                        .clone()
                        .send(())
                        .map(drop)
                        .map_err(log_error)
                        .wait()?
                }
            }

            Ok(())
        });

        // Incoming connections limiter
        let incoming_connections_limit = network_config.max_incoming_connections;
        let incoming_connections_counter: Rc<()> = Rc::default();
        // Incoming connections handler
        let listener = TcpListener::bind(&self.listen_address, &core.handle())?;
        let network_tx = self.network_tx.clone();
        let handle = core.handle();
        let server = listener
            .incoming()
            .for_each(move |(sock, addr)| {
                // Increment reference counter
                let holder = Rc::downgrade(&incoming_connections_counter);
                // Check incoming connections count
                let connections_count = Rc::weak_count(&incoming_connections_counter);
                if connections_count > incoming_connections_limit {
                    warn!(
                        "Rejected incoming connection with peer={}, \
                         connections limit reached.",
                        addr
                    );
                    return Ok(());
                }
                trace!("Accepted incoming connection with peer={}", addr);
                let stream = sock.framed(MessagesCodec);
                let (_, stream) = stream.split();
                let network_tx = network_tx.clone();
                let connection_handler = stream
                    .into_future()
                    .map_err(|e| e.0)
                    .and_then(move |(raw, stream)| match raw.map(Any::from_raw) {
                        Some(Ok(Any::Connect(msg))) => Ok((msg, stream)),
                        Some(Ok(other)) => Err(other_error(&format!(
                            "First message is not Connect, got={:?}",
                            other
                        ))),
                        Some(Err(e)) => Err(into_other(e)),
                        None => Err(other_error("Incoming socket closed")),
                    })
                    .and_then(move |(connect, stream)| {
                        trace!("Received handshake message={:?}", connect);
                        let event = NetworkEvent::PeerConnected(addr, connect);
                        let stream = network_tx
                            .clone()
                            .send(event)
                            .map_err(into_other)
                            .and_then(move |_| Ok(stream))
                            .flatten_stream();

                        stream.for_each(move |raw| {
                            let event = NetworkEvent::MessageReceived(addr, raw);
                            network_tx.clone().send(event).map_err(into_other).map(drop)
                        })
                    })
                    .map(|_| {
                        // Ensure that holder lives until the stream ends.
                        let _holder = holder;
                    })
                    .map_err(log_error);
                handle.spawn(connection_handler);
                Ok(())
            })
            .map_err(log_error);

        core.handle().spawn(server);
        core.handle().spawn(requests_handle);
        core.run(
            cancel_handler
                .into_future()
                .map(|_| trace!("Network thread shutdown"))
                .map_err(|_| other_error("An error during `Core` shutdown occured")),
        )
    }

    fn send_event(handle: &Handle, network_tx: &mpsc::Sender<NetworkEvent>, event: NetworkEvent) {
        handle.spawn(network_tx.clone().send(event).map(drop).map_err(log_error));
    }
}

// Tcp streams source for retry action.
struct TcpStreamConnectAction {
    handle: Handle,
    peer: SocketAddr,
}

impl Action for TcpStreamConnectAction {
    type Future = TcpStreamNew;
    type Item = <TcpStreamNew as Future>::Item;
    type Error = <TcpStreamNew as Future>::Error;

    fn run(&mut self) -> Self::Future {
        TcpStream::connect(&self.peer, &self.handle)
    }
}
