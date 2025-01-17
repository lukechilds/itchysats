use crate::bitmex_price_feed;
use crate::db;
use crate::model;
use crate::model::cfd::calculate_long_liquidation_price;
use crate::model::cfd::calculate_long_margin;
use crate::model::cfd::calculate_profit;
use crate::model::cfd::calculate_short_margin;
use crate::model::cfd::CfdEvent;
use crate::model::cfd::Dlc;
use crate::model::cfd::Event;
use crate::model::cfd::OrderId;
use crate::model::cfd::Role;
use crate::model::cfd::RolloverProposal;
use crate::model::cfd::SettlementKind;
use crate::model::cfd::SettlementProposal;
use crate::model::Identity;
use crate::model::Leverage;
use crate::model::Position;
use crate::model::Price;
use crate::model::Timestamp;
use crate::model::TradingPair;
use crate::model::Usd;
use crate::send_async_safe::SendAsyncSafe;
use crate::Order;
use anyhow::Result;
use async_trait::async_trait;
use bdk::bitcoin::Amount;
use bdk::bitcoin::Network;
use bdk::bitcoin::SignedAmount;
use bdk::bitcoin::Txid;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde::Serialize;
use sqlx::pool::PoolConnection;
use std::collections::HashMap;
use time::OffsetDateTime;
use tokio::sync::watch;
use xtra::Context;
use xtra_productivity::xtra_productivity;

/// Amend a given settlement proposal (if `proposal.is_none()`, it should be removed)
pub struct UpdateSettlementProposal {
    pub order: OrderId,
    pub proposal: Option<(SettlementProposal, SettlementKind)>,
}

/// Amend a given rollover proposal (if `proposal.is_none()`, it should be removed)
pub struct UpdateRollOverProposal {
    pub order: OrderId,
    pub proposal: Option<(RolloverProposal, SettlementKind)>,
}

/// Store the latest state of `T` for display purposes
/// (replaces previously stored values)
pub struct Update<T>(pub T);

/// Message indicating that the Cfds in the projection need to be reloaded, as at
/// least one of the Cfds has changed.
pub struct CfdsChanged;

pub struct Actor {
    db: sqlx::SqlitePool,
    tx: Tx,
    state: State,
}

pub struct Feeds {
    pub quote: watch::Receiver<Option<Quote>>,
    pub order: watch::Receiver<Option<CfdOrder>>,
    pub connected_takers: watch::Receiver<Vec<Identity>>,
    pub cfds: watch::Receiver<Vec<Cfd>>,
}

impl Actor {
    pub fn new(db: sqlx::SqlitePool, _role: Role, network: Network) -> (Self, Feeds) {
        let (tx_cfds, rx_cfds) = watch::channel(Vec::new());
        let (tx_order, rx_order) = watch::channel(None);
        let (tx_quote, rx_quote) = watch::channel(None);
        let (tx_connected_takers, rx_connected_takers) = watch::channel(Vec::new());

        let actor = Self {
            db,
            tx: Tx {
                cfds: tx_cfds,
                order: tx_order,
                quote: tx_quote,
                connected_takers: tx_connected_takers,
            },
            state: State::new(network),
        };
        let feeds = Feeds {
            cfds: rx_cfds,
            order: rx_order,
            quote: rx_quote,
            connected_takers: rx_connected_takers,
        };

        (actor, feeds)
    }

    async fn refresh_cfds(&mut self) {
        let mut conn = match self.db.acquire().await {
            Ok(conn) => conn,
            Err(e) => {
                tracing::warn!("Failed to acquire DB connection: {}", e);
                return;
            }
        };
        let cfds = match load_and_hydrate_cfds(
            &mut conn,
            self.state.quote,
            self.state.network,
            &self.state.settlement_proposals,
            &self.state.rollover_proposals,
        )
        .await
        {
            Ok(cfds) => cfds,
            Err(e) => {
                tracing::warn!("Failed to load CFDs: {:#}", e);
                return;
            }
        };

        let _ = self.tx.cfds.send(cfds);
    }
}

