use {
    crate::{
        boundary,
        domain::{auction, eth, liquidity, order, solution},
    },
    anyhow::{Context as _, Result},
    ethereum_types::{H160, U256},
    model::order::{OrderKind, OrderUid},
    shared::{
        http_solver::{
            gas_model::GasModel,
            model::{
                AmmModel,
                AmmParameters,
                BatchAuctionModel,
                ConcentratedPoolParameters,
                ConstantProductPoolParameters,
                MetadataModel,
                OrderModel,
                SettledBatchAuctionModel,
                StablePoolParameters,
                TokenAmount,
                TokenInfoModel,
                WeightedPoolTokenData,
                WeightedProductPoolParameters,
            },
            DefaultHttpSolverApi,
            HttpSolverApi,
            SolverConfig,
        },
        sources::uniswap_v3::{
            graph_api::Token,
            pool_fetching::{PoolInfo, PoolState, PoolStats},
        },
    },
    std::collections::BTreeMap,
};

pub struct Legacy {
    solver: DefaultHttpSolverApi,
    weth: eth::WethAddress,
}

impl Legacy {
    pub fn new(config: crate::domain::solver::legacy::Config) -> Self {
        let solve_path = config.endpoint.path().to_owned();
        let mut base = config.endpoint;
        base.set_path("");

        Self {
            solver: DefaultHttpSolverApi {
                name: config.solver_name,
                network_name: format!("{:?}", config.chain_id),
                chain_id: config.chain_id.value().as_u64(),
                base,
                client: reqwest::Client::new(),
                solve_path,
                config: SolverConfig {
                    // Note that we unconditionally set this to "true". This is
                    // because the auction that we are solving for already
                    // contains which tokens can and can't be internalized,
                    // and we don't need to duplicate this setting here. Ergo,
                    // in order to disable internalization, the driver would be
                    // configured to have 0 trusted tokens.
                    use_internal_buffers: Some(true),
                    ..Default::default()
                },
            },
            weth: config.weth,
        }
    }

    pub async fn solve(&self, auction: auction::Auction) -> Result<solution::Solution> {
        let (mapping, auction_model) = to_boundary_auction(&auction, self.weth);
        let solving_time = (auction.deadline - chrono::Utc::now()).to_std()?;
        let solution = self.solver.solve(&auction_model, solving_time).await?;
        to_domain_solution(&solution, mapping)
    }
}

/// Mapping state used for marshalling domain auctions and solutions to and from
/// their legacy HTTP solver DTO representations. This is needed becuase the
/// legacy HTTP solver API uses arbirtary indices for identifying orders and
/// AMMs that need to be back-referenced to auction domain values.
#[derive(Default)]
struct Mapping<'a> {
    orders: Vec<Order<'a>>,
    amms: BTreeMap<H160, &'a liquidity::Liquidity>,
}

enum Order<'a> {
    Protocol(&'a order::Order),
    Liquidity(
        &'a liquidity::Liquidity,
        &'a liquidity::limit_order::LimitOrder,
    ),
}

