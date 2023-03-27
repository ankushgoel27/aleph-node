use futures::channel::mpsc;
use jsonrpsee::{
    core::{error::Error as JsonRpseeError, RpcResult},
    proc_macros::rpc,
    types::error::{CallError, ErrorObject},
};
use serde::Serialize;
use finality_aleph::AlephJustification;
use sp_api::BlockT;
use sp_runtime::traits::NumberFor;
use finality_aleph::{Justification, JustificationTranslator};
use aleph_primitives::BlockNumber;
use sp_runtime::traits::Header;

/// System RPC errors.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Justification argument is malformatted.
    #[error("{0}")]
    MalformattedJustificationArg(String),
    /// Provided block range couldn't be resolved to a list of blocks.
    #[error("Node is not fully functional: {}", .0)]
    FailedJustificationSend(String),
}

// Base code for all system errors.
const BASE_ERROR: i32 = 2000;
// Justification argument is malformatted.
const MALFORMATTED_JUSTIFICATION_ARG_ERROR: i32 = BASE_ERROR + 1;
// AlephNodeApiServer is failed to send JustificationNotification.
const FAILED_JUSTIFICATION_SEND_ERROR: i32 = BASE_ERROR + 2;

impl From<Error> for JsonRpseeError {
    fn from(e: Error) -> Self {
        match e {
            Error::FailedJustificationSend(e) => CallError::Custom(ErrorObject::owned(
                FAILED_JUSTIFICATION_SEND_ERROR,
                e,
                None::<()>,
            )),
            Error::MalformattedJustificationArg(e) => CallError::Custom(ErrorObject::owned(
                MALFORMATTED_JUSTIFICATION_ARG_ERROR,
                e,
                None::<()>,
            )),
        }
        .into()
    }
}

/// Aleph Node RPC API
#[rpc(client, server)]
pub trait AlephNodeApi<Hash, Number> {
    /// Finalize the block with given hash and number using attached signature. Returns the empty string or an error.
    #[method(name = "alephNode_emergencyFinalize")]
    fn aleph_node_emergency_finalize(
        &self,
        justification: Vec<u8>,
        hash: Hash,
        number: Number,
    ) -> RpcResult<()>;
}

/// Aleph Node API implementation
pub struct AlephNode<B, JT>
where
    B: BlockT,
    B::Header: Header<Number = BlockNumber>,
    B::Hash: Serialize + for<'de> serde::Deserialize<'de>,
    NumberFor<B>: Serialize + for<'de> serde::Deserialize<'de>,
    JT: JustificationTranslator<B::Header> + Send + Sync + Clone + 'static,
{
    import_justification_tx: mpsc::UnboundedSender<Justification<B::Header>>,
    justification_translator: JT
}

impl<B, JT> AlephNode<B, JT>
where
    B: BlockT,
    B::Header: Header<Number = BlockNumber>,
    B::Hash: Serialize + for<'de> serde::Deserialize<'de>,
    NumberFor<B>: Serialize + for<'de> serde::Deserialize<'de>,
    JT: JustificationTranslator<B::Header> + Send + Sync + Clone + 'static,

{
    pub fn new(
        import_justification_tx: mpsc::UnboundedSender<Justification<B::Header>>,
        justification_translator: JT,
    ) -> Self {
        AlephNode {
            import_justification_tx,
            justification_translator,
        }
    }
}

impl<B, JT> AlephNodeApiServer<B::Hash, NumberFor<B>> for AlephNode<B, JT>
where
    B: BlockT,
    B::Header: Header<Number = BlockNumber>,
    B::Hash: Serialize + for<'de> serde::Deserialize<'de>,
    NumberFor<B>: Serialize + for<'de> serde::Deserialize<'de>,
    JT: JustificationTranslator<B::Header> + Send + Sync + Clone + 'static,
{
    fn aleph_node_emergency_finalize(
        &self,
        justification: Vec<u8>,
        hash: B::Hash,
        number: NumberFor<B>,
    ) -> RpcResult<()> {
        let justification: AlephJustification =
            AlephJustification::EmergencySignature(justification.try_into().map_err(|_| {
                Error::MalformattedJustificationArg(
                    "Provided justification cannot be converted into correct type".into(),
                )
            })?);
        let justification = self.justification_translator.translate(
            justification,
            hash,
            number,
        ).map_err(|_e| Error::MalformattedJustificationArg("todo".into()))?; // todo - proper error
        self.import_justification_tx.unbounded_send(justification)
            .map_err(|_| {
                Error::FailedJustificationSend(
                    "AlephNodeApiServer failed to send JustifictionNotification via its channel"
                        .into(),
                )
            })?;
        Ok(())
    }
}