async fn load_and_hydrate_cfds(
    conn: &mut PoolConnection<sqlx::Sqlite>,
    quote: Option<bitmex_price_feed::Quote>,
    network: Network,
    settlement_proposals: &HashMap<OrderId, (SettlementProposal, SettlementKind)>,
    rollover_proposals: &HashMap<OrderId, (RolloverProposal, SettlementKind)>,
) -> Result<Vec<Cfd>> {
    let ids = db::load_all_cfd_ids(conn).await?;

    let mut cfds = Vec::with_capacity(ids.len());

    for id in ids {
        let (cfd, events) = db::load_cfd(id, conn).await?;
        let role = cfd.role;

        let cfd = events.into_iter().fold(Cfd::new(cfd, quote), |cfd, event| {
            cfd.apply(
                event,
                network,
                settlement_proposals.get(&id),
                rollover_proposals.get(&id),
                role,
            )
        });

        cfds.push(cfd);
    }

    Ok(cfds)
}

#[derive(Clone, Debug, Serialize)]
pub struct Cfd {
    pub order_id: OrderId,
    #[serde(with = "round_to_two_dp")]
    pub initial_price: Price,

    pub leverage: Leverage,
    pub trading_pair: TradingPair,
    pub position: Position,
    #[serde(with = "round_to_two_dp")]
    pub liquidation_price: Price,

    #[serde(with = "round_to_two_dp")]
    pub quantity_usd: Usd,

    #[serde(with = "::bdk::bitcoin::util::amount::serde::as_btc")]
    pub margin: Amount,
    #[serde(with = "::bdk::bitcoin::util::amount::serde::as_btc")]
    pub margin_counterparty: Amount,

    #[serde(with = "::bdk::bitcoin::util::amount::serde::as_btc::opt")]
    pub profit_btc: Option<SignedAmount>,
    pub profit_percent: Option<String>,

    pub state: CfdState,
    pub actions: Vec<CfdAction>, // TODO: This should be a HashMap.
    pub state_transition_timestamp: i64,

    pub details: CfdDetails,

    #[serde(with = "::time::serde::timestamp::option")]
    pub expiry_timestamp: Option<OffsetDateTime>,

    pub counterparty: Identity,

    // This is a bit awkward but we need this to compute the appropriate state as more events are
    // processed.
    #[serde(skip)]
    latest_dlc: Option<Dlc>,
}

impl Cfd {
    fn new(
        db::Cfd {
            id,
            position,
            initial_price,
            leverage,
            quantity_usd,
            counterparty_network_identity,
            role,
            ..
        }: db::Cfd,
        latest_quote: Option<bitmex_price_feed::Quote>,
    ) -> Self {
        let long_margin = calculate_long_margin(initial_price, quantity_usd, leverage);
        let short_margin = calculate_short_margin(initial_price, quantity_usd);

        let (margin, margin_counterparty) = match position {
            Position::Long => (long_margin, short_margin),
            Position::Short => (short_margin, long_margin),
        };
        let liquidation_price = calculate_long_liquidation_price(leverage, initial_price);

        let latest_price = match (latest_quote, role) {
            (None, _) => None,
            (Some(quote), Role::Maker) => Some(quote.for_maker()),
            (Some(quote), Role::Taker) => Some(quote.for_taker()),
        };

        let (profit_btc_latest_price, profit_percent_latest_price) = latest_price.and_then(|latest_price| {
            match calculate_profit(initial_price, latest_price, quantity_usd, leverage, position) {
                Ok(profit) => Some(profit),
                Err(e) => {
                    tracing::warn!("Failed to calculate profit/loss {:#}", e);

                    None
                }
            }
        }).map(|(in_btc, in_percent)| (Some(in_btc), Some(in_percent.round_dp(1).to_string())))
            .unwrap_or_else(|| {
                tracing::debug!(order_id = %id, "Unable to calculate profit/loss without current price");

                (None, None)
            });

        let initial_actions = if role == Role::Maker {
            vec![CfdAction::AcceptOrder, CfdAction::RejectOrder]
        } else {
            vec![]
        };

        Self {
            order_id: id,
            initial_price,
            leverage,
            trading_pair: TradingPair::BtcUsd,
            position,
            liquidation_price,
            quantity_usd,
            margin,
            margin_counterparty,

            // By default, we assume profit should be based on the latest price!
            profit_btc: profit_btc_latest_price,
            profit_percent: profit_percent_latest_price,

            state: CfdState::PendingSetup,
            actions: initial_actions,
            state_transition_timestamp: 0,
            details: CfdDetails {
                tx_url_list: vec![],
                payout: None,
            },
            expiry_timestamp: None,
            counterparty: counterparty_network_identity,
            latest_dlc: None,
        }
    }

