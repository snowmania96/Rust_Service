use {
    self::solution::settlement,
    super::Mempools,
    crate::{
        domain::{competition::solution::Settlement, liquidity},
        infra::{
            self,
            blockchain::Ethereum,
            observe,
            solver::{self, Solver},
            Simulator,
        },
        util::Bytes,
    },
    futures::future::join_all,
    itertools::Itertools,
    rand::seq::SliceRandom,
    std::{collections::HashSet, sync::Mutex},
    tap::TapFallible,
};

pub mod auction;
pub mod order;
pub mod solution;

pub use {
    auction::Auction,
    order::Order,
    solution::{Score, Solution, SolverTimeout},
};

/// An ongoing competition. There is one competition going on per solver at any
/// time. The competition stores settlements to solutions generated by the
/// driver, and allows them to be executed onchain when requested later. The
/// solutions expire after a certain amount of time, at which point trying to
/// use them will return an `[Error::InvalidSolutionId]`.
#[derive(Debug)]
pub struct Competition {
    pub solver: Solver,
    pub eth: Ethereum,
    pub liquidity: infra::liquidity::Fetcher,
    pub simulator: Simulator,
    pub mempools: Mempools,
    pub settlement: Mutex<Option<Settlement>>,
}

impl Competition {
    /// Solve an auction as part of this competition.
    pub async fn solve(&self, auction: &Auction) -> Result<Reveal, Error> {
        let liquidity = self
            .liquidity
            .fetch(
                &auction
                    .orders
                    .iter()
                    .filter_map(|order| match order.kind {
                        order::Kind::Market | order::Kind::Limit { .. } => {
                            liquidity::TokenPair::new(order.sell.token, order.buy.token)
                        }
                        order::Kind::Liquidity => None,
                    })
                    .collect(),
            )
            .await;

        // Fetch the solutions from the solver.
        let solutions = self
            .solver
            .solve(auction, &liquidity, auction.deadline.timeout()?)
            .await?;

        // Empty solutions aren't useful, so discard them.
        let solutions = solutions.into_iter().filter(|solution| {
            if solution.is_empty() {
                observe::empty_solution(self.solver.name(), solution.id);
                false
            } else {
                true
            }
        });

        // Encode the solutions into settlements.
        let settlements = join_all(solutions.map(|solution| async move {
            observe::encoding(self.solver.name(), solution.id);
            (
                solution.id,
                solution.encode(auction, &self.eth, &self.simulator).await,
            )
        }))
        .await;

        // Filter out solutions that failed to encode.
        let mut settlements = settlements
            .into_iter()
            .filter_map(|(id, result)| {
                result
                    .tap_err(|err| observe::encoding_failed(self.solver.name(), id, err))
                    .ok()
            })
            .collect_vec();

        // TODO(#1483): parallelize this
        // TODO(#1480): more optimal approach for settlement merging

        // Merge the settlements in random order.
        settlements.shuffle(&mut rand::thread_rng());

        // The merging algorithm works as follows: the [`settlements`] vector keeps the
        // "most merged" settlements until they can't be merged anymore, at
        // which point they are moved into the [`results`] vector.

        // The merged settlements in their final form.
        let mut results = Vec::new();
        while let Some(settlement) = settlements.pop() {
            // Has [`settlement`] been merged into another settlement?
            let mut merged = false;
            // Try to merge [`settlement`] into some other settlement.
            for other in settlements.iter_mut() {
                match other.merge(&settlement, &self.eth, &self.simulator).await {
                    Ok(m) => {
                        *other = m;
                        merged = true;
                        observe::merged(self.solver.name(), &settlement, other);
                        break;
                    }
                    Err(err) => {
                        observe::not_merged(self.solver.name(), &settlement, other, err);
                    }
                }
            }
            // If [`settlement`] can't be merged into any other settlement, this is its
            // final, most optimal form. Push it to the results.
            if !merged {
                results.push(settlement);
            }
        }

        let settlements = results;

        // Score the settlements.
        let scores = settlements
            .into_iter()
            .map(|settlement| {
                observe::scoring(self.solver.name(), &settlement);
                (settlement.score(&self.eth, auction), settlement)
            })
            .collect_vec();

        // Filter out settlements which failed scoring.
        let scores = scores
            .into_iter()
            .filter_map(|(result, settlement)| {
                result
                    .tap_err(|err| {
                        observe::scoring_failed(self.solver.name(), settlement.auction_id, err)
                    })
                    .ok()
                    .map(|score| (score, settlement))
            })
            .collect_vec();

        // Observe the scores.
        for (score, settlement) in scores.iter() {
            observe::score(self.solver.name(), settlement, score);
        }

        // Pick the best-scoring settlement.
        let (score, settlement) = scores
            .into_iter()
            .max_by_key(|(score, _)| score.to_owned())
            .ok_or(Error::SolutionNotFound)?;

        let orders = settlement.orders();
        *self.settlement.lock().unwrap() = Some(settlement);

        Ok(Reveal { score, orders })
    }

    /// Execute the solution generated as part of this competition. Use
    /// [`Competition::solve`] to generate the solution.
    pub async fn settle(&self) -> Result<Calldata, Error> {
        let settlement = self
            .settlement
            .lock()
            .unwrap()
            .take()
            .ok_or(Error::SolutionNotAvailable)?;
        self.mempools.execute(&self.solver, &settlement);
        Ok(Calldata {
            internalized: settlement
                .calldata(
                    self.eth.contracts().settlement(),
                    settlement::Internalization::Enable,
                )
                .into(),
            uninternalized: settlement
                .calldata(
                    self.eth.contracts().settlement(),
                    settlement::Internalization::Disable,
                )
                .into(),
        })
    }

    /// The ID of the auction being competed on.
    pub fn auction_id(&self) -> Option<auction::Id> {
        self.settlement
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| s.auction_id)
    }
}

/// Solution information revealed to the protocol by the driver before the
/// settlement happens. Note that the calldata is only revealed once the
/// protocol instructs the driver to settle, and not before.
#[derive(Debug)]
pub struct Reveal {
    pub score: Score,
    /// The orders solved by this solution.
    pub orders: HashSet<order::Uid>,
}

#[derive(Debug)]
pub struct Calldata {
    pub internalized: Bytes<Vec<u8>>,
    /// The uninternalized calldata must be known so that the CoW solver team
    /// can manually enforce certain rules which can not be enforced
    /// automatically.
    pub uninternalized: Bytes<Vec<u8>>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(
        "no solution is available yet, this might mean that /settle was called before /solve \
         returned"
    )]
    SolutionNotAvailable,
    #[error("no solution found for the auction")]
    SolutionNotFound,
    #[error("{0:?}")]
    DeadlineExceeded(#[from] solution::DeadlineExceeded),
    #[error("solver error: {0:?}")]
    Solver(#[from] solver::Error),
}
