
use crate::ib_enums::*;
use crate::ib_contract;
use crate::utils::ib_message;
use crate::utils::ib_stream;
use crate::utils::ib_stream::AsyncResult;
use crate::utils::ib_message::Encodable;
use crate::account;
use crate::order;
use crate::ticker;
use crate::bars;
use crate::frame::IBFrame;

use std::collections::HashMap;
use std::collections::VecDeque;
use std::{error::Error, fmt};

use rust_decimal::prelude::*;

use std::str;
use chrono::{TimeZone, DateTime};
//use chrono::format::ParseError;
use tokio::task;
use tokio::time;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use crossbeam::channel::{self, RecvError};
use std::sync::atomic::{AtomicUsize,AtomicI32};
use futures::future::{Abortable, AbortHandle, Aborted};

enum Request {
    OrderID(oneshot::Sender<i32>),
    ReqWithID{id: i32, sender: oneshot::Sender<Response>},
}
enum Response {
    ContractDetails(Vec<ib_contract::ContractDetails>),
    Order(order::OrderTracker),
    Ticker(ticker::Ticker),
    Bars(bars::BarSeries),
    Empty
}

#[derive(Debug)]
struct ResponseError;

impl Error for ResponseError {}

impl fmt::Display for ResponseError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Invalid response type!") // user-facing output
    }
}

pub struct IBClient
{
    client_id: i32,
    writer_abort_handle: AbortHandle,
    reader_abort_handle: AbortHandle,
    keep_alive_abort_handle: AbortHandle,
    write_tx: mpsc::Sender<String>,
    req_tx: crossbeam::channel::Sender<Request>,
    server_version: i32,
    account: account::AccountReceiver,
    next_req_id: i32,
    next_order_id: i32,
    mkt_data_setting: MarketDataType
}

impl IBClient
{

