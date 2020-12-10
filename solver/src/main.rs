#![allow(dead_code)]

mod batcher;
mod driver;
mod encoding;
mod ethereum;
mod naive_solver;
mod orderbook;
mod settlement;

use reqwest::Url;
use std::time::Duration;
use structopt::StructOpt;

#[derive(Debug, StructOpt)]
struct Arguments {
    #[structopt(flatten)]
    shared: shared_arguments::Arguments,

    #[structopt(long, env = "ORDERBOOK_URL", default_value = "http://localhost:8080")]
    orderbook_url: Url,

    #[structopt(
        long,
        env = "ORDERBOOK_TIMEOUT",
        default_value = "10",
        parse(try_from_str = shared_arguments::duration_from_seconds),
    )]
    orderbook_timeout: Duration,
}

#[tokio::main]
async fn main() {
    let args = Arguments::from_args();
    tracing_setup::initialize(args.shared.log_filter.as_str());
    tracing::info!("running solver with {:#?}", args);
    let orderbook = orderbook::OrderBookApi::new(args.orderbook_url, args.orderbook_timeout);
    // TODO: start driver, for now just fetch orders as placeholder
    tracing::info!("fetching orders");
    let orders = orderbook.get_orders().await;
    tracing::info!(?orders);
}