    // TODO: There is probably a better way of doing this?
    // The issue is, we need to re-hydrate the CFD to get the latest state but at the same time
    // incorporate other data like network, current price, etc ...
    fn apply(
        mut self,
        event: Event,
        network: Network,
        pending_settlement_proposal: Option<&(SettlementProposal, SettlementKind)>,
        pending_rollover_proposal: Option<&(RolloverProposal, SettlementKind)>,
        role: Role,
    ) -> Self {
        // First, try to set state based on event.
        let (state, actions) = match event.event {
            CfdEvent::ContractSetupCompleted { dlc } => {
                self.details.tx_url_list.push(TxUrl::new(
                    dlc.lock.0.txid(),
                    network,
                    TxLabel::Lock,
                ));
                self.latest_dlc = Some(dlc);

                (CfdState::PendingOpen, vec![])
            }
            CfdEvent::ContractSetupFailed => {
                // Don't display profit for failed contracts.
                self.profit_btc = None;
                self.profit_percent = None;

                (CfdState::SetupFailed, vec![])
            }
            CfdEvent::OfferRejected => {
                // Don't display profit for rejected contracts.
                self.profit_btc = None;
                self.profit_percent = None;

                (CfdState::Rejected, vec![])
            }
            CfdEvent::RolloverCompleted { dlc } => {
                self.latest_dlc = Some(dlc);

                (CfdState::Open, vec![])
            }
            CfdEvent::RolloverRejected => (CfdState::Open, vec![]),
            CfdEvent::RolloverFailed => (CfdState::Open, vec![]),
            CfdEvent::CollaborativeSettlementCompleted {
                spend_tx, price, ..
            } => {
                self.details.tx_url_list.push(TxUrl::new(
                    spend_tx.txid(),
                    network,
                    TxLabel::Collaborative,
                ));

                let (profit_btc, profit_percent) = self.maybe_calculate_profit(price);
                self.profit_btc = profit_btc;
                self.profit_percent = profit_percent;

                (CfdState::PendingClose, vec![])
            }
            CfdEvent::CollaborativeSettlementRejected { commit_tx } => {
                self.details.tx_url_list.push(TxUrl::new(
                    commit_tx.txid(),
                    network,
                    TxLabel::Commit,
                ));

                (CfdState::PendingCommit, vec![])
            }
            CfdEvent::CollaborativeSettlementFailed { commit_tx } => {
                self.details.tx_url_list.push(TxUrl::new(
                    commit_tx.txid(),
                    network,
                    TxLabel::Commit,
                ));

                (CfdState::PendingCommit, vec![])
            }
            CfdEvent::LockConfirmed => (CfdState::Open, vec![CfdAction::Commit, CfdAction::Settle]),
            CfdEvent::CommitConfirmed => {
                // pretty weird if this is not defined ...
                if let Some(dlc) = self.latest_dlc.as_ref() {
                    self.details.tx_url_list.push(TxUrl::new(
                        dlc.commit.0.txid(),
                        network,
                        TxLabel::Commit,
                    ));
                }
                (CfdState::OpenCommitted, vec![])
            }
            CfdEvent::CetConfirmed => (CfdState::Closed, vec![]),
            CfdEvent::RefundConfirmed => {
                if let Some(dlc) = self.latest_dlc.as_ref() {
                    self.details.tx_url_list.push(TxUrl::new(
                        dlc.refund.0.txid(),
                        network,
                        TxLabel::Refund,
                    ));
                }
                (CfdState::Refunded, vec![])
            }
            CfdEvent::CollaborativeSettlementConfirmed => (CfdState::Closed, vec![]),
            CfdEvent::CetTimelockConfirmedPriorOracleAttestation => {
                (CfdState::OpenCommitted, self.actions)
            }
            CfdEvent::CetTimelockConfirmedPostOracleAttestation { .. } => {
                (CfdState::PendingCet, self.actions)
            }
            CfdEvent::RefundTimelockConfirmed { .. } => (self.state, self.actions),
            CfdEvent::OracleAttestedPriorCetTimelock {
                price, commit_tx, ..
            } => {
                let (profit_btc, profit_percent) = self.maybe_calculate_profit(price);
                self.profit_btc = profit_btc;
                self.profit_percent = profit_percent;

                self.details.tx_url_list.push(TxUrl::new(
                    commit_tx.txid(),
                    network,
                    TxLabel::Commit,
                ));

                // Only allow committing once the oracle attested.
                (CfdState::PendingCommit, vec![])
            }
            CfdEvent::OracleAttestedPostCetTimelock { cet, price } => {
                self.details
                    .tx_url_list
                    .push(TxUrl::new(cet.txid(), network, TxLabel::Cet));

                let (profit_btc, profit_percent) = self.maybe_calculate_profit(price);
                self.profit_btc = profit_btc;
                self.profit_percent = profit_percent;

                // Only allow committing once the oracle attested.
                (CfdState::PendingCet, vec![CfdAction::Commit])
            }
            CfdEvent::ManualCommit { tx } => {
                self.details
                    .tx_url_list
                    .push(TxUrl::new(tx.txid(), network, TxLabel::Commit));

                (CfdState::PendingCommit, vec![])
            }
            CfdEvent::RevokeConfirmed => todo!("Deal with revoked"),
        };

        self.state = state;
        self.actions = actions;

        // If we have pending proposals, override the state

        match pending_settlement_proposal {
            Some((_, SettlementKind::Incoming)) => {
                self.state = CfdState::IncomingSettlementProposal;

                if role == Role::Maker {
                    self.actions = vec![CfdAction::AcceptSettlement, CfdAction::RejectSettlement];
                }
            }
            Some((_, SettlementKind::Outgoing)) => {
                self.state = CfdState::OutgoingSettlementProposal;
            }
            None => {}
        }
        match pending_rollover_proposal {
            Some((_, SettlementKind::Incoming)) => {
                self.state = CfdState::IncomingRollOverProposal;

                if role == Role::Maker {
                    self.actions = vec![CfdAction::AcceptRollOver, CfdAction::RejectRollOver];
                }
            }
            Some((_, SettlementKind::Outgoing)) => {
                self.state = CfdState::OutgoingRollOverProposal;
            }
            None => {}
        }

        self
    }

