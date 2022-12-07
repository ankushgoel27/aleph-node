use std::{
    collections::{HashMap, HashSet},
    future::Future,
};

use futures::{channel::mpsc, StreamExt};
use log::{debug, error, info, trace, warn};
use sc_service::SpawnTaskHandle;
use sc_utils::mpsc::{tracing_unbounded, TracingUnboundedReceiver, TracingUnboundedSender};
use tokio::time;

use crate::{
    network::{Data, Event, EventStream, Network, NetworkSender, Protocol},
    STATUS_REPORT_INTERVAL,
};

/// A service managing all the direct interaction with the underlying network implementation. It
/// handles:
/// 1. Incoming network events
///   1. Messages are forwarded to the user.
///   2. Various forms of (dis)connecting, keeping track of all currently connected nodes.
/// 3. Outgoing messages, sending them out, using 1.2. to broadcast.
pub struct Service<N: Network, D: Data> {
    network: N,
    messages_from_user: mpsc::UnboundedReceiver<D>,
    messages_for_user: mpsc::UnboundedSender<D>,
    authentication_connected_peers: HashSet<N::PeerId>,
    authentication_peer_senders: HashMap<N::PeerId, TracingUnboundedSender<D>>,
    spawn_handle: SpawnTaskHandle,
}

/// Input/output channels for the network service.
pub struct IO<D: Data> {
    pub messages_from_user: mpsc::UnboundedReceiver<D>,
    pub messages_for_user: mpsc::UnboundedSender<D>,
}

impl<D: Data> IO<D> {
    pub fn new(
        messages_from_user: mpsc::UnboundedReceiver<D>,
        messages_for_user: mpsc::UnboundedSender<D>,
    ) -> IO<D> {
        IO {
            messages_from_user,
            messages_for_user,
        }
    }
}

#[derive(Debug)]
enum SendError {
    MissingSender,
    SendingFailed,
}

impl<N: Network, D: Data> Service<N, D> {
    pub fn new(network: N, spawn_handle: SpawnTaskHandle, io: IO<D>) -> Service<N, D> {
        Service {
            network,
            messages_from_user: io.messages_from_user,
            messages_for_user: io.messages_for_user,
            spawn_handle,
            authentication_connected_peers: HashSet::new(),
            authentication_peer_senders: HashMap::new(),
        }
    }

    fn get_sender(
        &mut self,
        peer: &N::PeerId,
        protocol: Protocol,
    ) -> Option<&mut TracingUnboundedSender<D>> {
        match protocol {
            Protocol::Authentication => self.authentication_peer_senders.get_mut(peer),
        }
    }

