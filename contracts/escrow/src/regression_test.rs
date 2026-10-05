use super::*;

#[test]
fn test_regression_event_topic_consistency() {
    let env = Env::default();
    let (client, _admin) = setup_initialized(&env);
    let agent = Address::generate(&env);
    let svc = Symbol::new(&env, "infer");
    client.set_service_price(&svc, &10i128);
    client.record_usage(&agent, &svc, &42u32);

    // The topic for 'usage' was previously symbol_short!("usage")
    // Now it should be events::TOPIC_USAGE which is also symbol_short!("usage")
    let events = env.events().all();
    let (_, topics, _) = events.last().unwrap();
    let expected_topics: soroban_sdk::Vec<soroban_sdk::Val> =
        (symbol_short!("usage"),).into_val(&env);
    assert_eq!(
        topics, expected_topics,
        "Topic must be byte-for-byte identical"
    );
}