    fn maybe_calculate_profit(
        &self,
        closing_price: Price,
    ) -> (Option<SignedAmount>, Option<String>) {
        match calculate_profit(
            self.initial_price,
            closing_price,
            self.quantity_usd,
            self.leverage,
            self.position,
        ) {
            Ok((profit_btc, profit_percent)) => {
                (Some(profit_btc), Some(profit_percent.to_string()))
            }
            Err(err) => {
                tracing::error!(initial_price=%self.initial_price, closing_price=%closing_price, quantity=%self.quantity_usd, leverage=%self.leverage, position=%self.position, "Profit calculation failed: {:#}", err);
                (None, None)
            }
        }
    }
}

/// Internal struct to keep all the senders around in one place
struct Tx {
    pub cfds: watch::Sender<Vec<Cfd>>,
    pub order: watch::Sender<Option<CfdOrder>>,
    pub quote: watch::Sender<Option<Quote>>,
    // TODO: Use this channel to communicate maker status as well with generic
    // ID of connected counterparties
    pub connected_takers: watch::Sender<Vec<Identity>>,
}

/// Internal struct to keep state in one place
struct State {
    network: Network,
    quote: Option<bitmex_price_feed::Quote>,
    settlement_proposals: HashMap<OrderId, (SettlementProposal, SettlementKind)>,
    rollover_proposals: HashMap<OrderId, (RolloverProposal, SettlementKind)>,
}

impl State {
    fn new(network: Network) -> Self {
        Self {
            network,
            quote: None,
            settlement_proposals: Default::default(),
            rollover_proposals: Default::default(),
        }
    }

