#![cfg(feature = "integration-test")]
use bitcoin::Amount;
use coinswap::{
    maker::{start_maker_server, MakerBehavior},
    market::directory::{start_directory_server, DirectoryServer},
    taker::{SwapParams, TakerBehavior},
};

mod test_framework;
use test_framework::*;

use log::info;
use std::{collections::BTreeSet, sync::Arc, thread, time::Duration};

/// Malice 2: Maker Broadcasts contract transactions prematurely.
///
/// The Taker and other Makers identify the situation and gets their money back via contract txs. This is
/// a potential DOS on other Makers. But the attacker Maker would loose money too in the process.
///
/// This case is hard to "blame". As the contract transactions is available to both the Makers, its not identifiable
/// which Maker is the culprit. Taker does not ban in this case.
#[tokio::test]
async fn malice2_maker_broadcast_contract_prematurely() {
    // ---- Setup ----

    let makers_config_map = [
        ((6102, 19051), MakerBehavior::Normal),
        ((16102, 19052), MakerBehavior::BroadcastContractAfterSetup),
    ];

    // Initiate test framework, Makers.
    // Taker has normal behavior.
    let (test_framework, taker, makers) =
        TestFramework::init(None, makers_config_map.into(), Some(TakerBehavior::Normal)).await;

    info!("Initiating Directory Server .....");

    let directory_server_instance = Arc::new(DirectoryServer::new(None).unwrap());
    let directory_server_instance_clone = directory_server_instance.clone();
    thread::spawn(move || {
        start_directory_server(directory_server_instance_clone);
    });

    // Fund the Taker and Makers with 3 utxos of 0.05 btc each.
    for _ in 0..3 {
        let taker_address = taker
            .write()
            .unwrap()
            .get_wallet_mut()
            .get_next_external_address()
            .unwrap();
        test_framework.send_to_address(&taker_address, Amount::from_btc(0.05).unwrap());
        makers.iter().for_each(|maker| {
            let maker_addrs = maker
                .get_wallet()
                .write()
                .unwrap()
                .get_next_external_address()
                .unwrap();
            test_framework.send_to_address(&maker_addrs, Amount::from_btc(0.05).unwrap());
        });
    }

    // Coins for fidelity creation
    makers.iter().for_each(|maker| {
        let maker_addrs = maker
            .get_wallet()
            .write()
            .unwrap()
            .get_next_external_address()
            .unwrap();
        test_framework.send_to_address(&maker_addrs, Amount::from_btc(0.05).unwrap());
    });

    // confirm balances
    test_framework.generate_blocks(1);

    let mut all_utxos = taker.read().unwrap().get_wallet().get_all_utxo().unwrap();

    let org_taker_balance_fidelity = taker
        .read()
        .unwrap()
        .get_wallet()
        .balance_fidelity_bonds(Some(&all_utxos))
        .unwrap();
    let org_taker_balance_descriptor_utxo = taker
        .read()
        .unwrap()
        .get_wallet()
        .balance_descriptor_utxo(Some(&all_utxos))
        .unwrap();
    let org_taker_balance_swap_coins = taker
        .read()
        .unwrap()
        .get_wallet()
        .balance_swap_coins(Some(&all_utxos))
        .unwrap();
    let org_taker_balance_live_contract = taker
        .read()
        .unwrap()
        .get_wallet()
        .balance_live_contract(Some(&all_utxos))
        .unwrap();

    let org_taker_balance = org_taker_balance_descriptor_utxo + org_taker_balance_swap_coins;

    // ---- Start Servers and attempt Swap ----

    // Start the Maker server threads
    let maker_threads = makers
        .iter()
        .map(|maker| {
            let maker_clone = maker.clone();
            thread::spawn(move || {
                start_maker_server(maker_clone).unwrap();
            })
        })
        .collect::<Vec<_>>();

    // Start swap

    // Makers take time to fully setup.
    makers.iter().for_each(|maker| {
        while !*maker.is_setup_complete.read().unwrap() {
            log::info!("Waiting for maker setup completion");
            // Introduce a delay of 10 seconds to prevent write lock starvation.
            thread::sleep(Duration::from_secs(10));
            continue;
        }
    });

    let swap_params = SwapParams {
        send_amount: 500000,
        maker_count: 2,
        tx_count: 3,
        required_confirms: 1,
        fee_rate: 1000,
    };

    // Calculate Original balance excluding fidelity bonds.
    // Bonds are created automatically after spawning the maker server.
    let org_maker_balances = makers
        .iter()
        .map(|maker| {
            all_utxos = maker.get_wallet().read().unwrap().get_all_utxo().unwrap();
            let maker_balance_fidelity = maker
                .get_wallet()
                .read()
                .unwrap()
                .balance_fidelity_bonds(Some(&all_utxos))
                .unwrap();
            let maker_balance_descriptor_utxo = maker
                .get_wallet()
                .read()
                .unwrap()
                .balance_descriptor_utxo(Some(&all_utxos))
                .unwrap();
            let maker_balance_swap_coins = maker
                .get_wallet()
                .read()
                .unwrap()
                .balance_swap_coins(Some(&all_utxos))
                .unwrap();
            let maker_balance_live_contract = maker
                .get_wallet()
                .read()
                .unwrap()
                .balance_live_contract(Some(&all_utxos))
                .unwrap();

            assert_eq!(maker_balance_fidelity, Amount::from_btc(0.05).unwrap());
            assert_eq!(
                maker_balance_descriptor_utxo,
                Amount::from_btc(0.14999).unwrap()
            );
            assert_eq!(maker_balance_swap_coins, Amount::from_btc(0.0).unwrap());
            assert_eq!(maker_balance_live_contract, Amount::from_btc(0.0).unwrap());
            maker_balance_descriptor_utxo + maker_balance_swap_coins
        })
        .collect::<BTreeSet<_>>();

    // Spawn a Taker coinswap thread.
    let taker_clone = taker.clone();
    let taker_thread = thread::spawn(move || {
        taker_clone
            .write()
            .unwrap()
            .do_coinswap(swap_params)
            .unwrap();
    });

    // Wait for Taker swap thread to conclude.
    taker_thread.join().unwrap();

    // Wait for Maker threads to conclude.
    makers.iter().for_each(|maker| maker.shutdown().unwrap());
    maker_threads
        .into_iter()
        .for_each(|thread| thread.join().unwrap());

    // ---- After Swap checks ----

    let _ = directory_server_instance.shutdown();

    thread::sleep(Duration::from_secs(10));

    let maker_balances = makers
        .iter()
        .map(|maker| {
            all_utxos = maker.get_wallet().read().unwrap().get_all_utxo().unwrap();
            let maker_balance_fidelity = maker
                .get_wallet()
                .read()
                .unwrap()
                .balance_fidelity_bonds(Some(&all_utxos))
                .unwrap();
            let maker_balance_descriptor_utxo = maker
                .get_wallet()
                .read()
                .unwrap()
                .balance_descriptor_utxo(Some(&all_utxos))
                .unwrap();
            let maker_balance_swap_coins = maker
                .get_wallet()
                .read()
                .unwrap()
                .balance_swap_coins(Some(&all_utxos))
                .unwrap();
            let maker_balance_live_contract = maker
                .get_wallet()
                .read()
                .unwrap()
                .balance_live_contract(Some(&all_utxos))
                .unwrap();

            assert_eq!(maker_balance_fidelity, Amount::from_btc(0.05).unwrap());
            // If the first maker misbehaves, then the 2nd maker doesn't loose anything.
            // as they haven't broadcasted their outgoing swap.
            assert!(
                maker_balance_descriptor_utxo == Amount::from_btc(0.14994773).unwrap()
                    || maker_balance_descriptor_utxo == Amount::from_btc(0.14999000).unwrap()
            );
            assert_eq!(maker_balance_swap_coins, Amount::from_btc(0.0).unwrap());
            assert_eq!(maker_balance_live_contract, Amount::from_btc(0.0).unwrap());

            maker_balance_descriptor_utxo + maker_balance_swap_coins
        })
        .collect::<BTreeSet<_>>();

    all_utxos = taker.read().unwrap().get_wallet().get_all_utxo().unwrap();

    let taker_balance_fidelity = taker
        .read()
        .unwrap()
        .get_wallet()
        .balance_fidelity_bonds(Some(&all_utxos))
        .unwrap();
    let taker_balance_descriptor_utxo = taker
        .read()
        .unwrap()
        .get_wallet()
        .balance_descriptor_utxo(Some(&all_utxos))
        .unwrap();
    let taker_balance_swap_coins = taker
        .read()
        .unwrap()
        .get_wallet()
        .balance_swap_coins(Some(&all_utxos))
        .unwrap();
    let taker_balance_live_contract = taker
        .read()
        .unwrap()
        .get_wallet()
        .balance_live_contract(Some(&all_utxos))
        .unwrap();

    let taker_balance = taker_balance_descriptor_utxo + taker_balance_swap_coins;

    assert_eq!(org_taker_balance_fidelity, Amount::from_btc(0.0).unwrap());
    assert_eq!(
        org_taker_balance_descriptor_utxo,
        Amount::from_btc(0.15).unwrap()
    );
    assert_eq!(
        org_taker_balance_live_contract,
        Amount::from_btc(0.0).unwrap()
    );
    assert_eq!(org_taker_balance_swap_coins, Amount::from_btc(0.0).unwrap());

    assert_eq!(taker_balance_fidelity, Amount::from_btc(0.0).unwrap());
    assert_eq!(
        taker_balance_descriptor_utxo,
        Amount::from_btc(0.14995773).unwrap()
    );
    assert_eq!(taker_balance_live_contract, Amount::from_btc(0.0).unwrap());
    assert_eq!(taker_balance_swap_coins, Amount::from_btc(0.0).unwrap());

    assert_eq!(*maker_balances.first().unwrap(), Amount::from_sat(14994773));

    // Everybody looses 4227 sats for contract transactions.
    assert_eq!(
        org_maker_balances
            .first()
            .unwrap()
            .checked_sub(*maker_balances.first().unwrap())
            .unwrap(),
        Amount::from_sat(4227)
    );

    assert_eq!(
        org_taker_balance.checked_sub(taker_balance).unwrap(),
        Amount::from_sat(4227)
    );

    test_framework.stop();
}
