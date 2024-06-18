//! Command for debugging execution.

use crate::{
    args::{get_secret_key, NetworkArgs},
    commands::common::{AccessRights, Environment, EnvironmentArgs},
    utils::get_single_header,
};
use clap::Parser;
use futures::{stream::select as stream_select, StreamExt};
use reth_beacon_consensus::EthBeaconConsensus;
use reth_cli_runner::CliContext;
use reth_config::Config;
use reth_consensus::Consensus;
use reth_db::DatabaseEnv;
use reth_db_api::database::Database;
use reth_downloaders::{
    bodies::bodies::BodiesDownloaderBuilder,
    headers::reverse_headers::ReverseHeadersDownloaderBuilder,
};
use reth_exex::ExExManagerHandle;
use reth_network::{NetworkEvents, NetworkHandle};
use reth_network_api::NetworkInfo;
use reth_network_p2p::{bodies::client::BodiesClient, headers::client::HeadersClient};
use reth_node_core::args::ExperimentalArgs;
use reth_node_ethereum::EthExecutorProvider;
use reth_primitives::{BlockHashOrNumber, BlockNumber, B256};
use reth_provider::{
    BlockExecutionWriter, ChainSpecProvider, ProviderFactory, StageCheckpointReader,
};
use reth_prune_types::PruneModes;
use reth_stages::{
    sets::DefaultStages,
    stages::{ExecutionStage, ExecutionStageThresholds},
    Pipeline, StageId, StageSet,
};
use reth_static_file::StaticFileProducer;
use reth_tasks::TaskExecutor;
use std::{net::SocketAddr, path::PathBuf, sync::Arc};
use tokio::sync::watch;
use tracing::*;

#[cfg(not(feature = "compiler"))]
use reth_node_ethereum::EthEvmConfig;

/// `reth debug execution` command
#[derive(Debug, Parser)]
pub struct Command {
    #[command(flatten)]
    env: EnvironmentArgs,

    #[command(flatten)]
    network: NetworkArgs,

    /// The maximum block height.
    #[arg(long)]
    pub to: u64,

    /// The block interval for sync and unwind.
    /// Defaults to `1000`.
    #[arg(long, default_value = "1000")]
    pub interval: u64,

    /// All experimental arguments
    #[command(flatten)]
    pub experimental: ExperimentalArgs,
}

impl Command {
    #[cfg(feature = "compiler")]
    async fn build_evm(
        &self,
        data_dir: reth_node_core::dirs::ChainPath<reth_node_core::dirs::DataDirPath>,
        task_executor: &TaskExecutor,
    ) -> eyre::Result<EthExecutorProvider<crate::compiler::CompilerEvmConfig>> {
        use reth_evm_compiler::*;
        use std::sync::atomic::{AtomicBool, Ordering};

        let compiler_config = &self.experimental.compiler;
        let compiler_dir = data_dir.compiler();
        if !compiler_config.compiler {
            tracing::debug!("EVM bytecode compiler is disabled");
            return Ok(EthExecutorProvider::new(
                self.env.chain.clone(),
                crate::compiler::CompilerEvmConfig::disabled(),
            ));
        }
        tracing::info!("EVM bytecode compiler initialized");

        let out_dir =
            compiler_config.out_dir.clone().unwrap_or_else(|| compiler_dir.join("artifacts"));
        let mut compiler = EvmParCompiler::new(out_dir.clone())?;

        let contracts_path = compiler_config
            .contracts_file
            .clone()
            .unwrap_or_else(|| compiler_dir.join("contracts.toml"));
        let contracts_config = ContractsConfig::load(&contracts_path)?;

        let done = Arc::new(AtomicBool::new(false));
        let done2 = done.clone();
        let handle = task_executor.spawn_blocking(async move {
            if let Err(err) = compiler.run_to_end(&contracts_config) {
                tracing::error!(%err, "failed to run compiler");
            }
            done2.store(true, Ordering::Relaxed);
        });
        if compiler_config.block_on_compiler {
            tracing::info!("Blocking on EVM bytecode compiler");
            handle.await?;
            tracing::info!("Done blocking on EVM bytecode compiler");
        }
        Ok(EthExecutorProvider::new(
            self.env.chain.clone(),
            crate::compiler::CompilerEvmConfig::new(done, out_dir),
        ))
    }

    async fn build_pipeline<DB, Client>(
        &self,
        config: &Config,
        _data_dir: reth_node_core::dirs::ChainPath<reth_node_core::dirs::DataDirPath>,
        client: Client,
        consensus: Arc<dyn Consensus>,
        provider_factory: ProviderFactory<DB>,
        task_executor: &TaskExecutor,
        static_file_producer: StaticFileProducer<DB>,
    ) -> eyre::Result<Pipeline<DB>>
    where
        DB: Database + Unpin + Clone + 'static,
        Client: HeadersClient + BodiesClient + Clone + 'static,
    {
        // building network downloaders using the fetch client
        let header_downloader = ReverseHeadersDownloaderBuilder::new(config.stages.headers)
            .build(client.clone(), Arc::clone(&consensus))
            .into_task_with(task_executor);

        let body_downloader = BodiesDownloaderBuilder::new(config.stages.bodies)
            .build(client, Arc::clone(&consensus), provider_factory.clone())
            .into_task_with(task_executor);

        let stage_conf = &config.stages;
        let prune_modes = config.prune.clone().map(|prune| prune.segments).unwrap_or_default();

        let (tip_tx, tip_rx) = watch::channel(B256::ZERO);

        // TODO: fix this
        #[cfg(not(feature = "compiler"))]
        let executor = EthExecutorProvider::new(self.env.chain.clone(), EthEvmConfig::default());
        #[cfg(feature = "compiler")]
        let executor = self.build_evm(_data_dir, task_executor).await?;

        let pipeline = Pipeline::builder()
            .with_tip_sender(tip_tx)
            .add_stages(
                DefaultStages::new(
                    provider_factory.clone(),
                    tip_rx,
                    Arc::clone(&consensus),
                    header_downloader,
                    body_downloader,
                    executor.clone(),
                    stage_conf.clone(),
                    prune_modes.clone(),
                )
                .set(ExecutionStage::new(
                    executor,
                    ExecutionStageThresholds {
                        max_blocks: None,
                        max_changes: None,
                        max_cumulative_gas: None,
                        max_duration: None,
                    },
                    stage_conf.execution_external_clean_threshold(),
                    prune_modes,
                    ExExManagerHandle::empty(),
                )),
            )
            .build(provider_factory, static_file_producer);

        Ok(pipeline)
    }