    fn amend_settlement_proposal(&mut self, update: UpdateSettlementProposal) {
        match update.proposal {
            Some(proposal) => {
                self.settlement_proposals.insert(update.order, proposal);
            }
            None => {
                self.settlement_proposals.remove(&update.order);
            }
        }
    }

    fn amend_rollover_proposal(&mut self, update: UpdateRollOverProposal) {
        match update.proposal {
            Some(proposal) => {
                self.rollover_proposals.insert(update.order, proposal);
            }
            None => {
                self.rollover_proposals.remove(&update.order);
            }
        }
    }

    fn update_quote(&mut self, quote: bitmex_price_feed::Quote) {
        self.quote = Some(quote);
    }
}

#[xtra_productivity]
impl Actor {
    async fn handle(&mut self, _: CfdsChanged) {
        self.refresh_cfds().await
    }

    fn handle(&mut self, msg: Update<Option<Order>>) {
        let _ = self.tx.order.send(msg.0.map(|x| x.into()));
    }

    fn handle(&mut self, msg: Update<bitmex_price_feed::Quote>) {
        self.state.update_quote(msg.0);
        let _ = self.tx.quote.send(Some(msg.0.into()));
        self.refresh_cfds().await;
    }

    fn handle(&mut self, msg: Update<Vec<model::Identity>>) {
        let _ = self.tx.connected_takers.send(msg.0);
    }

    fn handle(&mut self, msg: UpdateSettlementProposal) {
        self.state.amend_settlement_proposal(msg);
        self.refresh_cfds().await;
    }

    fn handle(&mut self, msg: UpdateRollOverProposal) {
        self.state.amend_rollover_proposal(msg);
        self.refresh_cfds().await;
    }
}

