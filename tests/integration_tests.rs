use rs_ib_api::ib_client::IBClient;
use rs_ib_api::ib_contract::Contract;
use rs_ib_api::order::Order;
use tokio::time;
use chrono::Duration;
use chrono::{TimeZone, Utc, DateTime};
use rs_ib_api::ib_enums::*;
use rust_decimal::prelude::*;



#[tokio::test]
async fn contract_details() {
    let mut client = match IBClient::connect(4002, 1, "").await {
        Ok(client) => client,
        Err(_error) => panic!("Connection not successful!")
    };
    let contract = Contract {
        symbol: Some("AAPL".to_string()),
        exchange: Some("SMART".to_string()),
        sec_type: Some(SecType::Stock),
        currency: Some("USD".to_string()),
        ..Default::default()
    }; 
    match client.req_contract_details(&contract).await {
        Ok(details) => for detail in &details {
            match &detail.contract {
                Some(contract) => assert_eq!(contract.symbol, Some("AAPL".to_string())),
                None => panic!("No valid contract details returned for AAPL")
            }
        }
        Err(_) => panic!("Error requesting contract details")
    };
}

#[tokio::test]
async fn place_market_order() {
    let mut client = match IBClient::connect(4002, 2, "").await {
        Ok(client) => client,
        Err(_error) => panic!("Connection not successful!")
    };
    let contract = Contract {
        symbol: Some("AAPL".to_string()),
        exchange: Some("SMART".to_string()),
        sec_type: Some(SecType::Stock),
        currency: Some("USD".to_string()),
        ..Default::default()
    };
    let order = Order::market(contract, Action::Buy, Decimal::new(10,0));
    match &mut client.place_order(&order).await {
        Ok(tracker) => {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            assert_eq!(tracker.status(), Some("Filled".to_string()));
        }
        Err(err)    => panic!("Error during order submission.")
    }
}

#[tokio::test]
async fn market_data() {
    let mut client = match IBClient::connect(4002, 3, "").await {
        Ok(client) => client,
        Err(_error) => panic!("Connection not successful!")
    };
    let contract = Contract {
        symbol: Some("AAPL".to_string()),
        exchange: Some("SMART".to_string()),
        sec_type: Some(SecType::Stock),
        currency: Some("USD".to_string()),
        ..Default::default()
    };
    match &client.req_market_data(&contract, false, false,
         vec![GenericTickType::ShortableData]).await {
        Ok(ticker) => {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            assert!(ticker.midpoint().is_some());
        }
        Err(_error) => panic!("Market data request not successful")
    }
}

#[tokio::test]
async fn historical_data() {
    let mut client = match IBClient::connect(4002, 4, "").await {
        Ok(client) => client,
        Err(_error) => panic!("Connection not successful!")
    };
    let contract = Contract {
        symbol: Some("AAPL".to_string()),
        exchange: Some("SMART".to_string()),
        sec_type: Some(SecType::Stock),
        currency: Some("USD".to_string()),
        ..Default::default()
    };
    let end_dt = Utc.datetime_from_str("2020-03-01 00:00:00", "%Y-%m-%d %H:%M:%S");

    match &client.req_historical_data(&contract, &end_dt.unwrap(), "1 M", "1 day",
    "MIDPOINT", true).await {
        Ok(bars) => {
            assert!(bars.n_bars > 0);
        },
        Err(_error) => panic!("Bar series loading not successful!")
    }
}