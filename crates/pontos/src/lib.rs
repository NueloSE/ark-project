pub mod event_handler;
pub mod managers;
pub mod storage;

use crate::storage::types::BlockIndexingStatus;
use anyhow::Result;
use ark_starknet::client::{StarknetClient, StarknetClientError};
use ark_starknet::format::to_hex_str;
use event_handler::EventHandler;
use managers::{BlockManager, ContractManager, EventManager, PendingBlockData, TokenManager};
use starknet::core::types::*;
use std::fmt;
use std::sync::Arc;
use storage::types::{ContractType, StorageError};
use storage::Storage;
use tokio::sync::RwLock as AsyncRwLock;
use tracing::{debug, error, info, trace, warn};

pub type IndexerResult<T> = Result<T, IndexerError>;

const ELEMENT_MARKETPLACE_EVENT_HEX: &str =
    "0x351e5a57ea6ca22e3e3cd212680ef7f3b57404609bda942a5e75ba4724b55e0";

const VENTORY_MARKETPLACE_EVENT_HEX: &str =
    "0x1b43f40d55364e989b3a8674460f61ba8f327542298ee6240a54ee2bf7b55bb"; // EventListingBought

const VENTORY_MARKETPLACE_OFFER_ACCEPTED_EVENT_HEX: &str =
    "0xe214ba50bf9d17a50de9ab9f433295bd671144999d5258dbc261cbf1e1c2cc"; // EventOfferAccepted

/// Generic errors for Pontos.
#[derive(Debug)]
pub enum IndexerError {
    StorageError(StorageError),
    Starknet(StarknetClientError),
    Anyhow(String),
}

impl From<StorageError> for IndexerError {
    fn from(e: StorageError) -> Self {
        IndexerError::StorageError(e)
    }
}

impl From<StarknetClientError> for IndexerError {
    fn from(e: StarknetClientError) -> Self {
        IndexerError::Starknet(e)
    }
}

impl From<anyhow::Error> for IndexerError {
    fn from(e: anyhow::Error) -> Self {
        IndexerError::Anyhow(e.to_string())
    }
}

impl fmt::Display for IndexerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IndexerError::StorageError(e) => write!(f, "Storage Error occurred: {}", e),
            IndexerError::Starknet(e) => write!(f, "Starknet Error occurred: {}", e),
            IndexerError::Anyhow(s) => write!(f, "An error occurred: {}", s),
        }
    }
}

impl std::error::Error for IndexerError {}

pub struct PontosConfig {
    pub indexer_version: String,
    pub indexer_identifier: String,
}

pub struct Pontos<S: Storage, C: StarknetClient, E: EventHandler> {
    client: Arc<C>,
    event_handler: Arc<E>,
    config: PontosConfig,
    block_manager: Arc<BlockManager<S>>,
    event_manager: Arc<EventManager<S>>,
    token_manager: Arc<TokenManager<S, C>>,
    contract_manager: Arc<AsyncRwLock<ContractManager<S, C>>>,
    pending_cache: Arc<AsyncRwLock<PendingBlockData>>,
}

impl<S: Storage, C: StarknetClient, E: EventHandler + Send + Sync> Pontos<S, C, E> {
    pub fn new(
        client: Arc<C>,
        storage: Arc<S>,
        event_handler: Arc<E>,
        config: PontosConfig,
    ) -> Self {
        Pontos {
            config,
            client: Arc::clone(&client),
            event_handler: Arc::clone(&event_handler),
            block_manager: Arc::new(BlockManager::new(Arc::clone(&storage))),
            event_manager: Arc::new(EventManager::new(Arc::clone(&storage))),
            token_manager: Arc::new(TokenManager::new(Arc::clone(&storage), Arc::clone(&client))),
            // Contract manager has internal cache, so some functions are using `&mut self`.
            // For this reason, we must protect the write operations in order to share
            // the cache with any possible thread using `index_block_range` of this instance.
            contract_manager: Arc::new(AsyncRwLock::new(ContractManager::new(
                Arc::clone(&storage),
                Arc::clone(&client),
            ))),
            pending_cache: Arc::new(AsyncRwLock::new(PendingBlockData::new())),
        }
    }

