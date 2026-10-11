use super::*;
#[test]
fn reward_parsing_rejects_spending_and_accepts_grouped_totals() {
    assert_eq!(parse_rewards("0|y|URI|25|2500"), [0.0, 25.0, 0.0, 0.0]);
    assert_eq!(parse_rewards("0|LM|STD|HON|10|500"), [0.0, 0.0, 10.0, 0.0]);
    assert_eq!(parse_rewards("0|y|URI|-100|2400"), [0.0; 4]);
    assert_eq!(
        parse_rewards("You received 12,345 Credits and 25 Uridium."),
        [12345.0, 25.0, 0.0, 0.0]
    );
    assert_eq!(
        parse_rewards("You received 1.234 honor points and 5 000 experience points."),
        [0.0, 0.0, 1234.0, 5000.0]
    );
    assert_eq!(parse_rewards("You spent 100 credits."), [0.0; 4]);
    assert_eq!(parse_rewards("Credits: 100"), [0.0; 4]);
    assert_eq!(parse_rewards("You received -100 credits."), [0.0; 4]);
}
#[test]
fn reconciliation_never_counts_as_rewards_and_accounts_are_isolated() {
    let directory =
        std::env::temp_dir().join(format!("xenomit-stats-{}-{}", std::process::id(), now_ms()));
    let mut session = Session::new("unused", "test");
    session.user_id = 1;
    let shared = Statistics::load(&session, directory.clone());
    let started = shared.lock().unwrap().started;
    {
        let mut stats = shared.lock().unwrap();
        stats.reconcile([Some(100.0), None, None, None], "game", started);
        stats.log("You received 50 credits.", "game", started + 1000);
        stats.log("You received 50 credits.", "game", started + 2000);
        stats.reconcile([Some(120.0), None, None, None], "backpage", started + 3000);
        assert_eq!(stats.balances[0], Some(120.0));
        assert_eq!(stats.rates(started + 3_600_000)[0], 100.0);
        stats.ended = Some(started + 3_600_000);
        assert_eq!(stats.rates(started + 7_200_000)[0], 100.0);
        stats.client_line(r#"[game-assets] {"user_id":2,"assets":{"credits":999}}"#);
        assert_eq!(stats.balances[0], Some(120.0));
        stats.client_line(
            r#"[game-log] {"user_id":1,"message":"Kazandınız: 25 kredi","rewards":[25,0,0,0]}"#,
        );
        assert_eq!(stats.balances[0], Some(145.0));
        stats.client_line("[game-assets] invalid JSON");
        assert_eq!(stats.balances[0], Some(145.0));
        stats.log("0|y|URI|-100|2400", "packet", started + 4000);
        assert_eq!(stats.balances[1], Some(2400.0));
        assert_eq!(stats.events.back().unwrap().rewards, [0.0; 4]);
    }
    assert_eq!(
        Statistics::load(&session, directory.clone())
            .lock()
            .unwrap()
            .balances[0],
        Some(145.0)
    );
    session.user_id = 2;
    assert_eq!(
        Statistics::load(&session, directory.clone())
            .lock()
            .unwrap()
            .balances[0],
        None
    );
    std::fs::remove_file(directory.join("test.darkorbit.com-1.jsonl")).unwrap();
    std::fs::remove_dir(directory).unwrap();
}
