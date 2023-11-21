use {
    crate::{
        boundary,
        domain::{
            competition::{self, order},
            eth::{self, TokenAddress},
        },
        infra::{
            self,
            blockchain::{self, Ethereum},
            simulator,
            solver::Solver,
            Simulator,
        },
    },
    futures::future::try_join_all,
    itertools::Itertools,
    std::collections::{BTreeSet, HashMap},
    thiserror::Error,
};

pub mod interaction;
pub mod settlement;
pub mod trade;

pub use {interaction::Interaction, settlement::Settlement, trade::Trade};

// TODO Add a constructor and ensure that the clearing prices are included for
// each trade
/// A solution represents a set of orders which the solver has found an optimal
/// way to settle. A [`Solution`] is generated by a solver as a response to a
/// [`competition::Auction`]. See also [`settlement::Settlement`].
#[derive(Clone)]
pub struct Solution {
    id: Id,
    trades: Vec<Trade>,
    prices: HashMap<eth::TokenAddress, eth::U256>,
    interactions: Vec<Interaction>,
    solver: Solver,
    score: SolverScore,
    weth: eth::WethAddress,
}

impl Solution {
    pub fn new(
        id: Id,
        trades: Vec<Trade>,
        prices: HashMap<eth::TokenAddress, eth::U256>,
        interactions: Vec<Interaction>,
        solver: Solver,
        score: SolverScore,
        weth: eth::WethAddress,
    ) -> Result<Self, InvalidClearingPrices> {
        let solution = Self {
            id,
            trades,
            prices,
            interactions,
            solver,
            score,
            weth,
        };

        // Check that the solution includes clearing prices for all user trades.
        if solution.user_trades().all(|trade| {
            solution.clearing_price(trade.order().sell.token).is_some()
                && solution.clearing_price(trade.order().buy.token).is_some()
        }) {
            Ok(solution)
        } else {
            Err(InvalidClearingPrices)
        }
    }

    /// The ID of this solution.
    pub fn id(&self) -> Id {
        self.id
    }

    /// Trades settled by this solution.
    pub fn trades(&self) -> &[Trade] {
        &self.trades
    }

    /// Interactions executed by this solution.
    pub fn interactions(&self) -> &[Interaction] {
        &self.interactions
    }

    /// The solver which generated this solution.
    pub fn solver(&self) -> &Solver {
        &self.solver
    }

    pub fn score(&self) -> &SolverScore {
        &self.score
    }

    /// Approval interactions necessary for encoding the settlement.
    pub async fn approvals(
        &self,
        eth: &Ethereum,
    ) -> Result<impl Iterator<Item = eth::allowance::Approval>, Error> {
        let settlement_contract = &eth.contracts().settlement();
        let allowances = try_join_all(self.allowances().map(|required| async move {
            eth.erc20(required.0.token)
                .allowance(settlement_contract.address().into(), required.0.spender)
                .await
                .map(|existing| (required, existing))
        }))
        .await?;
        let approvals = allowances.into_iter().filter_map(|(required, existing)| {
            required
                .approval(&existing)
                // As a gas optimization, we always approve the max amount possible. This minimizes
                // the number of approvals necessary, and therefore minimizes the approval fees over time. This is a
                // potential security issue, but its effects are minimized and only exploitable if
                // solvers use insecure contracts.
                .map(eth::allowance::Approval::max)
        });
        Ok(approvals)
    }

    /// An empty solution has no user trades and a score of 0.
    pub fn is_empty(&self) -> bool {
        self.user_trades().next().is_none()
    }

    /// Return the trades which fulfill non-liquidity auction orders. These are
    /// the orders placed by end users.
    fn user_trades(&self) -> impl Iterator<Item = &trade::Fulfillment> {
        self.trades.iter().filter_map(|trade| match trade {
            Trade::Fulfillment(fulfillment) => match fulfillment.order().kind {
                order::Kind::Market | order::Kind::Limit { .. } => Some(fulfillment),
                order::Kind::Liquidity => None,
            },
            Trade::Jit(_) => None,
        })
    }

    /// Return the allowances in a normalized form, where there is only one
    /// allowance per [`eth::allowance::Spender`], and they're ordered
    /// deterministically.
    fn allowances(&self) -> impl Iterator<Item = eth::allowance::Required> {
        let mut normalized = HashMap::new();
        // TODO: we need to carry the "internalize" flag with the allowances,
        // since we don't want to include approvals for interactions that are
        // meant to be internalized anyway.
        let allowances = self.interactions.iter().flat_map(Interaction::allowances);
        for allowance in allowances {
            let amount = normalized
                .entry((allowance.0.token, allowance.0.spender))
                .or_insert(eth::U256::zero());
            *amount = amount.saturating_add(allowance.0.amount);
        }
        normalized
            .into_iter()
            .map(|((token, spender), amount)| {
                eth::Allowance {
                    token,
                    spender,
                    amount,
                }
                .into()
            })
            .sorted()
    }