    async fn build_network(
        &self,
        config: &Config,
        task_executor: TaskExecutor,
        provider_factory: ProviderFactory<Arc<DatabaseEnv>>,
        network_secret_path: PathBuf,
        default_peers_path: PathBuf,
    ) -> eyre::Result<NetworkHandle> {
        let secret_key = get_secret_key(&network_secret_path)?;
        let network = self
            .network
            .network_config(config, provider_factory.chain_spec(), secret_key, default_peers_path)
            .with_task_executor(Box::new(task_executor))
            .listener_addr(SocketAddr::new(self.network.addr, self.network.port))
            .discovery_addr(SocketAddr::new(
                self.network.discovery.addr,
                self.network.discovery.port,
            ))
            .build(provider_factory)
            .start_network()
            .await?;
        info!(target: "reth::cli", peer_id = %network.peer_id(), local_addr = %network.local_addr(), "Connected to P2P network");
        debug!(target: "reth::cli", peer_id = ?network.peer_id(), "Full peer ID");
        Ok(network)
    }

    async fn fetch_block_hash<Client: HeadersClient>(
        &self,
        client: Client,
        block: BlockNumber,
    ) -> eyre::Result<B256> {
        info!(target: "reth::cli", ?block, "Fetching block from the network.");
        loop {
            match get_single_header(&client, BlockHashOrNumber::Number(block)).await {
                Ok(tip_header) => {
                    info!(target: "reth::cli", ?block, "Successfully fetched block");
                    return Ok(tip_header.hash())
                }
                Err(error) => {
                    error!(target: "reth::cli", ?block, %error, "Failed to fetch the block. Retrying...");
                }
            }
        }
    }

    /// Execute `execution-debug` command
    pub async fn execute(self, ctx: CliContext) -> eyre::Result<()> {
        let Environment { provider_factory, config, data_dir } = self.env.init(AccessRights::RW)?;

        let consensus: Arc<dyn Consensus> =
            Arc::new(EthBeaconConsensus::new(provider_factory.chain_spec()));

        // Configure and build network
        let network_secret_path =
            self.network.p2p_secret_key.clone().unwrap_or_else(|| data_dir.p2p_secret());
        let network = self
            .build_network(
                &config,
                ctx.task_executor.clone(),
                provider_factory.clone(),
                network_secret_path,
                data_dir.known_peers(),
            )
            .await?;

        let static_file_producer =
            StaticFileProducer::new(provider_factory.clone(), PruneModes::default());

        // Configure the pipeline
        let fetch_client = network.fetch_client().await?;
        let mut pipeline = self
            .build_pipeline(
                &config,
                data_dir,
                fetch_client.clone(),
                Arc::clone(&consensus),
                provider_factory.clone(),
                &ctx.task_executor,
                static_file_producer,
            )
            .await?;

        let provider = provider_factory.provider()?;

        let latest_block_number =
            provider.get_stage_checkpoint(StageId::Finish)?.map(|ch| ch.block_number);
        if latest_block_number.unwrap_or_default() >= self.to {
            info!(target: "reth::cli", latest = latest_block_number, "Nothing to run");
            return Ok(())
        }

        let pipeline_events = pipeline.events();
        let events = stream_select(
            network.event_listener().map(Into::into),
            pipeline_events.map(Into::into),
        );
        ctx.task_executor.spawn_critical(
            "events task",
            reth_node_events::node::handle_events(
                Some(network.clone()),
                latest_block_number,
                events,
                provider_factory.db_ref().clone(),
            ),
        );

        let mut current_max_block = latest_block_number.unwrap_or_default();
        while current_max_block < self.to {
            let next_block = current_max_block + 1;
            let target_block = self.to.min(current_max_block + self.interval);
            let target_block_hash =
                self.fetch_block_hash(fetch_client.clone(), target_block).await?;

            // Run the pipeline
            info!(target: "reth::cli", from = next_block, to = target_block, tip = ?target_block_hash, "Starting pipeline");
            pipeline.set_tip(target_block_hash);
            let result = pipeline.run_loop().await?;
            trace!(target: "reth::cli", from = next_block, to = target_block, tip = ?target_block_hash, ?result, "Pipeline finished");

            // Unwind the pipeline without committing.
            {
                provider_factory
                    .provider_rw()?
                    .take_block_and_execution_range(next_block..=target_block)?;
            }

            // Update latest block
            current_max_block = target_block;
        }

        Ok(())
    }
}
