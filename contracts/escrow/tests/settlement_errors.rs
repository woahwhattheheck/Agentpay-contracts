use escrow::{DataKey, Escrow, EscrowClient, EscrowError, MAX_SETTLE_ALL};
use soroban_sdk::{testutils::Address as _, Address, Env, String, Symbol, Vec};

fn initialized(env: &Env) -> (EscrowClient<'_>, Address) {
    env.mock_all_auths();
    let contract = env.register(Escrow, ());
    let client = EscrowClient::new(env, &contract);
    let admin = Address::generate(env);
    client.init(&admin);
    (client, admin)
}

#[test]
fn settlement_returns_typed_initialization_and_pause_errors() {
    let env = Env::default();
    env.mock_all_auths();
    let contract = env.register(Escrow, ());
    let client = EscrowClient::new(&env, &contract);
    let admin = Address::generate(&env);
    let agent = Address::generate(&env);
    let service = Symbol::new(&env, "infer");

    assert_eq!(
        client.try_settle(&admin, &agent, &service),
        Err(Ok(EscrowError::NotInitialized.into()))
    );
    assert_eq!(
        client.try_settle_all(&admin, &agent),
        Err(Ok(EscrowError::NotInitialized.into()))
    );

    client.init(&admin);
    client.pause();
    assert_eq!(
        client.try_settle(&admin, &agent, &service),
        Err(Ok(EscrowError::ContractPaused.into()))
    );
    assert_eq!(
        client.try_settle_all(&admin, &agent),
        Err(Ok(EscrowError::ContractPaused.into()))
    );
}

#[test]
fn settlement_returns_typed_metadata_and_authority_errors_without_draining_usage() {
    let env = Env::default();
    let (client, admin) = initialized(&env);
    let owner = Address::generate(&env);
    let intruder = Address::generate(&env);
    let agent = Address::generate(&env);
    let service = Symbol::new(&env, "infer");
    client.set_service_price(&service, &10);
    client.record_usage(&agent, &service, &5);

    assert_eq!(
        client.try_settle(&intruder, &agent, &service),
        Err(Ok(EscrowError::ServiceMetadataNotFound.into()))
    );
    assert_eq!(
        client.try_settle_all(&intruder, &agent),
        Err(Ok(EscrowError::ServiceMetadataNotFound.into()))
    );

    client.set_service_metadata(&service, &String::from_str(&env, "inference"), &owner);
    assert_eq!(
        client.try_settle(&intruder, &agent, &service),
        Err(Ok(EscrowError::Unauthorized.into()))
    );
    assert_eq!(
        client.try_settle_all(&intruder, &agent),
        Err(Ok(EscrowError::Unauthorized.into()))
    );
    assert_eq!(client.get_usage(&agent, &service), 5);
    assert_eq!(client.get_last_settlement(&agent, &service), None);
    assert_eq!(client.get_total_settled_by_agent(&agent), 0);

    // Both legitimate roles retain their existing settlement behavior.
    assert_eq!(client.settle(&owner, &agent, &service), 50);
    client.record_usage(&agent, &service, &2);
    assert_eq!(client.settle(&admin, &agent, &service), 20);
}

#[test]
fn settle_all_rolls_back_an_earlier_service_when_later_authority_is_rejected() {
    let env = Env::default();
    let (client, _admin) = initialized(&env);
    let caller = Address::generate(&env);
    let other_owner = Address::generate(&env);
    let agent = Address::generate(&env);
    let owned = Symbol::new(&env, "owned");
    let other = Symbol::new(&env, "other");
    for (service, owner) in [(&owned, &caller), (&other, &other_owner)] {
        client.set_service_metadata(service, &String::from_str(&env, "service"), owner);
        client.set_service_price(service, &10);
        client.record_usage(&agent, service, &5);
    }

    assert_eq!(
        client.try_settle_all(&caller, &agent),
        Err(Ok(EscrowError::Unauthorized.into()))
    );
    for service in [&owned, &other] {
        assert_eq!(client.get_usage(&agent, service), 5);
        assert_eq!(client.get_last_settlement(&agent, service), None);
    }
    assert_eq!(client.get_total_settled_by_agent(&agent), 0);
    assert_eq!(client.get_total_settled_all_time(), 0);
}

#[test]
fn settle_all_returns_typed_error_for_an_oversized_service_index() {
    let env = Env::default();
    let (client, admin) = initialized(&env);
    let agent = Address::generate(&env);
    env.cost_estimate().budget().reset_unlimited();

    // Only a migration or corrupt storage can bypass the public index cap.
    env.as_contract(&client.address, || {
        let mut index = Vec::new(&env);
        for i in 0..=MAX_SETTLE_ALL {
            index.push_back(Symbol::new(&env, &format!("svc_{i}")));
        }
        env.storage()
            .persistent()
            .set(&DataKey::AgentServiceIndex(agent.clone()), &index);
    });

    assert_eq!(
        client.try_settle_all(&admin, &agent),
        Err(Ok(EscrowError::SettleAllTooLarge.into()))
    );
    assert_eq!(client.get_total_settled_by_agent(&agent), 0);
}