fn to_boundary_auction(
    auction: &auction::Auction,
    weth: eth::WethAddress,
) -> (Mapping, BatchAuctionModel) {
    let gas = GasModel {
        native_token: weth.0,
        gas_price: auction.gas_price.0 .0.to_f64_lossy(),
    };

    let mut mapping = Mapping::default();
    let mut model = BatchAuctionModel {
        tokens: auction
            .tokens
            .iter()
            .map(|(address, info)| {
                (
                    address.0,
                    TokenInfoModel {
                        decimals: info.decimals,
                        alias: info.symbol.clone(),
                        external_price: info
                            .reference_price
                            .map(|price| price.0 .0.to_f64_lossy() / 1e18),
                        internal_buffer: Some(info.available_balance),
                        accepted_for_internalization: info.trusted,
                        ..Default::default()
                    },
                )
            })
            .collect(),
        metadata: Some(MetadataModel {
            environment: None,
            auction_id: auction.id.as_ref().and_then(|id| id.0.parse().ok()),
            run_id: None,
            gas_price: Some(gas.gas_price),
            native_token: Some(weth.0),
        }),
        ..Default::default()
    };

    for order in &auction.orders {
        let index = mapping.orders.len();
        mapping.orders.push(Order::Protocol(order));
        model.orders.insert(
            index,
            OrderModel {
                id: Some(OrderUid(order.uid.0)),
                sell_token: order.sell.token.0,
                buy_token: order.buy.token.0,
                sell_amount: order.sell.amount,
                buy_amount: order.buy.amount,
                allow_partial_fill: order.partially_fillable,
                is_sell_order: order.side == order::Side::Sell,
                fee: TokenAmount {
                    amount: order.fee().amount,
                    token: order.fee().token.0,
                },
                cost: gas.gp_order_cost(),
                is_liquidity_order: order.class == order::Class::Liquidity,
                is_mature: true,
                // Auctions for a /quote request don't have an id and always contain exactly a
                // single user order that is mandatory to be matched.
                mandatory: auction.id.is_none(),
                has_atomic_execution: false,
                reward: order.reward.0,
            },
        );
    }

    for liquidity in &auction.liquidity {
        let cost = TokenAmount {
            amount: U256::from_f64_lossy(liquidity.gas.0.to_f64_lossy() * gas.gas_price),
            token: weth.0,
        };

        let (parameters, fee) = match &liquidity.state {
            liquidity::State::ConstantProduct(state) => (
                AmmParameters::ConstantProduct(ConstantProductPoolParameters {
                    reserves: [
                        (
                            state.reserves.get().0.token.0,
                            state.reserves.get().0.amount,
                        ),
                        (
                            state.reserves.get().1.token.0,
                            state.reserves.get().1.amount,
                        ),
                    ]
                    .into_iter()
                    .collect(),
                }),
                to_big_rational(&state.fee),
            ),
            liquidity::State::WeightedProduct(state) => (
                AmmParameters::WeightedProduct(WeightedProductPoolParameters {
                    reserves: state
                        .reserves
                        .iter()
                        .map(|reserve| {
                            (
                                reserve.asset.token.0,
                                WeightedPoolTokenData {
                                    balance: reserve.asset.amount,
                                    weight: to_big_rational(&reserve.weight),
                                },
                            )
                        })
                        .collect(),
                }),
                to_big_rational(&state.fee),
            ),
            liquidity::State::Stable(state) => (
                AmmParameters::Stable(StablePoolParameters {
                    reserves: state
                        .reserves
                        .iter()
                        .map(|reserve| (reserve.asset.token.0, reserve.asset.amount))
                        .collect(),
                    scaling_rates: state
                        .reserves
                        .iter()
                        .map(|reserve| (reserve.asset.token.0, reserve.scale.get()))
                        .collect(),
                    amplification_parameter: to_big_rational(&state.amplification_parameter),
                }),
                to_big_rational(&state.fee),
            ),
            liquidity::State::Concentrated(state) => {
                let token = |address: eth::TokenAddress| {
                    // Uniswap V3 math doesn't care about decimals, so default
                    // to 18 if it isn't available.
                    let decimals = auction
                        .tokens
                        .get(&address)
                        .and_then(|token| token.decimals)
                        .unwrap_or(18);

                    Token {
                        id: address.0,
                        decimals,
                    }
                };
                (
                    AmmParameters::Concentrated(ConcentratedPoolParameters {
                        pool: PoolInfo {
                            address: liquidity.address,
                            tokens: vec![token(state.tokens.get().0), token(state.tokens.get().1)],
                            state: PoolState {
                                sqrt_price: state.sqrt_price.0,
                                liquidity: state.liquidity.0,
                                tick: num::BigInt::from(state.tick.0),
                                liquidity_net: state
                                    .liquidity_net
                                    .iter()
                                    .map(|(tick, amount)| {
                                        (num::BigInt::from(tick.0), to_big_int(&amount.0))
                                    })
                                    .collect(),
                                fee: num::rational::Ratio::new(
                                    state.fee.numer().as_u32(),
                                    state.fee.denom().as_u32(),
                                ),
                            },
                            gas_stats: PoolStats {
                                mean_gas: liquidity.gas.0,
                            },
                        },
                    }),
                    to_big_rational(&state.fee),
                )
            }
            liquidity::State::LimitOrder(state) => {
                let index = mapping.orders.len();
                mapping.orders.push(Order::Liquidity(liquidity, state));
                model.orders.insert(
                    index,
                    OrderModel {
                        id: None,
                        sell_token: state.maker.token.0,
                        buy_token: state.taker.token.0,
                        sell_amount: state.maker.amount,
                        buy_amount: state.taker.amount,
                        allow_partial_fill: true,
                        is_sell_order: false,
                        fee: TokenAmount {
                            amount: state.fee().amount,
                            token: state.fee().token.0,
                        },
                        cost,
                        is_liquidity_order: true,
                        is_mature: true,
                        mandatory: false,
                        has_atomic_execution: true,
                        reward: 0.,
                    },
                );
                continue;
            }
        };

        mapping.amms.insert(liquidity.address, liquidity);
        model.amms.insert(
            liquidity.address,
            AmmModel {
                parameters,
                fee,
                cost,
                mandatory: false,
                address: liquidity.address,
            },
        );
    }

    (mapping, model)
}

