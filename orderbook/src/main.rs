use contracts::GPv2Settlement;
use model::{order::OrderUid, DomainSeparator};
use orderbook::{
    account_balances::Web3BalanceFetcher,
    database::{Database, OrderFilter},
    event_updater::EventUpdater,
    orderbook::Orderbook,
    serve_task, verify_deployed_contract_constants,
};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use structopt::StructOpt;
use tokio::task;
use url::Url;

#[derive(Debug, StructOpt)]
struct Arguments {
    #[structopt(flatten)]
    shared: shared::arguments::Arguments,

    #[structopt(long, env = "BIND_ADDRESS", default_value = "0.0.0.0:8080")]
    bind_address: SocketAddr,

    /// Url of the Postgres database. By default connects to locally running postgres.
    #[structopt(long, env = "DB_URL", default_value = "postgresql://")]
    db_url: Url,
}

const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(10);

pub async fn orderbook_maintenance(
    storage: Arc<Orderbook>,
    settlement_contract: GPv2Settlement,
) -> ! {
    loop {
        tracing::debug!("running order book maintenance");
        if let Err(err) = storage.run_maintenance(&settlement_contract).await {
            tracing::error!(?err, "maintenance error");
        }
        tokio::time::delay_for(MAINTENANCE_INTERVAL).await;
    }
}
#[tokio::main]
async fn main() {
    let args = Arguments::from_args();
    shared::tracing::initialize(args.shared.log_filter.as_str());
    tracing::info!("running order book with {:#?}", args);

    let transport = web3::transports::Http::new(args.shared.node_url.as_str())
        .expect("transport creation failed");
    let web3 = web3::Web3::new(transport);
    let settlement_contract = GPv2Settlement::deployed(&web3)
        .await
        .expect("Couldn't load deployed settlement");
    let gp_allowance = settlement_contract
        .allowance_manager()
        .call()
        .await
        .expect("Couldn't get allowance manager address");
    let chain_id = web3
        .eth()
        .chain_id()
        .await
        .expect("Could not get chainId")
        .as_u64();
    verify_deployed_contract_constants(&settlement_contract, chain_id)
        .await
        .expect("Deployed contract constants don't match the ones in this binary");
    let domain_separator =
        DomainSeparator::get_domain_separator(chain_id, settlement_contract.address());
    let database = Database::new(args.db_url.as_str()).expect("failed to create database");
    let event_updater = EventUpdater::new(settlement_contract.clone(), database.clone());
    let balance_fetcher = Web3BalanceFetcher::new(web3.clone(), gp_allowance);
    let orderbook = Arc::new(Orderbook::new(
        domain_separator,
        database,
        event_updater,
        Box::new(balance_fetcher),
    ));
    check_database_connection(orderbook.as_ref()).await;
    let serve_task = serve_task(orderbook.clone(), args.bind_address);
    let maintenance_task = task::spawn(orderbook_maintenance(orderbook, settlement_contract));
    tokio::select! {
        result = serve_task => tracing::error!(?result, "serve task exited"),
        result = maintenance_task => tracing::error!(?result, "maintenance task exited"),
    };
}

async fn check_database_connection(orderbook: &Orderbook) {
    orderbook
        .get_orders(&OrderFilter {
            uid: Some(OrderUid::default()),
            ..Default::default()
        })
        .await
        .expect("failed to connect to database");
}
