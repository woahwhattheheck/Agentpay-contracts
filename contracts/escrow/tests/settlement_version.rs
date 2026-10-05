#![allow(deprecated)]

use escrow::{Escrow, EscrowClient};
use soroban_sdk::{testutils::Address as _, Address, Env, Symbol};

#[test]
fn versioned_settlement_round_trip() {
    let env = Env::default();
    env.mock_all_auths();

    let contract_id = env.register_contract(None, Escrow);
    let client = EscrowClient::new(&env, &contract_id);
    let admin = Address::generate(&env);
    let agent = Address::generate(&env);
    let service_id = Symbol::new(&env, "infer");

    client.init(&admin);
    client.set_service_price(&service_id, &10i128);
    client.record_usage(&agent, &service_id, &2u32);

    let version = client.get_settlement_version(&agent, &service_id);
    assert_eq!(version, 0u64);
    assert_eq!(client.settle(&admin, &agent, &service_id, &version), 20i128);
    assert_eq!(client.get_settlement_version(&agent, &service_id), 1u64);
    assert_eq!(client.get_usage(&agent, &service_id), 0u32);
}
