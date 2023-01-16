use anyhow::anyhow;
use log::info;
use primitives::{BlockNumber, EraIndex, SessionIndex};
use subxt::{blocks::ExtrinsicEvents, ext::sp_runtime::traits::Hash, Config};

use crate::{
    connections::{AsConnection, TxInfo},
    pallets::{elections::ElectionsApi, staking::StakingApi},
    AlephConfig, BlockHash,
};

/// Block info API.
#[async_trait::async_trait]
pub trait BlocksApi {
    /// Returns the first block of a session.
    /// * `session` - number of the session to query the first block from
    async fn first_block_of_session(
        &self,
        session: SessionIndex,
    ) -> anyhow::Result<Option<BlockHash>>;

    /// Returns hash of a given block if the given block exists, otherwise `None`
    /// * `block` - number of the block
    async fn get_block_hash(&self, block: BlockNumber) -> anyhow::Result<Option<BlockHash>>;

    /// Returns the most recent block from the current best chain.
    async fn get_best_block(&self) -> anyhow::Result<Option<BlockNumber>>;

    /// Returns the most recent block from the finalized chain.
    async fn get_finalized_block_hash(&self) -> anyhow::Result<BlockHash>;

    /// Returns number of a given block hash, if the given block exists, otherwise `None`
    /// This is version that returns `Result`
    /// * `block` - hash of the block to query its number
    async fn get_block_number(&self, block: BlockHash) -> anyhow::Result<Option<BlockNumber>>;

    /// Returns number of a given block hash, if the given block exists, otherwise `None`
    /// * `block` - hash of the block to query its number
    async fn get_block_number_opt(
        &self,
        block: Option<BlockHash>,
    ) -> anyhow::Result<Option<BlockNumber>>;

    /// Fetch all events that corresponds to the transaction identified by `tx_info`.
    async fn get_tx_events(&self, tx_info: TxInfo) -> anyhow::Result<ExtrinsicEvents<AlephConfig>>;
}

/// Interaction logic between pallet session and pallet staking.
#[async_trait::async_trait]
pub trait SessionEraApi {
    /// Returns which era given session is.
    /// * `session` - session index
    async fn get_active_era_for_session(&self, session: SessionIndex) -> anyhow::Result<EraIndex>;
}

#[async_trait::async_trait]
impl<C: AsConnection + Sync> BlocksApi for C {
    async fn first_block_of_session(
        &self,
        session: SessionIndex,
    ) -> anyhow::Result<Option<BlockHash>> {
        let period = self.get_session_period().await?;
        let block_num = period * session;

        self.get_block_hash(block_num).await
    }

    async fn get_block_hash(&self, block: BlockNumber) -> anyhow::Result<Option<BlockHash>> {
        info!(target: "aleph-client", "querying block hash for number #{}", block);
        self.as_connection()
            .as_client()
            .rpc()
            .block_hash(Some(block.into()))
            .await
            .map_err(|e| e.into())
    }

    async fn get_best_block(&self) -> anyhow::Result<Option<BlockNumber>> {
        self.get_block_number_opt(None).await
    }

    async fn get_finalized_block_hash(&self) -> anyhow::Result<BlockHash> {
        self.as_connection()
            .as_client()
            .rpc()
            .finalized_head()
            .await
            .map_err(|e| e.into())
    }

    async fn get_block_number_opt(
        &self,
        block: Option<BlockHash>,
    ) -> anyhow::Result<Option<BlockNumber>> {
        self.as_connection()
            .as_client()
            .rpc()
            .header(block)
            .await
            .map(|maybe_header| maybe_header.map(|header| header.number))
            .map_err(|e| e.into())
    }

    async fn get_block_number(&self, block: BlockHash) -> anyhow::Result<Option<BlockNumber>> {
        self.get_block_number_opt(Some(block)).await
    }

    async fn get_tx_events(&self, tx_info: TxInfo) -> anyhow::Result<ExtrinsicEvents<AlephConfig>> {
        let block_body = self
            .as_connection()
            .as_client()
            .blocks()
            .at(Some(tx_info.block_hash))
            .await?
            .body()
            .await?;

        let extrinsic_events = block_body
            .extrinsics()
            .find(|tx| tx_info.tx_hash == <AlephConfig as Config>::Hashing::hash_of(&tx.bytes()))
            .ok_or(anyhow!("Couldn't find the transaction in the block."))?
            .events()
            .await
            .map_err(|e| anyhow!("Couldn't fetch events for the transaction: {e:?}"))?;

        Ok(extrinsic_events)
    }
}

#[async_trait::async_trait]
impl<C: AsConnection + Sync> SessionEraApi for C {
    async fn get_active_era_for_session(&self, session: SessionIndex) -> anyhow::Result<EraIndex> {
        let block = self.first_block_of_session(session).await?;
        Ok(self.get_active_era(block).await)
    }
}