    pub async fn connect(port: i32, client_id: i32, optional_capabilities: &str) -> AsyncResult<Self> {

        let mut addr = "127.0.0.1:".to_string();
        addr.push_str(&port.to_string());
        let stream = TcpStream::connect(addr).await?;
        let ( recv, trans) = stream.into_split();
        let mut writer = ib_stream::IBWriter::new(trans);
        let mut reader = ib_stream::IBReader::new(recv);
        //initiate handshake
        writer.write_raw(b"API\0").await?;
        let mut valid_versions = constants::MIN_CLIENT_VER.to_string();
        valid_versions.push_str("..");
        valid_versions.push_str(&constants::MAX_CLIENT_VER.to_string());
        writer.write(&valid_versions).await?;
        let msg = reader.read().await?;
        let msg = String::from_utf8_lossy(&msg);
        let mut it = msg.split("\0");
        let server_version = it.next().unwrap().parse().unwrap();

        //println!("{:?}", server_version);
        let mut msg = Outgoing::StartApi.encode();
        let version : i32 = 2;
        //start API
        msg.push_str(&version.encode());
        msg.push_str(&client_id.encode());
        msg.push_str(&optional_capabilities.to_string().encode());
        writer.write(&msg).await?;
        let client_id = client_id;
        let (tx, mut rx) = mpsc::channel(64);
        let write_tx: mpsc::Sender<String> = tx.clone();
        let (req_tx, req_rx) = channel::bounded(100);


        //start the writer task managing the write half of the socket
        let (writer_abort_handle, writer_abort_registration) = AbortHandle::new_pair();
        let writer_fut = Abortable::new(async move {
            loop {
                let msg = rx.recv().await.unwrap();
                writer.write(&msg).await.expect("Could not write to socket.");
            }
        }, writer_abort_registration);
        let _writer_task = tokio::spawn(writer_fut);

        //start the keep alive task to send a message across the socket every minute
        let (keep_alive_abort_handle, keep_alive_abort_registration) = AbortHandle::new_pair();
        let keep_alive_fut = Abortable::new(async move{
            let mut msg = Outgoing::ReqCurrentTime.encode();
            msg.push_str(&1i32.encode());
            loop{
                tx.send(msg.clone()).await.expect("Could not send heartbeat");
                time::sleep(time::Duration::from_secs(60)).await;
            }
        }, keep_alive_abort_registration);
        let _keep_alive_task = tokio::spawn(keep_alive_fut);
        let (account_tx, account) = account::init_account_channel();
        //start the reader task
        let (reader_abort_handle, reader_abort_registration) = AbortHandle::new_pair();
        let reader_fut = Abortable::new(async move {
            //caches
            let mut positions_cache= Vec::new();
            let mut contract_details_cache = HashMap::new();
            let mut executions_cache = HashMap::new();
            //pending requests
            let mut order_id_reqs = VecDeque::new();
            let mut requests = HashMap::new();
            //open order trackers
            let mut order_trackers = HashMap::new();
            //open tickers
            let mut tickers = HashMap::new();


            loop {
                let msg = reader.read().await.unwrap();
                loop {
                    match req_rx.try_recv() {
                        Ok(req) => match req {
                            Request::OrderID(sender) => {
                                order_id_reqs.push_back(sender)},
                            Request::ReqWithID{id,sender} => {
                                requests.insert(id, sender);}
                        },
                        Err(_) => break
                    }
                };
                //println!("{:?}", String::from_utf8_lossy(&msg));
                let frame = IBFrame::parse(&msg);
                match frame {
                    IBFrame::AccountCode(code) => account_tx.account_code.send(code).unwrap(),
                    IBFrame::AccountType(typ) => account_tx.account_type.send(typ).unwrap(),
                    IBFrame::AccountUpdateTime(time) => account_tx.update_time.send(time).unwrap(),
                    IBFrame::CashBalance(cash) => account_tx.cash_balance.send(cash).unwrap(),
                    IBFrame::EquityWithLoanValue(loan) => account_tx.equity_with_loan_value.send(loan).unwrap(),
                    IBFrame::ExcessLiquidity(liquidity) => account_tx.excess_liquidity.send(liquidity).unwrap(),
                    IBFrame::NetLiquidation(nav) => account_tx.net_liquidation.send(nav).unwrap(),
                    IBFrame::UnrealizedPnL(u_pnl) => account_tx.unrealized_pnl.send(u_pnl).unwrap(),
                    IBFrame::RealizedPnL(pnl) => account_tx.realized_pnl.send(pnl).unwrap(),
                    IBFrame::TotalCashBalance(balance) => account_tx.total_cash_balance.send(balance).unwrap(),
                    IBFrame::PortfolioValue(position) => positions_cache.push(position),
                    IBFrame::AccountUpdateEnd(_) => {
                        account_tx.portfolio.send(Some(positions_cache)).unwrap();
                        positions_cache = Vec::new();},
                    IBFrame::CurrentTime(dtime) => println!("Heartbeat at {}", dtime),
                    IBFrame::OrderID(id) => {
                        match order_id_reqs.pop_front() {
                            Some(sender) => sender.send(id).unwrap(),
                            None => println!("No pending order id request.")
                        }
                    },
                    IBFrame::ContractDetails{req_id: id,contract_details: details} => {
                        contract_details_cache.entry(id).or_insert(Vec::new());
                        contract_details_cache.get_mut(&id).unwrap().push(details);
                    },
                    IBFrame::ContractDetailsEnd(req_id) => {
                        match requests.remove_entry(&req_id) {
                            Some((_, sender)) => {

                                let _res = match contract_details_cache.remove_entry(&req_id) {
                                    Some((_, details)) => sender.send(Response::ContractDetails(details)),
                                    None => sender.send(Response::Empty)
                                };

                            },
                            None => println!("No pending contract details request for req_id {}", req_id)
                        };
                    },
                    IBFrame::OpenOrder{order,order_state} => {
                        let order_id = order.order_id;
                        match requests.remove_entry(&order_id) {
                            Some((_, sender)) => {
                                let (order_sender, order_receiver) = order::OrderTracker::new(order, order_state);
                                sender.send(Response::Order(order_receiver));
                                order_trackers.insert(order_id, order_sender);
                            },
                            None => {
                                if let Some(tracker) = order_trackers.get(&order_id) {
                                    tracker.order_state_tx.send(order_state);
                                    tracker.order_tx.send(order);
                                }
                            }
                        }
                        
                    },
                    IBFrame::Execution(execution) => {
                        if let Some(tracker) = order_trackers.get_mut(&execution.order_id) {
                            executions_cache.insert(execution.exec_id.clone(), execution.order_id);
                            tracker.executions_tx.send(execution).unwrap();
                        }
                    },
                    IBFrame::CommissionReport(report) => {
                        if let Some((_,order_id)) = executions_cache.remove_entry(&report.exec_id) {
                            if let Some(tracker) = order_trackers.get_mut(&order_id) {
                                match tracker.commission_reports_tx.send(report) {
                                    Err(error) => println!("{:?}", error),
                                    _ => ()
                                }
                            }
                        }
                    },
                    IBFrame::OrderStatus(status) => {
                        if let Some(tracker) = order_trackers.get(&status.order_id) {
                            match tracker.order_status_tx.send(Some(status)) {
                                Err(error) => println!("Order Status send error"),
                                _ => ()
                            }
                        }
                    }
                    IBFrame::PriceTick{id, kind, price, size, ..} => {
                        if let Some((_, req)) = requests.remove_entry(&id) {
                            let (ticker_sender, ticker) = ticker::Ticker::new();
                            tickers.insert(id, ticker_sender);
                            if let Ok(()) = req.send(Response::Ticker(ticker)) {} else {continue}; //else: request is dead
                        }
                        if let Some(t) = tickers.get_mut(&id) {
                            let ok = match kind {
                                TickType::Bid | TickType::DelayedBid => {
                                    if let Err(_) = t.bid.send(Some(price)) {false}
                                    else if let Err(_) = t.bid_size.send(size) {false}
                                    else {true}
                                },
                                TickType::Ask | TickType::DelayedAsk => {
                                    if let Err(_) = t.ask.send(Some(price)) {false}
                                    else if let Err(_) = t.ask_size.send(size) {false}
                                    else {true}

                                },
                                TickType::Last | TickType::DelayedLast => {
                                    if let Err(_) = t.last.send(Some(price)) {false}
                                    else if let Err(_) = t.last_size.send(size) {false}
                                    else {true}
                                }
                                _ => true
                            };
                            if !ok {tickers.remove_entry(&id);}    
                        };
                    },
                    IBFrame::SizeTick{id, kind, size} => {
                        if let Some((_, req)) = requests.remove_entry(&id) {
                            let (ticker_sender, ticker) = ticker::Ticker::new();
                            tickers.insert(id, ticker_sender);
                            if let Ok(()) = req.send(Response::Ticker(ticker)) {} else {continue}; //else: request is dead
                        }
                        if let Some(t) = tickers.get_mut(&id) {
                            let ok = match kind {
                                TickType::BidSize | TickType::DelayedBidSize => {
                                    if let Err(_) = t.bid_size.send(Some(size)) {false}
                                    else {true}
                                },
                                TickType::AskSize | TickType::DelayedAskSize => {
                                    if let Err(_) = t.ask_size.send(Some(size)) {false}
                                    else {true}

                                },
                                TickType::LastSize | TickType::DelayedLastSize => {
                                    if let Err(_) = t.last_size.send(Some(size)) {false}
                                    else {true}
                                }
                                TickType::ShortableShares => {
                                    if let Err(_) = t.shortable_shares.send(Some(size)) {false}
                                    else {true}
                                }
                                _ => true
                            };
                            if !ok {tickers.remove_entry(&id);}    
                        };
                    },
                    IBFrame::GenericTick{id, kind, val} => {
                        if let Some((_, req)) = requests.remove_entry(&id) {
                            let (ticker_sender, ticker) = ticker::Ticker::new();
                            tickers.insert(id, ticker_sender);
                            if let Ok(()) = req.send(Response::Ticker(ticker)) {} else {continue}; //else: request is dead
                        }
                        if let Some(t) = tickers.get_mut(&id) {
                            let ok = match kind {
                                TickType::Shortable => {
                                    if let Err(_) = t.short_availability.send(Some(ticker::ShortAvailability::from_f64(val))) {false}
                                    else {true}
                                }
                                _ => true
                            };
                            if !ok {tickers.remove_entry(&id);}    //ticker is dead
                        };
                    },
                    IBFrame::Bars{id, data} => {
                        if let Some((_, req)) = requests.remove_entry(&id) {
                            req.send(Response::Bars(data));
                        }
                    }
                    IBFrame::Error{id, code, msg} => {

                    }
                    _ => ()
                };
            }
        }, reader_abort_registration);
        let _reader_task = tokio::spawn(reader_fut);
        let mut client = IBClient {
            client_id,
            writer_abort_handle,
            reader_abort_handle,
            keep_alive_abort_handle,
            write_tx,
            req_tx,
            server_version,
            account,
            next_req_id: 0,
            next_order_id: 0,
            mkt_data_setting: MarketDataType::RealTime
        };
        //subscribe to account updates
        msg = Outgoing::ReqAcctData.encode();
        msg.push_str(&2i32.encode());
        msg.push_str(&true.encode());
        msg.push_str("\0");
        client.write_tx.send(msg).await?;
        //get the latest order id
        msg = Outgoing::ReqIds.encode();
        msg.push_str("1\01\0");
        let (resp_tx, resp_rx) = oneshot::channel();
        client.req_tx.send(Request::OrderID(resp_tx))?;
        client.write_tx.send(msg).await?;
        match resp_rx.await {
            Ok(id) => client.next_order_id = id,
            Err(err) => return Err(Box::new(err))
        }
        Ok(client)
    }

