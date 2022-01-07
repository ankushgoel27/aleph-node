use crate::new_network::{Data, DataNetwork, SendError};
use aleph_bft::Recipient;
use futures::channel::mpsc;
use std::sync::Arc;
use tokio::{stream::StreamExt, sync::Mutex};

/// For sending arbitrary messages.
pub trait Sender<D: Data>: Sync + Send + Clone {
    fn send(&self, data: D, recipient: Recipient) -> Result<(), SendError>;
}

/// For receiving arbitrary messages.
#[async_trait::async_trait]
pub trait Receiver<D: Data>: Sync + Send {
    async fn next(&mut self) -> Option<D>;
}

/// A bare version of network components.
pub trait Network<D: Data>: Sync + Send {
    type S: Sender<D>;
    type R: Receiver<D>;
    fn sender(&self) -> &Self::S;
    fn receiver(&self) -> Arc<Mutex<Self::R>>;
}

#[async_trait::async_trait]
impl<D: Data, CN: Network<D>> DataNetwork<D> for CN {
    fn send(&self, data: D, recipient: Recipient) -> Result<(), SendError> {
        self.sender().send(data, recipient)
    }
    async fn next(&mut self) -> Option<D> {
        self.receiver().clone().lock_owned().await.next().await
    }
}

#[async_trait::async_trait]
impl<D: Data> Receiver<D> for mpsc::UnboundedReceiver<D> {
    async fn next(&mut self) -> Option<D> {
        StreamExt::next(self).await
    }
}

#[cfg(test)]
mod tests {
    use super::Receiver;
    use futures::channel::mpsc;

    #[tokio::test]
    async fn test_receiver_implementation() {
        let (sender, mut receiver) = mpsc::unbounded();

        let val = 1234;
        sender.unbounded_send(val).unwrap();
        let received = Receiver::<u64>::next(&mut receiver).await;
        assert_eq!(Some(val), received);
    }
}
