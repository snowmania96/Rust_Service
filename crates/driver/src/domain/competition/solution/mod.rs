use {
    crate::{
        boundary,
        domain::{
            competition::{self, order},
            eth,
        },
        infra::{
            self,
            blockchain::{self, Ethereum},
            simulator,
            solver::Solver,
            time,
            Simulator,
        },
    },
    futures::future::try_join_all,
    itertools::Itertools,
    num::ToPrimitive,
    rand::Rng,
    settlement::Settlement,
    std::collections::HashMap,
};

pub mod interaction;
pub mod settlement;
pub mod trade;

pub use {interaction::Interaction, trade::Trade};

/// A solution represents a set of orders which the solver has found an optimal
/// way to settle. A [`Solution`] is generated by a solver as a response to a
/// [`super::auction::Auction`].
#[derive(Debug)]
pub struct Solution {
    pub id: Id,
    /// Trades settled by this solution.
    pub trades: Vec<Trade>,
    /// Token prices for this solution, expressed using an arbitrary reference
    /// unit chosen by the solver. These values are only meaningful in relation
    /// to each others.
    ///
    /// The rule which relates two prices for tokens X and Y is:
    /// ```
    /// amount_x * price_x = amount_y * price_y
    /// ```
    pub prices: HashMap<eth::TokenAddress, eth::U256>,
    pub interactions: Vec<Interaction>,
    /// The solver which generated this solution.
    pub solver: Solver,
}