#[async_trait]
impl xtra::Actor for Actor {
    async fn started(&mut self, ctx: &mut Context<Self>) {
        let this = ctx.address().expect("we just started");

        // this will make us load all cfds from the DB
        this.send_async_safe(CfdsChanged)
            .await
            .expect("we just started");
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Quote {
    bid: Price,
    ask: Price,
    last_updated_at: Timestamp,
}

impl From<bitmex_price_feed::Quote> for Quote {
    fn from(quote: bitmex_price_feed::Quote) -> Self {
        Quote {
            bid: quote.bid,
            ask: quote.ask,
            last_updated_at: quote.timestamp,
        }
    }
}

// FIXME: Remove this hack when it's not needed
impl From<Quote> for bitmex_price_feed::Quote {
    fn from(quote: Quote) -> Self {
        Self {
            timestamp: quote.last_updated_at,
            bid: quote.bid,
            ask: quote.ask,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CfdOrder {
    pub id: OrderId,

    pub trading_pair: TradingPair,
    pub position: Position,

    #[serde(with = "round_to_two_dp")]
    pub price: Price,

    #[serde(with = "round_to_two_dp")]
    pub min_quantity: Usd,
    #[serde(with = "round_to_two_dp")]
    pub max_quantity: Usd,

    pub leverage: Leverage,
    #[serde(with = "round_to_two_dp")]
    pub liquidation_price: Price,

    pub creation_timestamp: Timestamp,
    pub settlement_time_interval_in_secs: u64,
}

impl From<Order> for CfdOrder {
    fn from(order: Order) -> Self {
        Self {
            id: order.id,
            trading_pair: order.trading_pair,
            position: order.position,
            price: order.price,
            min_quantity: order.min_quantity,
            max_quantity: order.max_quantity,
            leverage: order.leverage,
            liquidation_price: order.liquidation_price,
            creation_timestamp: order.creation_timestamp,
            settlement_time_interval_in_secs: order
                .settlement_interval
                .whole_seconds()
                .try_into()
                .expect("settlement_time_interval_hours is always positive number"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum CfdState {
    PendingSetup,
    Rejected,
    PendingOpen,
    Open,
    PendingCommit,
    PendingCet,
    PendingClose,
    OpenCommitted,
    IncomingSettlementProposal,
    OutgoingSettlementProposal,
    IncomingRollOverProposal,
    OutgoingRollOverProposal,
    Closed,
    PendingRefund,
    Refunded,
    SetupFailed,
}

#[derive(Debug, Clone, Serialize)]
pub struct CfdDetails {
    // TODO: I think there should be one field per tx URL otherwise we can add duplicate entries
    // easily ...
    tx_url_list: Vec<TxUrl>,
    #[serde(with = "::bdk::bitcoin::util::amount::serde::as_btc::opt")]
    payout: Option<Amount>,
}

#[derive(Debug, derive_more::Display, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum CfdAction {
    AcceptOrder,
    RejectOrder,
    Commit,
    Settle,
    AcceptSettlement,
    RejectSettlement,
    AcceptRollOver,
    RejectRollOver,
}

mod round_to_two_dp {
    use super::*;
    use serde::Serializer;

    pub trait ToDecimal {
        fn to_decimal(&self) -> Decimal;
    }

    impl ToDecimal for Usd {
        fn to_decimal(&self) -> Decimal {
            self.into_decimal()
        }
    }

    impl ToDecimal for Price {
        fn to_decimal(&self) -> Decimal {
            self.into_decimal()
        }
    }

    pub fn serialize<D: ToDecimal, S: Serializer>(
        value: &D,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let decimal = value.to_decimal();
        let decimal = decimal.round_dp(2);

        Serialize::serialize(&decimal, serializer)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use rust_decimal_macros::dec;
        use serde_test::assert_ser_tokens;
        use serde_test::Token;

        #[derive(Serialize)]
        #[serde(transparent)]
        struct WithOnlyTwoDecimalPlaces<I: ToDecimal> {
            #[serde(with = "super")]
            inner: I,
        }

        #[test]
        fn usd_serializes_with_only_cents() {
            let usd = WithOnlyTwoDecimalPlaces {
                inner: model::Usd::new(dec!(1000.12345)),
            };

            assert_ser_tokens(&usd, &[Token::Str("1000.12")]);
        }

        #[test]
        fn price_serializes_with_only_cents() {
            let price = WithOnlyTwoDecimalPlaces {
                inner: model::Price::new(dec!(1000.12345)).unwrap(),
            };

            assert_ser_tokens(&price, &[Token::Str("1000.12")]);
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TxUrl {
    pub label: TxLabel,
    pub url: String,
}

/// Construct a mempool.space URL for a given txid
pub fn to_mempool_url(txid: Txid, network: Network) -> String {
    match network {
        Network::Bitcoin => format!("https://mempool.space/tx/{}", txid),
        Network::Testnet => format!("https://mempool.space/testnet/tx/{}", txid),
        Network::Signet => format!("https://mempool.space/signet/tx/{}", txid),
        Network::Regtest => txid.to_string(),
    }
}

impl TxUrl {
    pub fn new(txid: Txid, network: Network, label: TxLabel) -> Self {
        Self {
            label,
            url: to_mempool_url(txid, network),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub enum TxLabel {
    Lock,
    Commit,
    Cet,
    Refund,
    Collaborative,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_snapshot_test() {
        // Make sure to update the UI after changing this test!

        let json = serde_json::to_string(&CfdState::PendingSetup).unwrap();
        assert_eq!(json, "\"PendingSetup\"");
        let json = serde_json::to_string(&CfdState::Rejected).unwrap();
        assert_eq!(json, "\"Rejected\"");
        let json = serde_json::to_string(&CfdState::PendingOpen).unwrap();
        assert_eq!(json, "\"PendingOpen\"");
        let json = serde_json::to_string(&CfdState::Open).unwrap();
        assert_eq!(json, "\"Open\"");
        let json = serde_json::to_string(&CfdState::OpenCommitted).unwrap();
        assert_eq!(json, "\"OpenCommitted\"");
        let json = serde_json::to_string(&CfdState::PendingRefund).unwrap();
        assert_eq!(json, "\"PendingRefund\"");
        let json = serde_json::to_string(&CfdState::Refunded).unwrap();
        assert_eq!(json, "\"Refunded\"");
        let json = serde_json::to_string(&CfdState::SetupFailed).unwrap();
        assert_eq!(json, "\"SetupFailed\"");
    }
}