    pub fn net_liquidation_value(&self) -> Option<Decimal> {
        *self.account.net_liquidation.borrow()
    }

    pub fn cash_balance(&self) -> Option<Decimal> {
        *self.account.cash_balance.borrow()
    }

    pub fn excess_liquidity(&self) -> Option<Decimal> {
        *self.account.excess_liquidity.borrow()
    }

    fn get_next_req_id(&mut self) -> i32 {
        self.next_req_id += 1;
        self.next_req_id
    }

    fn get_next_order_id(&mut self) -> i32 {
        self.next_order_id += 1;
        self.next_order_id
    }

    pub async fn req_contract_details(&mut self, contract: &ib_contract::Contract) -> AsyncResult<Vec<ib_contract::ContractDetails>> {
        let mut msg = Outgoing::ReqContractData.encode();
        msg.push_str(&8i32.encode());
        let id = self.get_next_req_id();
        msg.push_str(&id.encode());
        msg.push_str(&contract.encode());
        let (rep_tx, rep_rx) = oneshot::channel();
        self.req_tx.send(Request::ReqWithID{id, sender: rep_tx})?;
        self.write_tx.send(msg).await?;
        match rep_rx.await {
            Ok(response) => 
            {
                match response {
                    Response::ContractDetails(contracts) => Ok(contracts),
                    _ => Err(Box::new(ResponseError{}))
                }
            },
            Err(error) => Err(Box::new(error))
        }
    }