impl Solution {
    pub async fn approvals(
        &self,
        eth: &Ethereum,
    ) -> Result<impl Iterator<Item = eth::allowance::Approval>, blockchain::Error> {
        let settlement_contract = &eth.contracts().settlement();
        let allowances = try_join_all(self.allowances().map(|required| async move {
            eth.allowance(settlement_contract.address().into(), required.0.spender)
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

    /// Return the trades which fulfill non-liquidity auction orders. These are
    /// the orders placed by end users.
    fn user_trades(&self) -> impl Iterator<Item = &trade::Fulfillment> {
        self.trades.iter().filter_map(|trade| match trade {
            Trade::Fulfillment(fulfillment) => match fulfillment.order.kind {
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
                .entry(allowance.0.spender)
                .or_insert(eth::U256::zero());
            *amount = amount.saturating_add(allowance.0.amount);
        }
        normalized
            .into_iter()
            .map(|(spender, amount)| eth::Allowance { spender, amount }.into())
            .sorted()
    }

    /// Verify that the solution is valid and can be broadcast safely. See
    /// [`settlement::Verified`].
    pub async fn verify(
        &self,
        eth: &Ethereum,
        simulator: &Simulator,
        auction: &competition::Auction,
    ) -> Result<settlement::Verified, Error> {
        self.verify_asset_flow()?;
        self.verify_internalization(auction)?;
        self.simulate(eth, simulator, auction).await
    }

    /// Simulate settling this solution on the blockchain. This process
    /// generates the access list and estimates the gas needed to settle
    /// the solution.
    async fn simulate(
        &self,
        eth: &Ethereum,
        simulator: &Simulator,
        auction: &competition::Auction,
    ) -> Result<settlement::Verified, Error> {
        // Our settlement contract will fail if the receiver is a smart contract.
        // Because of this, if the receiver is a smart contract and we try to
        // estimate the access list, the access list estimation will also fail.
        //
        // This failure happens is because the Ethereum protocol sets a hard gas limit
        // on transferring ETH into a smart contract, which some contracts exceed unless
        // the access list is already specified.

        // The solution is to do access list estimation in two steps: first, simulate
        // moving 1 wei into every smart contract to get a partial access list.
        let partial_access_lists = try_join_all(self.user_trades().map(|trade| async {
            if !trade.order.buys_eth() || !trade.order.pays_to_contract(eth).await? {
                return Ok(Default::default());
            }
            let tx = eth::Tx {
                from: self.solver.address(),
                to: trade.order.receiver(),
                value: 1.into(),
                input: Vec::new(),
                access_list: Default::default(),
            };
            Result::<_, Error>::Ok(simulator.access_list(tx).await?)
        }))
        .await?;
        let partial_access_list = partial_access_lists
            .into_iter()
            .fold(eth::AccessList::default(), |acc, list| acc.merge(list));

        // Encode the settlement with the partial access list.
        let settlement = Settlement::encode(eth, auction, self).await?;
        let tx = settlement.clone().tx().set_access_list(partial_access_list);

        // Second, simulate the full access list, passing the partial access
        // list into the simulation. This way the settlement contract does not
        // fail, and hence the full access list estimation also does not fail.
        let access_list = simulator.access_list(tx.clone()).await?;
        let tx = tx.set_access_list(access_list.clone());

        // Finally, get the gas for the settlement using the full access list.
        let gas = simulator.gas(tx).await?;

        Ok(settlement::Verified {
            inner: settlement,
            access_list,
            gas,
        })
    }

    /// Check that the sum of tokens entering the settlement is not less than
    /// the sum of tokens exiting the settlement.
    fn verify_asset_flow(&self) -> Result<(), VerificationError> {
        Ok(())
    }

    fn verify_internalization(
        &self,
        _auction: &competition::Auction,
    ) -> Result<(), VerificationError> {
        // TODO Will be done in a follow-up PR.
        // Check that internalized interactions use trusted tokens. This requires
        // checking the internalized interactions in the solution against the
        // trusted tokens in the auction to make sure there's no foul play.
        Ok(())
    }
}

/// The time allocated for the solver to solve an auction.
#[derive(Debug, Clone, Copy)]
pub struct SolverTimeout(std::time::Duration);

impl From<std::time::Duration> for SolverTimeout {
    fn from(value: std::time::Duration) -> Self {
        Self(value)
    }
}

impl From<SolverTimeout> for std::time::Duration {
    fn from(value: SolverTimeout) -> Self {
        value.0
    }
}

impl SolverTimeout {
    /// The time limit passed to the solver for solving an auction.
    ///
    /// Solvers are given a time limit that's `buffer` less than the specified
    /// deadline. The reason for this is to allow the solver sufficient time to
    /// search for the most optimal solution, but still ensure there is time
    /// left for the driver to do some other necessary work and forward the
    /// results back to the protocol.
    pub fn new(
        deadline: chrono::DateTime<chrono::Utc>,
        buffer: chrono::Duration,
        now: time::Now,
    ) -> Option<SolverTimeout> {
        let deadline = deadline - now.now() - buffer;
        deadline.to_std().map(Self).ok()
    }

    pub fn deadline(self, now: infra::time::Now) -> chrono::DateTime<chrono::Utc> {
        now.now() + chrono::Duration::from_std(self.0).expect("reasonable solver timeout")
    }
}

/// The solution score. This is often referred to as the "objective value".
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone)]
pub struct Score(pub num::BigRational);

impl From<Score> for f64 {
    fn from(score: Score) -> Self {
        score.0.to_f64().expect("value can be represented as f64")
    }
}

impl From<num::BigRational> for Score {
    fn from(inner: num::BigRational) -> Self {
        Self(inner)
    }
}

/// A unique solution ID. This ID is encoded as part of the calldata of the
/// settlement transaction, and it's used by the protocol to match onchain
/// transactions to corresponding solutions.
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

impl Id {
    pub fn random() -> Self {
        Self(rand::thread_rng().gen())
    }

    pub fn to_be_bytes(self) -> [u8; 8] {
        self.0.to_be_bytes()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("blockchain error: {0:?}")]
    Blockchain(#[from] blockchain::Error),
    #[error("boundary error: {0:?}")]
    Boundary(#[from] boundary::Error),
    #[error("verification error: {0:?}")]
    Verification(#[from] VerificationError),
}

/// Solution verification failed.
#[derive(Debug, thiserror::Error)]
#[error("verification error")]
pub enum VerificationError {
    #[error("simulation error: {0:?}")]
    Simulation(#[from] simulator::Error),
    #[error(
        "invalid asset flow: token amounts entering the settlement do not equal token amounts \
         exiting the settlement"
    )]
    AssetFlow,
}

impl From<simulator::Error> for Error {
    fn from(value: simulator::Error) -> Self {
        VerificationError::from(value).into()
    }
}
