use reth_evm::ConfigureEvm;
use reth_network_api::NetworkInfo;
use reth_provider::{CanonStateSubscriptions, FullRpcProvider};
use reth_rpc_builder::{
    EthApiBuilder, EthApiBuilderCtx, FeeHistoryCacheBuilder, GasPriceOracleBuilder,
};
use reth_rpc_eth_api::EthApi;
use reth_tasks::{pool::BlockingTaskPool, TaskSpawner};
use reth_transaction_pool::TransactionPool;

/// Ethereum layer one `eth` RPC server builder.
#[derive(Default, Debug, Clone, Copy)]
pub struct EthApiBuild;

impl<Provider, Pool, EvmConfig, Network, Tasks, Events>
    EthApiBuilder<Provider, Pool, EvmConfig, Network, Tasks, Events> for EthApiBuild
where
    Provider: FullRpcProvider,
    Pool: TransactionPool + 'static,
    Network: NetworkInfo + 'static,
    Tasks: TaskSpawner + 'static,
    Events: CanonStateSubscriptions,
    EvmConfig: ConfigureEvm,
{
    type Server = EthApi<Provider, Pool, Network, EvmConfig>;

    fn build(
        self,
        ctx: EthApiBuilderCtx<'_, Provider, Pool, EvmConfig, Network, Tasks, Events>,
    ) -> Self::Server {
        let gas_oracle = GasPriceOracleBuilder::build(&ctx);
        let fee_history_cache = FeeHistoryCacheBuilder::build(&ctx);

        let EthApiBuilderCtx {
            provider,
            pool,
            network,
            evm_config,
            executor,
            cache,
            config,
            raw_transaction_forwarder,
            ..
        } = ctx;

        EthApi::with_spawner(
            provider,
            pool,
            network,
            cache,
            gas_oracle,
            config.rpc_gas_cap,
            executor,
            BlockingTaskPool::build().expect("failed to build blocking task pool"),
            fee_history_cache,
            evm_config,
            raw_transaction_forwarder,
        )
    }
}