    pub async fn place_order(&mut self, order: &order::Order) -> AsyncResult<order::OrderTracker> {
        let mut msg = Outgoing::PlaceOrder.encode();
        let id = self.get_next_order_id();
        msg.push_str(&id.encode());
        msg.push_str(&order.encode());
        let (rep_tx, rep_rx) = oneshot::channel();
        self.req_tx.send(Request::ReqWithID{id, sender: rep_tx})?;
        println!("{:?}", msg);
        self.write_tx.send(msg).await?;
        match rep_rx.await {
            Ok(response) => 
            {
                match response {
                    Response::Order(tracker) => Ok(tracker),
                    _ => Err(Box::new(ResponseError{}))
                }
            },
            Err(error) => Err(Box::new(error))
        }
    }

    pub async fn req_market_data(&mut self, contract: &ib_contract::Contract, snapshot: bool, regulatory: bool, 
        additional_data: Option<Vec<GenericTickType>>) -> AsyncResult<ticker::Ticker> {
        let mut msg = Outgoing::ReqMktData.encode();
        msg.push_str("11\0"); //version
        let id = self.get_next_req_id();
        msg.push_str(&id.encode());
        msg.push_str(&contract.encode_for_ticker());
        msg.push_str("0\0");
        
        if let Some(add_data) = additional_data {
            for i in 0..add_data.len()-1 {
                msg.push_str(&add_data[i].encode());
                msg.push_str(",");
            }
            if let Some(gen_tick) = add_data.last() {
                msg.push_str(&gen_tick.encode());
            }
        }
        
        msg.push_str("\0"); //generic tick data
        msg.push_str(&snapshot.encode());
        msg.push_str(&regulatory.encode());
        msg.push_str("\0");
        println!("{:?}", msg);
        let (req_tx, req_rx) = oneshot::channel();
        self.req_tx.send(Request::ReqWithID{id, sender: req_tx})?;
        self.write_tx.send(msg).await?;
        match req_rx.await {
            Ok(response) => 
            {
                match response {
                    Response::Ticker(ticker) => Ok(ticker),
                    _ => Err(Box::new(ResponseError{}))
                }
            },
            Err(err) => Err(Box::new(err))
        }
    }

