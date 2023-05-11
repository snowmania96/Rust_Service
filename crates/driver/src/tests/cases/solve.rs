use crate::tests::{
    setup,
    setup::new::{Order, Solution},
};

/// Test that the /solve endpoint calculates the correct score.
#[tokio::test]
#[ignore]
async fn test() {
    let test = setup()
        .pool(
            "A",
            1000000000000000000000u128.into(),
            "B",
            600000000000u64.into(),
        )
        .order(Order {
            name: "example order",
            amount: 500000000000000000u64.into(),
            sell_token: "A",
            buy_token: "B",
            ..Default::default()
        })
        .solution(Solution::Valid, &["example order"])
        .done()
        .await;

    let solve = test.solve().await;

    solve.ok().score(-51517992626182.0);
}