fn to_domain_solution(
    model: &SettledBatchAuctionModel,
    mapping: Mapping,
) -> boundary::Result<solution::Solution> {
    let mut trades = Vec::new();
    let mut interactions = Vec::new();

    for jit in &model.foreign_liquidity_orders {
        trades.push(solution::Trade::Jit(solution::JitTrade {
            order: order::JitOrder {
                owner: jit.order.from,
                pre_interactions: jit
                    .order
                    .interactions
                    .pre
                    .iter()
                    .map(|i| order::Interaction {
                        target: i.target,
                        value: eth::Ether(i.value),
                        calldata: i.call_data.clone(),
                    })
                    .collect(),
                signature: jit.order.signature.clone().into(),
                sell: eth::Asset {
                    token: eth::TokenAddress(jit.order.data.sell_token),
                    amount: jit.order.data.sell_amount,
                },
                buy: eth::Asset {
                    token: eth::TokenAddress(jit.order.data.buy_token),
                    amount: jit.order.data.buy_amount,
                },
                fee: order::Fee(jit.order.data.fee_amount),
                side: match jit.order.data.kind {
                    OrderKind::Buy => order::Side::Buy,
                    OrderKind::Sell => order::Side::Sell,
                },
                class: order::Class::Liquidity,
                partially_fillable: jit.order.data.partially_fillable,
                receiver: jit.order.data.receiver.unwrap_or_default(),
                app_data: order::AppData(jit.order.data.app_data.0),
                valid_to: jit.order.data.valid_to,
            },
            executed: match jit.order.data.kind {
                model::order::OrderKind::Buy => jit.exec_buy_amount,
                model::order::OrderKind::Sell => jit.exec_sell_amount,
            },
        }));
    }

    for (id, execution) in &model.orders {
        match mapping
            .orders
            .get(*id)
            .context("solution contains order not part of auction")?
        {
            Order::Protocol(order) => trades.push(solution::Trade::Fulfillment(
                solution::Fulfillment::partial(
                    (*order).clone(),
                    match order.side {
                        order::Side::Buy => execution.exec_buy_amount,
                        order::Side::Sell => execution.exec_sell_amount,
                    },
                )
                .context("invalid trade execution")?,
            )),
            Order::Liquidity(liquidity, state) => {
                let coordinate = execution.exec_plan.as_ref().map(|e| &e.coordinates);
                let interaction =
                    solution::Interaction::Liquidity(solution::LiquidityInteraction {
                        liquidity: (*liquidity).clone(),
                        input: eth::Asset {
                            token: state.taker.token,
                            amount: execution.exec_buy_amount,
                        },
                        output: eth::Asset {
                            token: state.maker.token,
                            amount: execution.exec_sell_amount,
                        },
                        internalize: execution
                            .exec_plan
                            .as_ref()
                            .map(|e| e.internal)
                            .unwrap_or_default(),
                    });
                interactions.push((interaction, coordinate));
            }
        }
    }

    for (address, amm) in &model.amms {
        let liquidity = mapping
            .amms
            .get(address)
            .context("uses unknown liquidity")?;

        for interaction in &amm.execution {
            let coordinate = Some(&interaction.exec_plan.coordinates);
            let interaction = solution::Interaction::Liquidity(solution::LiquidityInteraction {
                liquidity: (*liquidity).clone(),
                input: eth::Asset {
                    token: eth::TokenAddress(interaction.sell_token),
                    amount: interaction.exec_sell_amount,
                },
                output: eth::Asset {
                    token: eth::TokenAddress(interaction.buy_token),
                    amount: interaction.exec_buy_amount,
                },
                internalize: interaction.exec_plan.internal,
            });
            interactions.push((interaction, coordinate));
        }
    }

    for interaction in &model.interaction_data {
        let coordinate = interaction.exec_plan.as_ref().map(|e| &e.coordinates);
        let interaction = solution::Interaction::Custom(solution::CustomInteraction {
            target: interaction.target,
            value: eth::Ether(interaction.value),
            calldata: interaction.call_data.clone(),
            inputs: interaction
                .inputs
                .iter()
                .map(|i| eth::Asset {
                    token: eth::TokenAddress(i.token),
                    amount: i.amount,
                })
                .collect(),
            outputs: interaction
                .outputs
                .iter()
                .map(|i| eth::Asset {
                    token: eth::TokenAddress(i.token),
                    amount: i.amount,
                })
                .collect(),
            internalize: interaction
                .exec_plan
                .as_ref()
                .map(|e| e.internal)
                .unwrap_or_default(),
            // allowances get added later
            allowances: Default::default(),
        });
        interactions.push((interaction, coordinate));
    }

    // sort Vec<(interaction, Option<coordinate>)> by coordinates (optionals first)
    interactions.sort_by(|(_, a), (_, b)| a.cmp(b));

    let allowances: Vec<_> = model
        .approvals
        .iter()
        .map(|approval| solution::Allowance {
            spender: approval.spender,
            asset: eth::Asset {
                token: eth::TokenAddress(approval.token),
                amount: approval.amount,
            },
        })
        .collect();

    // Add all allowances to first non-internalized interaction. This is a work
    // around because the legacy solvers didn't associate approvals with
    // specific interactions so we have to come up with some association.
    for (interaction, _) in &mut interactions {
        match interaction {
            solution::Interaction::Custom(custom) if !custom.internalize => {
                custom.allowances = allowances;
                break;
            }
            _ => (),
        };
    }

    Ok(solution::Solution {
        prices: solution::ClearingPrices(
            model
                .prices
                .iter()
                .map(|(address, price)| (eth::TokenAddress(*address), *price))
                .collect(),
        ),
        trades,
        interactions: interactions
            .into_iter()
            .map(|(interaction, _)| interaction)
            .collect(),
    })
}

fn to_big_rational(r: &eth::Rational) -> num::BigRational {
    num::BigRational::new(to_big_int(r.numer()), to_big_int(r.denom()))
}

fn to_big_int(i: &U256) -> num::BigInt {
    let mut bytes = [0; 32];
    i.to_big_endian(&mut bytes);
    num::BigInt::from_bytes_be(num::bigint::Sign::Plus, &bytes)
}

impl From<model::signature::EcdsaSignature> for order::EcdsaSignature {
    fn from(signature: model::signature::EcdsaSignature) -> Self {
        Self {
            r: signature.r,
            s: signature.s,
            v: signature.v,
        }
    }
}

impl From<model::signature::Signature> for order::Signature {
    fn from(signature: model::signature::Signature) -> Self {
        use model::signature::Signature::*;

        match signature {
            Eip712(signature) => order::Signature::Eip712(signature.into()),
            EthSign(signature) => order::Signature::EthSign(signature.into()),
            Eip1271(data) => order::Signature::Eip1271(data),
            PreSign => order::Signature::PreSign,
        }
    }
}