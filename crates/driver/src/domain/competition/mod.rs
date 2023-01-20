use {
    self::solution::settlement,
    crate::{
        boundary,
        infra::{
            blockchain::Ethereum,
            mempool,
            solver::{self, Solver},
            time,
            Mempool,
            Simulator,
        },
    },
    std::sync::Mutex,
};

pub mod auction;
pub mod order;
pub mod quote;
pub mod solution;

pub use {
    auction::Auction,
    order::Order,
    quote::Quote,
    solution::{Score, Solution, SolverTimeout},
};

/// An ongoing competition. There is one competition going on per solver at any
/// time. The competition stores settlements to solutions generated by the
/// driver, and allows them to be executed onchain when requested later. The
/// solutions expire after a certain amount of time, at which point trying to
/// use them will return an `[Error::SolutionNotFound]`.
#[derive(Debug)]
pub struct Competition {
    pub solver: Solver,
    pub eth: Ethereum,
    pub simulator: Simulator,
    pub now: time::Now,
    pub mempools: Vec<Mempool>,
    pub settlement: Mutex<Option<(solution::Id, settlement::Simulated)>>,
}

impl Competition {
    /// Solve an auction as part of this competition.
    pub async fn solve(&self, auction: &Auction) -> Result<(solution::Id, solution::Score), Error> {
        let solution = self
            .solver
            .solve(
                auction,
                SolverTimeout::for_solving(auction.deadline, self.now)?,
            )
            .await?;
        // TODO(#1009) Keep in mind that the driver needs to make sure that the solution
        // doesn't fail simulation. Currently this is the case, but this needs to stay
        // the same as this code changes.
        let settlement = solution
            .simulate(&self.eth, &self.simulator, auction)
            .await?;
        let score = settlement.score(&self.eth, auction).await?;
        let solution_id = solution::Id::random();
        *self.settlement.lock().unwrap() = Some((solution_id, settlement));
        Ok((solution_id, score))
    }

    // TODO Rename this to settle()?
    /// Execute (settle) a solution generated as part of this competition.
    pub async fn settle(&self, solution_id: solution::Id) -> Result<(), Error> {
        let settlement = match self.settlement.lock().unwrap().take() {
            Some((id, settlement)) if id == solution_id => settlement,
            Some((id, _)) => {
                tracing::warn!(?id, ?solution_id, "execute with wrong id");
                return Err(Error::SolutionNotFound);
            }
            None => {
                tracing::warn!(?solution_id, "execute without solve");
                return Err(Error::SolutionNotFound);
            }
        };
        mempool::send(&self.mempools, settlement)
            .await
            .map_err(Into::into)
    }

    fn expiration_time() -> std::time::Duration {
        std::time::Duration::from_secs(60 * 60)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no solution found for given id")]
    SolutionNotFound,
    #[error("solution error: {0:?}")]
    Solution(#[from] solution::Error),
    #[error("mempool error: {0:?}")]
    Mempool(#[from] mempool::Error),
    #[error("boundary error: {0:?}")]
    Boundary(#[from] boundary::Error),
    #[error("{0:?}")]
    DeadlineExceeded(#[from] auction::DeadlineExceeded),
    #[error("solver error: {0:?}")]
    Solver(#[from] solver::Error),
}