    pub async fn req_historical_data<Tz: TimeZone> (&mut self, contract: &ib_contract::Contract, end_date_time: &DateTime<Tz>, 
        duration: HistoricalDataDuration, bar_period: HistoricalDataBarSize, what_to_show: HistoricalDataType, use_rth: bool) -> AsyncResult<bars::BarSeries>
        where
        <Tz as TimeZone>::Offset: std::fmt::Display
        {
        let mut msg = Outgoing::ReqHistoricalData.encode();
        let id = self.get_next_req_id();
        msg.push_str(&id.encode());
        msg.push_str(&contract.encode_for_hist_data());
        msg.push_str(&end_date_time.format("%Y%m%d %H:%M:%S").to_string().encode());
        msg.push_str(&bar_period.encode());
        msg.push_str(&duration.encode());
        msg.push_str(&use_rth.encode());
        msg.push_str(&what_to_show.encode());
        msg.push_str("1\00\0\0");
        let (resp_tx, resp_rx) = oneshot::channel();
        self.req_tx.send(Request::ReqWithID{id, sender: resp_tx});
        self.write_tx.send(msg).await?;
        match resp_rx.await {
            Ok(response) => 
            {
                match response {
                    Response::Bars(bars) => Ok(bars),
                    _ => Err(Box::new(ResponseError{}))
                }
            },
            Err(err) => Err(Box::new(err))
        }
    }

    pub async fn req_adj_historical_data(&mut self, contract: &ib_contract::Contract, duration: HistoricalDataDuration, bar_period: HistoricalDataBarSize, use_rth: bool) -> AsyncResult<bars::BarSeries> {
        let mut msg = Outgoing::ReqHistoricalData.encode();
        let id = self.get_next_req_id();
        msg.push_str(&id.encode());
        msg.push_str(&contract.encode_for_hist_data());
        msg.push_str("\0");
        msg.push_str(&bar_period.encode());
        msg.push_str(&duration.encode());
        msg.push_str(&use_rth.encode());
        msg.push_str("ADJUSTED_LAST\0");
        msg.push_str("1\00\0\0");
        let (resp_tx, resp_rx) = oneshot::channel();
        self.req_tx.send(Request::ReqWithID{id, sender: resp_tx});
        self.write_tx.send(msg).await?;
        match resp_rx.await {
            Ok(response) => 
            {
                match response {
                    Response::Bars(bars) => Ok(bars),
                    _ => Err(Box::new(ResponseError{}))
                }
            },
            Err(err) => Err(Box::new(err))
        }
    }

    pub async fn set_mkt_data_delayed(&mut self) -> AsyncResult<()> {
        let mut msg = Outgoing::ReqMarketDataType.encode();
        msg.push_str("1\0");
        msg.push_str(&MarketDataType::Delayed.encode());
        self.write_tx.send(msg).await?;
        self.mkt_data_setting = MarketDataType::Delayed;
        Ok(())
    }

    pub async fn set_mkt_data_real_time(&mut self) -> AsyncResult<()> {
        let mut msg = Outgoing::ReqMarketDataType.encode();
        msg.push_str("1\0");
        msg.push_str(&MarketDataType::RealTime.encode());
        self.write_tx.send(msg).await?;
        self.mkt_data_setting = MarketDataType::RealTime;
        Ok(())
    }

}

impl Drop for IBClient {
    fn drop(&mut self) {
        self.keep_alive_abort_handle.abort();
        self.writer_abort_handle.abort();
        self.reader_abort_handle.abort();
    }
}