    /// Starts a loop to only index the pending block.
    pub async fn index_pending(&self) -> IndexerResult<()> {
        loop {
            let mut cache = self.pending_cache.write().await;

            let (pending_ts, txs) = match self
                .client
                .block_txs_hashes(BlockId::Tag(BlockTag::Pending))
                .await
            {
                Ok((ts, txs)) => (ts, txs),
                Err(e) => {
                    error!("Error while fetching pending block txs: {:?}", e);
                    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                    continue;
                }
            };

            if cache.get_timestamp() == 0 {
                cache.set_timestamp(pending_ts);
            }

            debug!("Pending block {} with {} txs", pending_ts, txs.len());

            let previous_loop_ts = cache.get_timestamp();

            // If the timestamp is different from the previous loop,
            // we must first ensure we've fetched and processed all the transactions
            // of the previous pending block, which is now the "Latest".
            if pending_ts != previous_loop_ts {
                debug!("ts differ! {} {}", pending_ts, previous_loop_ts);
                // Get the latest block number, generated by the sequencer, which is
                // expected to be the one we just processed.
                let block_number = match self.client.block_number().await {
                    Ok(n) => n,
                    Err(e) => {
                        error!("Error while fetching latest block number: {:?}", e);
                        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                        continue;
                    }
                };

                self.event_handler.on_new_latest_block(block_number).await;

                info!(
                    "Pending block {} is now latest block number #{}",
                    previous_loop_ts, block_number
                );

                // Setup the local variables to directly start the pending block
                // indexation instead of waiting the next tick.
                cache.set_timestamp(pending_ts);
                cache.clear_tx_hashes();
            }

            // TODO: make this configurable?
            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        }
    }

    pub async fn index_contract_events(
        &self,
        from_block: Option<BlockId>,
        to_block: Option<BlockId>,
        contract_address: FieldElement,
        chain_id: &str,
    ) -> IndexerResult<()> {
        let mut continuation_token: Option<String> = None;

        loop {
            let result = self
                .client
                .fetch_events(
                    from_block,
                    to_block,
                    self.event_manager.keys_selector(),
                    Some(contract_address),
                    continuation_token,
                )
                .await?;

            let mut current_block_number: u64 = 0;
            let mut current_block_timestamp: u64 = 0;

            for (block_number, events) in result.events {
                if current_block_number != block_number {
                    current_block_number = block_number;

                    match self.client.block_time(BlockId::Number(block_number)).await {
                        Ok(ts) => {
                            current_block_timestamp = ts;
                            self.process_events(events, current_block_timestamp, chain_id)
                                .await?;
                        }
                        Err(e) => {
                            error!("Error while fetching block timestamp: {:?}", e);
                        }
                    };
                } else {
                    self.process_events(events, current_block_timestamp, chain_id)
                        .await?;
                }
            }

            if result.continuation_token.is_none() {
                break;
            } else {
                continuation_token = result.continuation_token;
                continue;
            }
        }

        Ok(())
    }

