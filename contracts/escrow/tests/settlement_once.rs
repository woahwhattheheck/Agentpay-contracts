#![allow(deprecated)]

use escrow::{Escrow, EscrowClient};
use soroban_sdk::{testutils::Address as _, Address, Env, Symbol};

fn setup(env: &Env) -> (EscrowClient<'_>, Address) {
    env.mock_all_auths();
    let contract_id = env.register_contract(None, Escrow);
    let client = EscrowClient::new(env, &contract_id);
    let admin = Address::generate(env);
    client.init(&admin);
    (client, admin)
}

#[test]
fn duplicate_single_settlement_cannot_repeat_value_effects() {
    let env = Env::default();
    let (client, admin) = setup(&env);
    let agent = Address::generate(&env);
    let service = Symbol::new(&env, "single");

    client.set_service_price(&service, &10i128);
    client.credit_agent(&agent, &100i128);
    client.record_usage(&agent, &service, &4u32);

    assert_eq!(client.settle(&admin, &agent, &service), 40i128);
    let credit_after = client.get_agent_credit(&agent);
    let settled_after = client.get_total_settled_by_agent(&agent);
    let stamp_after = client.get_last_settlement(&agent, &service);

    assert!(client.try_settle(&admin, &agent, &service).is_err());
    assert_eq!(client.get_agent_credit(&agent), credit_after);
    assert_eq!(client.get_total_settled_by_agent(&agent), settled_after);
    assert_eq!(client.get_last_settlement(&agent, &service), stamp_after);
}

#[test]
fn batch_and_single_entrypoints_share_one_claim() {
    let env = Env::default();
    let (client, admin) = setup(&env);
    let agent = Address::generate(&env);
    let service = Symbol::new(&env, "mixed");

    client.set_service_price(&service, &6i128);
    client.record_usage(&agent, &service, &5u32);
    let batch = client.settle_all(&admin, &agent);
    assert_eq!(batch.get(0), Some((service.clone(), 30i128)));

    assert!(client.try_settle(&admin, &agent, &service).is_err());
}

#[test]
fn positive_usage_creates_a_new_settlement_cycle() {
    let env = Env::default();
    let (client, admin) = setup(&env);
    let agent = Address::generate(&env);
    let service = Symbol::new(&env, "renew");

    client.set_service_price(&service, &4i128);
    client.record_usage(&agent, &service, &2u32);
    assert_eq!(client.settle(&admin, &agent, &service), 8i128);

    client.record_usage(&agent, &service, &3u32);
    assert_eq!(client.settle(&admin, &agent, &service), 12i128);
}