    /// Encode the solution into a [`Settlement`], which can be used to execute
    /// the solution onchain.
    pub async fn encode(
        self,
        auction: &competition::Auction,
        eth: &Ethereum,
        simulator: &Simulator,
    ) -> Result<Settlement, Error> {
        Settlement::encode(self, auction, eth, simulator).await
    }

    /// Token prices settled by this solution, expressed using an arbitrary
    /// reference unit chosen by the solver. These values are only
    /// meaningful in relation to each others.
    ///
    /// The rule which relates two prices for tokens X and Y is:
    /// ```
    /// amount_x * price_x = amount_y * price_y
    /// ```
    pub fn clearing_prices(&self) -> Result<Vec<eth::Asset>, Error> {
        let prices = self.prices.iter().map(|(&token, &amount)| eth::Asset {
            token,
            amount: amount.into(),
        });

        if self.user_trades().any(|trade| trade.order().buys_eth()) {
            // The solution contains an order which buys ETH. Solvers only produce solutions
            // for ERC20 tokens, while the driver adds special [`Interaction`]s to
            // wrap/unwrap the ETH tokens into WETH, and sends orders to the solver with
            // WETH instead of ETH. Once the driver receives the solution which fulfills an
            // ETH order, a clearing price for ETH needs to be added, equal to the
            // WETH clearing price.

            // If no order trades WETH, the WETH price is not necessary, only the ETH
            // price is needed. Remove the unneeded WETH price, which slightly reduces
            // gas used by the settlement.
            let mut prices = if self.user_trades().all(|trade| {
                trade.order().sell.token != self.weth.0 && trade.order().buy.token != self.weth.0
            }) {
                prices
                    .filter(|price| price.token != self.weth.0)
                    .collect_vec()
            } else {
                prices.collect_vec()
            };

            // Add a clearing price for ETH equal to WETH.
            prices.push(eth::Asset {
                token: eth::ETH_TOKEN,
                amount: self.prices[&self.weth.into()].to_owned().into(),
            });

            return Ok(prices);
        }

        // TODO: We should probably filter out all unused prices to save gas.

        Ok(prices.collect_vec())
    }

    /// Clearing price for the given token.
    pub fn clearing_price(&self, token: eth::TokenAddress) -> Option<eth::U256> {
        // The clearing price of ETH is equal to WETH.
        let token = token.wrap(self.weth);
        self.prices.get(&token).map(ToOwned::to_owned)
    }
}

impl std::fmt::Debug for Solution {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("Solution")
            .field("id", &self.id)
            .field("trades", &self.trades)
            .field("prices", &self.prices)
            .field("interactions", &self.interactions)
            .field("solver", &self.solver.name())
            .field("score", &self.score)
            .finish()
    }
}

/// The time limit passed to the solver for solving an auction.
#[derive(Debug, Clone, Copy)]
pub struct SolverTimeout(chrono::Duration);

impl SolverTimeout {
    pub fn deadline(self) -> chrono::DateTime<chrono::Utc> {
        infra::time::now() + self.0
    }

    pub fn duration(self) -> chrono::Duration {
        self.0
    }

    #[must_use]
    pub fn reduce(self, duration: chrono::Duration) -> Self {
        Self(self.0 - duration)
    }
}

impl From<chrono::Duration> for SolverTimeout {
    fn from(duration: chrono::Duration) -> Self {
        Self(duration)
    }
}

/// Carries information how the score should be calculated.
#[derive(Debug, Clone)]
pub enum SolverScore {
    Solver(eth::U256),
    RiskAdjusted(f64),
}
/// A unique solution ID. This ID is generated by the solver and only needs to
/// be unique within a single round of competition. This ID is only important in
/// the communication between the driver and the solver, and it is not used by
/// the protocol.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub struct Id(pub u64);

impl From<u64> for Id {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl From<Id> for u64 {
    fn from(value: Id) -> Self {
        value.0
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("blockchain error: {0:?}")]
    Blockchain(#[from] blockchain::Error),
    #[error("boundary error: {0:?}")]
    Boundary(#[from] boundary::Error),
    #[error("simulation error: {0:?}")]
    Simulation(#[from] simulator::Error),
    #[error(
        "invalid asset flow: token amounts entering the settlement do not equal token amounts \
         exiting the settlement"
    )]
    AssetFlow(HashMap<eth::TokenAddress, num::BigInt>),
    #[error(transparent)]
    Execution(#[from] trade::ExecutionError),
    #[error(
        "non bufferable tokens used: solution attempts to internalize tokens which are not trusted"
    )]
    NonBufferableTokensUsed(BTreeSet<TokenAddress>),
    #[error("invalid internalization: uninternalized solution fails to simulate")]
    FailingInternalization,
    #[error("insufficient solver account Ether balance, required {0:?}")]
    SolverAccountInsufficientBalance(eth::Ether),
    #[error("attempted to merge settlements generated by different solvers")]
    DifferentSolvers,
}

#[derive(Debug, Error)]
#[error("invalid clearing prices")]
pub struct InvalidClearingPrices;