    /// If "Latest" is used for the `to_block`,
    /// this function will only index the latest block
    /// that is not pending.
    /// If you use this on latest, be sure to don't have any
    /// other pontos instance running `index_pending` as you may
    /// deal with overlaps or at least check db registers first.
    pub async fn index_block_range(
        &self,
        from_block: BlockId,
        to_block: BlockId,
        do_force: bool,
        chain_id: &str,
    ) -> IndexerResult<()> {
        let mut current_u64 = self.client.block_id_to_u64(&from_block).await?;
        let to_u64 = self.client.block_id_to_u64(&to_block).await?;
        let from_u64 = current_u64;

        // Some contracts are causing too much recursion for the Cairo VM.
        // This is restarting the full node (Juno) as it is OOM and is shutdown by the OS.
        // To mitigate this problem before scaling the full node up,
        // we setup a `max_attempt` to reach the full node before skipping
        // the entire block.
        // Currently, we observed that the node almost always reponds after the
        // second attempt.
        let max_attempt = 5;
        let mut attempt = 0;

        loop {
            trace!("Indexing block range: {} {}", current_u64, to_u64);

            if current_u64 > to_u64 {
                info!("End of indexing block range");
                break;
            }

            let block_ts = match self.client.block_time(BlockId::Number(current_u64)).await {
                Ok(ts) => ts,
                Err(e) => {
                    error!(
                        "Attempt #{} - Couldn't get timestamp for block {}: {:?}",
                        attempt + 1,
                        current_u64,
                        e
                    );
                    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                    attempt += 1;

                    if attempt > max_attempt {
                        warn!(
                            "Skipping block {} as timestamp is not available",
                            current_u64
                        );
                        current_u64 += 1;
                    }

                    continue;
                }
            };

            if self
                .block_manager
                .should_skip_indexing(
                    current_u64,
                    block_ts,
                    self.config.indexer_version.clone(),
                    do_force,
                )
                .await?
            {
                info!("Skipping block {}", current_u64);
                current_u64 += 1;
                continue;
            }

            self.event_handler
                .on_block_processing(block_ts, Some(current_u64))
                .await;

            // Set block as processing.
            self.block_manager
                .set_block_info(
                    current_u64,
                    block_ts,
                    self.config.indexer_version.clone(),
                    self.config.indexer_identifier.clone(),
                    BlockIndexingStatus::Processing,
                )
                .await?;

            let blocks_events = match self
                .client
                .fetch_all_block_events(
                    BlockId::Number(current_u64),
                    self.event_manager.keys_selector(),
                )
                .await
            {
                Ok(events) => events,
                Err(e) => {
                    error!("Error while fetching events: {:?}", e);
                    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                    continue;
                }
            };

            let total_events_count: usize = blocks_events.values().map(|events| events.len()).sum();
            info!(
                "✨ Processing block {}. Total Events Count: {}.",
                current_u64, total_events_count
            );

            for (_, events) in blocks_events {
                self.process_events(events, block_ts, chain_id).await?;
            }

            self.block_manager
                .set_block_info(
                    current_u64,
                    block_ts,
                    self.config.indexer_version.clone(),
                    self.config.indexer_identifier.clone(),
                    BlockIndexingStatus::Terminated,
                )
                .await?;

            let progress = if to_u64 == from_u64 {
                if current_u64 == to_u64 {
                    100.0
                } else {
                    0.0
                }
            } else {
                ((current_u64 - from_u64) as f64 / (to_u64 - from_u64) as f64) * 100.0
            };

            self.event_handler
                .on_block_processed(current_u64, progress)
                .await;

            current_u64 += 1;
        }

        self.event_handler.on_indexation_range_completed().await;

        Ok(())
    }

    async fn process_element_sale(
        &self,
        event: EmittedEvent,
        block_timestamp: u64,
        chain_id: &str,
    ) -> Result<()> {
        let mut token_sale_event = self
            .event_manager
            .format_element_sale_event(&event, block_timestamp)
            .await?;

        let contract_addr = FieldElement::from_hex_be(
            token_sale_event.nft_contract_address.as_str(),
        )
        .map_err(|e| {
            error!("Invalid NFT contract address format: {:?}", e);
            e
        })?;

        let contract_type = match self
            .contract_manager
            .write()
            .await
            .identify_contract(contract_addr, block_timestamp, chain_id)
            .await
        {
            Ok(info) => info,
            Err(e) => {
                error!(
                    "Error while identifying contract {}: {:?}",
                    token_sale_event.nft_contract_address, e
                );
                return Ok(());
            }
        };

        if contract_type == ContractType::Other {
            debug!(
                "Contract identified as OTHER: {}",
                token_sale_event.nft_contract_address
            );
            return Ok(());
        }

        token_sale_event.nft_type = Some(contract_type.to_string());
        self.event_manager
            .register_sale_event(&token_sale_event, block_timestamp)
            .await?;

        Ok(())
    }

    async fn process_ventory_sale_or_accepted_offer_event(
        &self,
        event: EmittedEvent,
        block_timestamp: u64,
        chain_id: &str,
    ) -> Result<()> {
        info!("Processing Ventory Sale or Accepted Offer event...");

        let mut token_sale_event = self
            .event_manager
            .format_ventory_sale_or_accepted_offer_event(&event, block_timestamp)
            .await?;

        let contract_addr = FieldElement::from_hex_be(
            token_sale_event.nft_contract_address.as_str(),
        )
        .map_err(|e| {
            error!("Invalid NFT contract address format: {:?}", e);
            e
        })?;

        let contract_type = match self
            .contract_manager
            .write()
            .await
            .identify_contract(contract_addr, block_timestamp, chain_id)
            .await
        {
            Ok(info) => info,
            Err(e) => {
                error!(
                    "Error while identifying contract {}: {:?}",
                    token_sale_event.nft_contract_address, e
                );
                return Ok(());
            }
        };

        if contract_type == ContractType::Other {
            debug!(
                "Contract identified as OTHER: {}",
                token_sale_event.nft_contract_address
            );
            return Ok(());
        }

        token_sale_event.nft_type = Some(contract_type.to_string());
        self.event_manager
            .register_sale_event(&token_sale_event, block_timestamp)
            .await?;

        Ok(())
    }