    fn peer_sender(
        &self,
        peer_id: N::PeerId,
        mut receiver: TracingUnboundedReceiver<D>,
        protocol: Protocol,
    ) -> impl Future<Output = ()> + Send + 'static {
        let network = self.network.clone();
        async move {
            let mut sender = None;
            loop {
                if let Some(data) = receiver.next().await {
                    let s = if let Some(s) = sender.as_mut() {
                        s
                    } else {
                        match network.sender(peer_id.clone(), protocol) {
                            Ok(s) => sender.insert(s),
                            Err(e) => {
                                debug!(target: "aleph-network", "Failed creating sender. Dropping message: {}", e);
                                continue;
                            }
                        }
                    };
                    if let Err(e) = s.send(data.encode()).await {
                        debug!(target: "aleph-network", "Failed sending data to peer. Dropping sender and message: {}", e);
                        sender = None;
                    }
                } else {
                    debug!(target: "aleph-network", "Sender was dropped for peer {:?}. Peer sender exiting.", peer_id);
                    return;
                }
            }
        }
    }

    fn send_to_peer(
        &mut self,
        data: D,
        peer: N::PeerId,
        protocol: Protocol,
    ) -> Result<(), SendError> {
        match self.get_sender(&peer, protocol) {
            Some(sender) => {
                match sender.unbounded_send(data) {
                    Err(e) => {
                        // Receiver can also be dropped when thread cannot send to peer. In case receiver is dropped this entry will be removed by Event::NotificationStreamClosed
                        // No need to remove the entry here
                        if e.is_disconnected() {
                            trace!(target: "aleph-network", "Failed sending data to peer because peer_sender receiver is dropped: {:?}", peer);
                        }
                        Err(SendError::SendingFailed)
                    }
                    Ok(_) => Ok(()),
                }
            }
            None => Err(SendError::MissingSender),
        }
    }

    fn broadcast(&mut self, data: D, protocol: Protocol) {
        let peers = match protocol {
            Protocol::Authentication => self.authentication_connected_peers.clone(),
        };
        for peer in peers {
            if let Err(e) = self.send_to_peer(data.clone(), peer.clone(), protocol) {
                trace!(target: "aleph-network", "Failed to send broadcast to peer{:?}, {:?}", peer, e);
            }
        }
    }

    fn handle_network_event(
        &mut self,
        event: Event<N::PeerId>,
    ) -> Result<(), mpsc::TrySendError<D>> {
        use Event::*;
        match event {
            StreamOpened(peer, protocol) => {
                trace!(target: "aleph-network", "StreamOpened event for peer {:?} and the protocol {:?}.", peer, protocol);
                let rx = match &protocol {
                    Protocol::Authentication => {
                        let (tx, rx) = tracing_unbounded("mpsc_notification_stream_authentication");
                        self.authentication_connected_peers.insert(peer.clone());
                        self.authentication_peer_senders.insert(peer.clone(), tx);
                        rx
                    }
                };
                self.spawn_handle.spawn(
                    "aleph/network/peer_sender",
                    None,
                    self.peer_sender(peer, rx, protocol),
                );
            }
            StreamClosed(peer, protocol) => {
                trace!(target: "aleph-network", "StreamClosed event for peer {:?} and protocol {:?}", peer, protocol);
                match protocol {
                    Protocol::Authentication => {
                        self.authentication_connected_peers.remove(&peer);
                        self.authentication_peer_senders.remove(&peer);
                    }
                }
            }
            Messages(messages) => {
                for (protocol, data) in messages.into_iter() {
                    match protocol {
                        Protocol::Authentication => match D::decode(&mut &data[..]) {
                            Ok(data) => self.messages_for_user.unbounded_send(data)?,
                            Err(e) => {
                                warn!(target: "aleph-network", "Error decoding authentication protocol message: {}", e)
                            }
                        },
                    };
                }
            }
        }
        Ok(())
    }

    fn status_report(&self) {
        let mut status = String::from("Network status report: ");

        status.push_str(&format!(
            "authentication connected peers - {:?}; ",
            self.authentication_connected_peers.len()
        ));

        info!(target: "aleph-network", "{}", status);
    }

    pub async fn run(mut self) {
        let mut events_from_network = self.network.event_stream();

        let mut status_ticker = time::interval(STATUS_REPORT_INTERVAL);
        loop {
            tokio::select! {
                maybe_event = events_from_network.next_event() => match maybe_event {
                    Some(event) => if let Err(e) = self.handle_network_event(event) {
                        error!(target: "aleph-network", "Cannot forward messages to user: {:?}", e);
                        return;
                    },
                    None => {
                        error!(target: "aleph-network", "Network event stream ended.");
                        return;
                    }
                },
                maybe_message = self.messages_from_user.next() => match maybe_message {
                    Some(message) => self.broadcast(message, Protocol::Authentication),
                    None => {
                        error!(target: "aleph-network", "User message stream ended.");
                        return;
                    }
                },
                _ = status_ticker.tick() => {
                    self.status_report();
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use codec::Encode;
    use futures::{
        channel::{mpsc, oneshot},
        StreamExt,
    };
    use sc_service::TaskManager;
    use tokio::runtime::Handle;

    use super::Service;
    use crate::{
        network::{
            mock::{MockData, MockEvent, MockNetwork, MockSenderError},
            NetworkServiceIO, Protocol,
        },
        testing::mocks::validator_network::random_peer_id,
    };

    const PROTOCOL: Protocol = Protocol::Authentication;

    pub struct TestData {
        pub network: MockNetwork,
        pub messages_from_network: mpsc::UnboundedReceiver<MockData>,
        pub service: Service<MockNetwork, MockData>,
        // `TaskManager` can't be dropped for `SpawnTaskHandle` to work
        _task_manager: TaskManager,
    }

    impl TestData {
        async fn prepare() -> Self {
            let task_manager = TaskManager::new(Handle::current(), None).unwrap();

            // Prepare communication with service
            // We can drop the sender, as we will call service.broadcast directly
            let (_, messages_from_user) = mpsc::unbounded();
            let (messages_for_user, messages_from_network) = mpsc::unbounded();
            let io = NetworkServiceIO::new(messages_from_user, messages_for_user);
            // Event stream will never be taken, so we can drop the receiver
            let (event_stream_oneshot_tx, _) = oneshot::channel();

            // Prepare service
            let network = MockNetwork::new(event_stream_oneshot_tx);
            let service = Service::new(network.clone(), task_manager.spawn_handle(), io);

            // `TaskManager` needs to be passed, so sender threads are running in background.
            Self {
                network,
                service,
                messages_from_network,
                _task_manager: task_manager,
            }
        }

        async fn cleanup(self) {
            self.network.close_channels().await;
        }
    }

    fn message(i: u8) -> MockData {
        vec![i, i + 1, i + 2]
    }

    #[tokio::test]
    async fn test_notification_stream_opened() {
        let mut test_data = TestData::prepare().await;

        let peer_ids: Vec<_> = (0..3).map(|_| random_peer_id()).collect();

        peer_ids.iter().for_each(|peer_id| {
            test_data
                .service
                .handle_network_event(MockEvent::StreamOpened(peer_id.clone(), PROTOCOL))
                .expect("Should handle");
        });

        let message = message(1);
        test_data.service.broadcast(message.clone(), PROTOCOL);

        let broadcasted_messages = HashSet::<_>::from_iter(
            test_data
                .network
                .send_message
                .take(peer_ids.len())
                .await
                .into_iter(),
        );

        let expected_messages = HashSet::from_iter(
            peer_ids
                .into_iter()
                .map(|peer_id| (message.clone().encode(), peer_id, PROTOCOL)),
        );

        assert_eq!(broadcasted_messages, expected_messages);

        test_data.cleanup().await
    }

    #[tokio::test]
    async fn test_notification_stream_closed() {
        let mut test_data = TestData::prepare().await;

        let peer_ids: Vec<_> = (0..3).map(|_| random_peer_id()).collect();
        let opened_authorities_n = 2;

        peer_ids.iter().for_each(|peer_id| {
            test_data
                .service
                .handle_network_event(MockEvent::StreamOpened(peer_id.clone(), PROTOCOL))
                .expect("Should handle");
        });

        peer_ids
            .iter()
            .skip(opened_authorities_n)
            .for_each(|peer_id| {
                test_data
                    .service
                    .handle_network_event(MockEvent::StreamClosed(peer_id.clone(), PROTOCOL))
                    .expect("Should handle");
            });

        let message = message(1);
        test_data.service.broadcast(message.clone(), PROTOCOL);

        let broadcasted_messages = HashSet::<_>::from_iter(
            test_data
                .network
                .send_message
                .take(opened_authorities_n)
                .await
                .into_iter(),
        );

        let expected_messages = HashSet::from_iter(
            peer_ids
                .into_iter()
                .take(opened_authorities_n)
                .map(|peer_id| (message.clone().encode(), peer_id, PROTOCOL)),
        );

        assert_eq!(broadcasted_messages, expected_messages);

        test_data.cleanup().await
    }

    #[tokio::test]
    async fn test_create_sender_error() {
        let mut test_data = TestData::prepare().await;

        test_data
            .network
            .create_sender_errors
            .lock()
            .push_back(MockSenderError);

        let peer_id = random_peer_id();

        let message_1 = message(1);
        let message_2 = message(4);

        test_data
            .service
            .handle_network_event(MockEvent::StreamOpened(peer_id.clone(), PROTOCOL))
            .expect("Should handle");

        test_data.service.broadcast(message_1, PROTOCOL);

        test_data.service.broadcast(message_2.clone(), PROTOCOL);

        let expected = (message_2.encode(), peer_id, PROTOCOL);

        assert_eq!(
            test_data
                .network
                .send_message
                .next()
                .await
                .expect("Should receive message"),
            expected,
        );

        test_data.cleanup().await
    }

    #[tokio::test]
    async fn test_send_error() {
        let mut test_data = TestData::prepare().await;

        test_data
            .network
            .send_errors
            .lock()
            .push_back(MockSenderError);

        let peer_id = random_peer_id();

        let message_1 = message(1);
        let message_2 = message(4);

        test_data
            .service
            .handle_network_event(MockEvent::StreamOpened(peer_id.clone(), PROTOCOL))
            .expect("Should handle");

        test_data.service.broadcast(message_1, PROTOCOL);

        test_data.service.broadcast(message_2.clone(), PROTOCOL);

        let expected = (message_2.encode(), peer_id, PROTOCOL);

        assert_eq!(
            test_data
                .network
                .send_message
                .next()
                .await
                .expect("Should receive message"),
            expected,
        );

        test_data.cleanup().await
    }

    #[tokio::test]
    async fn test_notification_received() {
        let mut test_data = TestData::prepare().await;

        let message = message(1);

        test_data
            .service
            .handle_network_event(MockEvent::Messages(vec![(
                PROTOCOL,
                message.clone().encode().into(),
            )]))
            .expect("Should handle");

        assert_eq!(
            test_data
                .messages_from_network
                .next()
                .await
                .expect("Should receive message"),
            message,
        );

        test_data.cleanup().await
    }
}