    async fn process_marketplace_event(
        &self,
        event: EmittedEvent,
        block_timestamp: u64,
        chain_id: &str,
    ) -> Result<()> {
        let element_sale_event_name = FieldElement::from_hex_be(ELEMENT_MARKETPLACE_EVENT_HEX)?;
        let ventory_sale_event_name = FieldElement::from_hex_be(VENTORY_MARKETPLACE_EVENT_HEX)?;
        let ventory_offer_accepted_event_name =
            FieldElement::from_hex_be(VENTORY_MARKETPLACE_OFFER_ACCEPTED_EVENT_HEX)?;

        if let Some(event_name) = event.keys.first() {
            info!("Processing marketplace event: {:?}", event_name);

            match event_name {
                name if name == &element_sale_event_name => {
                    self.process_element_sale(event, block_timestamp, chain_id)
                        .await?
                }
                name if name == &ventory_sale_event_name
                    || name == &ventory_offer_accepted_event_name =>
                {
                    self.process_ventory_sale_or_accepted_offer_event(
                        event,
                        block_timestamp,
                        chain_id,
                    )
                    .await?
                }
                _ => (),
            }
        }

        Ok(())
    }

    async fn process_nft_transfers(
        &self,
        event: EmittedEvent,
        block_timestamp: u64,
        contract_address: FieldElement,
        chain_id: &str,
    ) -> Result<()> {
        let contract_address_hex = to_hex_str(&contract_address);
        let contract_type = self
            .contract_manager
            .write()
            .await
            .identify_contract(contract_address, block_timestamp, chain_id)
            .await
            .map_err(|e| {
                error!(
                    "Error while identifying contract {}: {:?}",
                    contract_address_hex, e
                );
                e
            })?;

        if contract_type == ContractType::Other {
            debug!("Contract identified as OTHER: {}", contract_address_hex);
            return Ok(());
        }

        info!(
            "Processing event... Block Id: {:?}, Tx Hash: 0x{:064x}, contract_type: {:?}",
            event.block_number, event.transaction_hash, contract_type
        );

        let (token_id, token_event) = self
            .event_manager
            .format_and_register_event(&event, contract_type, block_timestamp)
            .await
            .map_err(|err| {
                error!("Error while registering event {:?}\n{:?}", err, event);
                err
            })?;

        self.token_manager
            .format_and_register_token(&token_id, &token_event, block_timestamp, event.block_number)
            .await
            .map_err(|err| {
                error!("Can't format token {:?}\ntevent: {:?}", err, token_event);
                err
            })?;

        Ok(())
    }

    /// Inner function to process events.
    async fn process_events(
        &self,
        events: Vec<EmittedEvent>,
        block_timestamp: u64,
        chain_id: &str,
    ) -> IndexerResult<()> {
        let marketplace_contracts = [
            FieldElement::from_hex_be(
                "0x04d8bb956e6bd7a50fcb8b49d8e9fd8269cfadbeb73f457fd6d3fc1dff4b879e", // Element Marketplace
            )
            .unwrap(),
            FieldElement::from_hex_be(
                "0x008755a98ccf7d25e69aa90ef3b73b07c470ba4ec6391b0b0c7c598f992c3fee", // Ventory Marketplace
            )
            .unwrap(),
        ];

        for e in events {
            let contract_address = e.from_address;
            let is_marketplace_event = marketplace_contracts.contains(&contract_address);

            if is_marketplace_event {
                if let Err(e) = self
                    .process_marketplace_event(e, block_timestamp, chain_id)
                    .await
                {
                    error!("Error while processing marketplace event: {:?}", e);
                }
            } else if let Err(e) = self
                .process_nft_transfers(e, block_timestamp, contract_address, chain_id)
                .await
            {
                error!("Error while processing NFT transfers: {:?}", e);
            }
        }

        Ok(())
    }
}
