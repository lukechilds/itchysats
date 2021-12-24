use crate::model::BitMexPriceEventId;
use crate::model::Identity;
use crate::model::InversePrice;
use crate::model::Leverage;
use crate::model::Percent;
use crate::model::Position;
use crate::model::Price;
use crate::model::Timestamp;
use crate::model::TradingPair;
use crate::model::Usd;
use crate::oracle;
use crate::payout_curve;
use crate::setup_contract::RolloverParams;
use crate::setup_contract::SetupParams;
use crate::SETTLEMENT_INTERVAL;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use bdk::bitcoin::secp256k1::SecretKey;
use bdk::bitcoin::secp256k1::Signature;
use bdk::bitcoin::Address;
use bdk::bitcoin::Amount;
use bdk::bitcoin::PublicKey;
use bdk::bitcoin::Script;
use bdk::bitcoin::SignedAmount;
use bdk::bitcoin::Transaction;
use bdk::bitcoin::Txid;
use bdk::descriptor::Descriptor;
use bdk::miniscript::DescriptorTrait;
use maia::finalize_spend_transaction;
use maia::secp256k1_zkp;
use maia::secp256k1_zkp::EcdsaAdaptorSignature;
use maia::secp256k1_zkp::SECP256K1;
use maia::spending_tx_sighash;
use maia::TransactionExt;
use rocket::request::FromParam;
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::Decimal;
use serde::de::Error as _;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::fmt;
use std::ops::RangeInclusive;
use std::str;
use time::Duration;
use time::OffsetDateTime;
use uuid::adapter::Hyphenated;
use uuid::Uuid;

pub const CET_TIMELOCK: u32 = 12;

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, sqlx::Type)]
#[sqlx(transparent)]
pub struct OrderId(Hyphenated);

impl Serialize for OrderId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for OrderId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let uuid = String::deserialize(deserializer)?;
        let uuid = uuid.parse::<Uuid>().map_err(D::Error::custom)?;

        Ok(Self(uuid.to_hyphenated()))
    }
}

impl Default for OrderId {
    fn default() -> Self {
        Self(Uuid::new_v4().to_hyphenated())
    }
}

impl fmt::Display for OrderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<'v> FromParam<'v> for OrderId {
    type Error = uuid::Error;

    fn from_param(param: &'v str) -> Result<Self, Self::Error> {
        let uuid = param.parse::<Uuid>()?;
        Ok(OrderId(uuid.to_hyphenated()))
    }
}

// TODO: Could potentially remove this and use the Role in the Order instead
/// Origin of the order
#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, sqlx::Type)]
pub enum Origin {
    Ours,
    Theirs,
}

/// Role in the Cfd
#[derive(Debug, Copy, Clone, PartialEq, sqlx::Type)]
pub enum Role {
    Maker,
    Taker,
}

impl From<Origin> for Role {
    fn from(origin: Origin) -> Self {
        match origin {
            Origin::Ours => Role::Maker,
            Origin::Theirs => Role::Taker,
        }
    }
}

/// A concrete order created by a maker for a taker
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Order {
    pub id: OrderId,

    pub trading_pair: TradingPair,
    pub position: Position,

    pub price: Price,

    // TODO: [post-MVP] Representation of the contract size; at the moment the contract size is
    //  always 1 USD
    pub min_quantity: Usd,
    pub max_quantity: Usd,

    pub leverage: Leverage,

    // TODO: Remove from order, can be calculated
    pub liquidation_price: Price,

    pub creation_timestamp: Timestamp,

    /// The duration that will be used for calculating the settlement timestamp
    pub settlement_interval: Duration,

    pub origin: Origin,

    /// The id of the event to be used for price attestation
    ///
    /// The maker includes this into the Order based on the Oracle announcement to be used.
    pub oracle_event_id: BitMexPriceEventId,

    pub fee_rate: u32,
}

impl Order {
    pub fn new_short(
        price: Price,
        min_quantity: Usd,
        max_quantity: Usd,
        origin: Origin,
        oracle_event_id: BitMexPriceEventId,
        settlement_interval: Duration,
        fee_rate: u32,
    ) -> Result<Self> {
        let leverage = Leverage::new(2)?;
        let liquidation_price = calculate_long_liquidation_price(leverage, price);

        Ok(Order {
            id: OrderId::default(),
            price,
            min_quantity,
            max_quantity,
            leverage,
            trading_pair: TradingPair::BtcUsd,
            liquidation_price,
            position: Position::Short,
            creation_timestamp: Timestamp::now(),
            settlement_interval,
            origin,
            oracle_event_id,
            fee_rate,
        })
    }
}

/// Proposed collaborative settlement
#[derive(Debug, Clone)]
pub struct SettlementProposal {
    pub order_id: OrderId,
    pub timestamp: Timestamp,
    pub taker: Amount,
    pub maker: Amount,
    pub price: Price,
}

/// Proposed collaborative settlement
#[derive(Debug, Clone)]
pub struct RolloverProposal {
    pub order_id: OrderId,
    pub timestamp: Timestamp,
}

#[derive(Debug, Clone)]
pub enum SettlementKind {
    Incoming,
    Outgoing,
}

#[derive(thiserror::Error, Debug, PartialEq)]
pub enum CannotRollover {
    #[error("Cfd does not have a dlc")]
    NoDlc,
    #[error("The Cfd is already expired")]
    AlreadyExpired,
    #[error("The Cfd was just rolled over")]
    WasJustRolledOver,
    #[error("Cannot roll over in state {state}")]
    WrongState { state: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub timestamp: Timestamp,
    pub id: OrderId,
    pub event: CfdEvent,
}

impl Event {
    pub fn new(id: OrderId, event: CfdEvent) -> Self {
        Event {
            timestamp: Timestamp::now(),
            id,
            event,
        }
    }
}

/// CfdEvents used by the maker and taker, some events are only for one role
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
#[serde(tag = "name", content = "data")]
pub enum CfdEvent {
    ContractSetupCompleted {
        dlc: Dlc,
    },

    ContractSetupFailed,
    OfferRejected,

    RolloverCompleted {
        dlc: Dlc,
    },
    RolloverRejected,
    RolloverFailed,

    CollaborativeSettlementCompleted {
        #[serde(with = "hex_transaction")]
        spend_tx: Transaction,
        script: Script,
        price: Price,
    },
    CollaborativeSettlementRejected {
        #[serde(with = "hex_transaction")]
        commit_tx: Transaction,
    },
    // TODO: What does "failed" mean here? Do we have to record this as event? what would it mean?
    CollaborativeSettlementFailed {
        #[serde(with = "hex_transaction")]
        commit_tx: Transaction,
    },

    // TODO: The monitoring events should move into the monitor once we use multiple
    // aggregates in different actors
    LockConfirmed,
    CommitConfirmed,
    CetConfirmed,
    RefundConfirmed,
    RevokeConfirmed,
    CollaborativeSettlementConfirmed,

    CetTimelockConfirmedPriorOracleAttestation,
    CetTimelockConfirmedPostOracleAttestation {
        #[serde(with = "hex_transaction")]
        cet: Transaction,
    },

    RefundTimelockConfirmed {
        #[serde(with = "hex_transaction")]
        refund_tx: Transaction,
    },

    // TODO: Once we use multiple aggregates in different actors we could change this to something
    // like CetReadyForPublication that is emitted by the CfdActor. The Oracle actor would
    // take care of saving and broadcasting an attestation event that can be picked up by the
    // wallet actor which can then decide to publish the CetReadyForPublication event.
    OracleAttestedPriorCetTimelock {
        #[serde(with = "hex_transaction")]
        timelocked_cet: Transaction,
        #[serde(with = "hex_transaction")]
        commit_tx: Transaction,
        price: Price,
    },
    OracleAttestedPostCetTimelock {
        #[serde(with = "hex_transaction")]
        cet: Transaction,
        price: Price,
    },
    ManualCommit {
        #[serde(with = "hex_transaction")]
        tx: Transaction,
    },
}

impl CfdEvent {
    pub fn to_json(&self) -> (String, String) {
        let value = serde_json::to_value(self).expect("serialization to always work");
        let object = value.as_object().expect("always an object");

        let name = object
            .get("name")
            .expect("to have property `name`")
            .as_str()
            .expect("name to be `string`")
            .to_owned();
        let data = object.get("data").cloned().unwrap_or_default().to_string();

        (name, data)
    }

    pub fn from_json(name: String, data: String) -> Result<Self> {
        use serde_json::json;

        let data = serde_json::from_str::<serde_json::Value>(&data)?;

        let event = serde_json::from_value::<Self>(json!({
            "name": name,
            "data": data
        }))?;

        Ok(event)
    }
}

/// Models the cfd state of the taker
///
/// Upon `Command`s, that are reaction to something happening in the system, we decide to
/// produce `Event`s that are saved in the database. After saving an `Event` in the database
/// we apply the event to the aggregate producing a new aggregate (representing the latest state
/// `version`). To bring a cfd into a certain state version we load all events from the
/// database and apply them in order (order by version).
#[derive(Debug, PartialEq)]
pub struct Cfd {
    version: u64,

    // static
    id: OrderId,
    position: Position,
    initial_price: Price,
    leverage: Leverage,
    settlement_interval: Duration,
    quantity: Usd,
    counterparty_network_identity: Identity,
    role: Role,

    // dynamic (based on events)
    dlc: Option<Dlc>,

    /// Holds the decrypted CET transaction once it is available in the CFD lifecycle
    ///
    /// Only `Some` in case we receive the attestation after the CET timelock expiry.
    /// This does _not_ imply that the transaction is actually confirmed.
    cet: Option<Transaction>,

    /// Holds the decrypted commit transaction once it is available in the CFD lifecycle
    ///
    /// Only `Some` in case we receive the attestation before the CET timelock expiry.
    /// This does _not_ imply that the transaction is actually confirmed.
    commit_tx: Option<Transaction>,

    collaborative_settlement_spend_tx: Option<Transaction>,

    refund_tx: Option<Transaction>,

    lock_finality: bool,
    commit_finality: bool,
    refund_finality: bool,
    cet_finality: bool,
    collaborative_settlement_finality: bool,

    cet_timelock_expired: bool,
    refund_timelock_expired: bool,
}

impl Cfd {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: OrderId,
        position: Position,
        initial_price: Price,
        leverage: Leverage,
        settlement_interval: Duration, /* TODO: Make a newtype that enforces hours only so
                                        * we don't have to deal with precisions in the
                                        * database. */
        role: Role,
        quantity: Usd,
        counterparty_network_identity: Identity,
    ) -> Self {
        Cfd {
            version: 0,
            id,
            position,
            initial_price,
            leverage,
            settlement_interval,
            quantity,
            counterparty_network_identity,
            role,
            dlc: None,
            cet: None,
            commit_tx: None,
            collaborative_settlement_spend_tx: None,
            refund_tx: None,
            lock_finality: false,
            commit_finality: false,
            refund_finality: false,
            cet_finality: false,
            collaborative_settlement_finality: false,
            cet_timelock_expired: false,
            refund_timelock_expired: false,
        }
    }

    /// A convenience method, creating a Cfd from an Order
    pub fn from_order(
        order: Order,
        position: Position,
        quantity: Usd,
        counterparty_network_identity: Identity,
        role: Role,
    ) -> Self {
        Cfd::new(
            order.id,
            position,
            order.price,
            order.leverage,
            order.settlement_interval,
            role,
            quantity,
            counterparty_network_identity,
        )
    }

    /// Creates a new [`Cfd`] and rehydrates it from the given list of events.
    #[allow(clippy::too_many_arguments)]
    pub fn rehydrate(
        id: OrderId,
        position: Position,
        initial_price: Price,
        leverage: Leverage,
        settlement_interval: Duration,
        quantity: Usd,
        counterparty_network_identity: Identity,
        role: Role,
        events: Vec<Event>,
    ) -> Self {
        let cfd = Self::new(
            id,
            position,
            initial_price,
            leverage,
            settlement_interval,
            role,
            quantity,
            counterparty_network_identity,
        );
        events.into_iter().fold(cfd, Cfd::apply)
    }

    fn expiry_timestamp(&self) -> Option<OffsetDateTime> {
        self.dlc
            .as_ref()
            .map(|dlc| dlc.settlement_event_id.timestamp)
    }

    /// Only cfds in state `Open` that have not received an attestation and are within 23 hours
    /// until expiry are eligible for rollover
    pub fn is_rollover_possible(&self, now: OffsetDateTime) -> Result<(), CannotRollover> {
        if self.is_final() {
            return Err(CannotRollover::WrongState {
                state: "final".to_owned(),
            });
        }

        if self.commit_tx.is_some() {
            return Err(CannotRollover::WrongState {
                state: "committed".to_owned(),
            });
        }

        let expiry_timestamp = self.expiry_timestamp().ok_or(CannotRollover::NoDlc)?;

        if now > expiry_timestamp {
            return Err(CannotRollover::AlreadyExpired);
        }

        let time_until_expiry = expiry_timestamp - now;

        if time_until_expiry > SETTLEMENT_INTERVAL - Duration::HOUR {
            return Err(CannotRollover::WasJustRolledOver);
        }

        // only state open with no attestation is acceptable for rollover
        // TODO: Rewrite it terms of events
        // if !matches!(
        //     self.state(),
        //     CfdState::Open {
        //         attestation: None,
        //         ..
        //     }
        // ) {
        //     // TODO: how to derive state for these messages?
        //     return Err(CannotRollover::WrongState {
        //         state: "Insert state here (how do to it?)".into(),
        //     });
        // }

        Ok(())
    }

    fn can_roll_over(&self) -> bool {
        self.lock_finality && !self.commit_finality && !self.is_final() && !self.is_attested()
    }

    fn can_settle_collaboratively(&self) -> bool {
        self.lock_finality && !self.commit_finality && !self.is_final() && !self.is_attested()
    }

    fn is_attested(&self) -> bool {
        self.cet.is_some()
    }

    fn is_final(&self) -> bool {
        self.collaborative_settlement_finality || self.cet_finality || self.refund_finality
    }

    pub fn start_contract_setup(&self) -> Result<(SetupParams, Identity)> {
        if self.version > 0 {
            bail!("Start contract not allowed in version {}", self.version)
        }

        let margin = match self.position {
            Position::Long => {
                calculate_long_margin(self.initial_price, self.quantity, self.leverage)
            }
            Position::Short => calculate_short_margin(self.initial_price, self.quantity),
        };

        let counterparty_margin = match self.position {
            Position::Long => calculate_short_margin(self.initial_price, self.quantity),
            Position::Short => {
                calculate_long_margin(self.initial_price, self.quantity, self.leverage)
            }
        };

        Ok((
            SetupParams::new(
                margin,
                counterparty_margin,
                self.initial_price,
                self.quantity,
                self.leverage,
                self.refund_timelock_in_blocks(),
                1, // TODO: Where should I get the fee rate from?
            ),
            self.counterparty_network_identity,
        ))
    }

    pub fn start_rollover(&self) -> Result<(RolloverParams, Dlc, Duration)> {
        if !self.can_roll_over() {
            bail!("Start rollover only allowed when open")
        }

        Ok((
            RolloverParams::new(
                self.initial_price,
                self.quantity,
                self.leverage,
                self.refund_timelock_in_blocks(),
                1, // TODO: Where should I get the fee rate from?
            ),
            self.dlc
                .as_ref()
                .context("dlc has to be available for rollover")?
                .clone(),
            self.settlement_interval,
        ))
    }

    pub fn start_collaborative_settlement_maker(
        &self,
        proposal: SettlementProposal,
        sig_taker: Signature,
    ) -> Result<CollaborativeSettlement> {
        let dlc = self
            .dlc
            .as_ref()
            .context("dlc has to be available for collab settlemment")?
            .clone();

        let (tx, sig_maker) = dlc.close_transaction(&proposal)?;
        let spend_tx = dlc.finalize_spend_transaction((tx, sig_maker), sig_taker)?;
        let script_pk = dlc.script_pubkey_for(Role::Maker);

        let settlement = CollaborativeSettlement::new(spend_tx, script_pk, proposal.price)?;
        Ok(settlement)
    }

    pub fn start_collaborative_settlement_taker(
        &self,
        current_price: Price,
        n_payouts: usize,
    ) -> Result<SettlementProposal> {
        if !self.can_settle_collaboratively() {
            bail!("Start collaborative settlement only allowed when open")
        }

        let payout_curve = payout_curve::calculate(
            // TODO: Is this correct? Does rollover change the price? (I think currently not)
            self.initial_price,
            self.quantity,
            self.leverage,
            n_payouts,
        )?;

        let payout = {
            let current_price = current_price.try_into_u64()?;
            payout_curve
                .iter()
                .find(|&x| x.digits().range().contains(&current_price))
                .context("find current price on the payout curve")?
        };

        let settlement_proposal = SettlementProposal {
            order_id: self.id,
            timestamp: Timestamp::now(),
            taker: *payout.taker_amount(),
            maker: *payout.maker_amount(),
            price: current_price,
        };

        Ok(settlement_proposal)
    }

    pub fn setup_contract(self, completed: SetupCompleted) -> Result<Event> {
        if self.version > 0 {
            bail!(
                "Complete contract setup not allowed because cfd already in version {}",
                self.version
            )
        }

        let event = match completed {
            SetupCompleted::Succeeded {
                payload: (dlc, _), ..
            } => CfdEvent::ContractSetupCompleted { dlc },
            SetupCompleted::Rejected { .. } => CfdEvent::OfferRejected,
            SetupCompleted::Failed { error, .. } => {
                tracing::error!("Contract setup failed: {:#}", error);

                CfdEvent::ContractSetupFailed
            }
        };

        Ok(self.event(event))
    }

    // TODO: Pass the entire enum
    pub fn roll_over(self, rollover_result: Result<Dlc>) -> Result<Event> {
        // TODO: Compare that the version that we started the rollover with is the same as the
        // version now. For that to work we should pass the version into the state machine
        // that will handle rollover and the pass it back in here for comparison.
        if !self.can_roll_over() {
            bail!("Complete rollover only allowed when open")
        }

        let event = match rollover_result {
            Ok(dlc) => CfdEvent::RolloverCompleted { dlc },
            Err(err) => {
                tracing::error!("Rollover failed: {:#}", err);

                CfdEvent::RolloverFailed
            }
        };

        Ok(self.event(event))
    }

    pub fn settle_collaboratively(
        mut self,
        settlement: CollaborativeSettlementCompleted,
    ) -> Result<Event> {
        if !self.can_settle_collaboratively() {
            bail!("Cannot collaboratively settle anymore")
        }

        let event = match settlement {
            Completed::Succeeded {
                payload: settlement,
                ..
            } => CfdEvent::CollaborativeSettlementCompleted {
                spend_tx: settlement.tx,
                script: settlement.script_pubkey,
                price: settlement.price,
            },
            Completed::Rejected { reason, .. } => {
                tracing::info!(order_id=%self.id(), "Collaborative close rejected: {:#}", reason);

                let dlc = self
                    .dlc
                    .take()
                    .context("No dlc after collaborative settlement rejected")?;
                let commit_tx = dlc.signed_commit_tx()?;

                CfdEvent::CollaborativeSettlementRejected { commit_tx }
            }
            Completed::Failed { error, .. } => {
                tracing::warn!(order_id=%self.id(), "Collaborative close failed: {:#}", error);

                let dlc = self
                    .dlc
                    .take()
                    .context("No dlc after collaborative settlement rejected")?;
                let commit_tx = dlc.signed_commit_tx()?;

                CfdEvent::CollaborativeSettlementFailed { commit_tx }
            }
        };

        Ok(self.event(event))
    }

    /// Given an attestation, find and decrypt the relevant CET.
    pub fn decrypt_cet(self, attestation: &oracle::Attestation) -> Result<Option<Event>> {
        anyhow::ensure!(!self.is_final());

        let dlc = match self.dlc.as_ref() {
            Some(dlc) => dlc,
            None => {
                tracing::warn!(order_id = %self.id(), "Handling attestation without a DLC is a no-op");
                return Ok(None);
            }
        };

        let cet = match dlc.signed_cet(attestation)? {
            Ok(cet) => cet,
            Err(e @ IrrelevantAttestation { .. }) => {
                tracing::debug!("{}", e);
                return Ok(None);
            }
        };

        let price = Price(Decimal::from(attestation.price));

        if self.cet_timelock_expired {
            return Ok(Some(
                self.event(CfdEvent::OracleAttestedPostCetTimelock { cet, price }),
            ));
        }

        Ok(Some(self.event(CfdEvent::OracleAttestedPriorCetTimelock {
            timelocked_cet: cet,
            commit_tx: dlc.signed_commit_tx()?,
            price,
        })))
    }

    pub fn handle_cet_timelock_expired(mut self) -> Result<Event> {
        anyhow::ensure!(!self.is_final());

        let cfd_event = self
            .cet
            .take()
            // If we have cet, that means it has been attested
            .map(|cet| CfdEvent::CetTimelockConfirmedPostOracleAttestation { cet })
            .unwrap_or_else(|| CfdEvent::CetTimelockConfirmedPriorOracleAttestation);

        Ok(self.event(cfd_event))
    }

    pub fn handle_refund_timelock_expired(self) -> Event {
        todo!()
    }

    pub fn handle_lock_confirmed(self) -> Event {
        self.event(CfdEvent::LockConfirmed)
    }

    pub fn handle_commit_confirmed(self) -> Event {
        self.event(CfdEvent::CommitConfirmed)
    }

    pub fn handle_collaborative_settlement_confirmed(self) -> Event {
        self.event(CfdEvent::CollaborativeSettlementConfirmed)
    }

    pub fn handle_cet_confirmed(self) -> Event {
        self.event(CfdEvent::CetConfirmed)
    }

    pub fn handle_refund_confirmed(self) -> Event {
        self.event(CfdEvent::RefundConfirmed)
    }

    pub fn handle_revoke_confirmed(self) -> Event {
        self.event(CfdEvent::RevokeConfirmed)
    }

    pub fn manual_commit_to_blockchain(&self) -> Result<Event> {
        anyhow::ensure!(!self.is_final());

        let dlc = self.dlc.as_ref().context("Cannot commit without a DLC")?;

        Ok(self.event(CfdEvent::ManualCommit {
            tx: dlc.signed_commit_tx()?,
        }))
    }

    fn event(&self, event: CfdEvent) -> Event {
        Event::new(self.id, event)
    }

    /// A factor to be added to the CFD order settlement_interval for calculating the
    /// refund timelock.
    ///
    /// The refund timelock is important in case the oracle disappears or never publishes a
    /// signature. Ideally, both users collaboratively settle in the refund scenario. This
    /// factor is important if the users do not settle collaboratively.
    /// `1.5` times the settlement_interval as defined in CFD order should be safe in the
    /// extreme case where a user publishes the commit transaction right after the contract was
    /// initialized. In this case, the oracle still has `1.0 *
    /// cfdorder.settlement_interval` time to attest and no one can publish the refund
    /// transaction.
    /// The downside is that if the oracle disappears: the users would only notice at the end
    /// of the cfd settlement_interval. In this case the users has to wait for another
    /// `1.5` times of the settlement_interval to get his funds back.
    const REFUND_THRESHOLD: f32 = 1.5;

    fn refund_timelock_in_blocks(&self) -> u32 {
        (self.settlement_interval * Self::REFUND_THRESHOLD)
            .as_blocks()
            .ceil() as u32
    }

    pub fn id(&self) -> OrderId {
        self.id
    }

    pub fn position(&self) -> Position {
        self.position
    }

    pub fn initial_price(&self) -> Price {
        self.initial_price
    }

    pub fn leverage(&self) -> Leverage {
        self.leverage
    }

    pub fn settlement_time_interval_hours(&self) -> Duration {
        self.settlement_interval
    }

    pub fn quantity(&self) -> Usd {
        self.quantity
    }

    pub fn counterparty_network_identity(&self) -> Identity {
        self.counterparty_network_identity
    }

    pub fn role(&self) -> Role {
        self.role
    }

    pub fn sign_collaborative_close_transaction_taker(
        &mut self,
        proposal: &SettlementProposal,
    ) -> Result<(Transaction, Signature, Script)> {
        let dlc = self.dlc.take().context("Collaborative close without DLC")?;

        let (tx, sig) = dlc.close_transaction(proposal)?;
        let script_pk = dlc.script_pubkey_for(Role::Taker);

        Ok((tx, sig, script_pk))
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn apply(mut self, evt: Event) -> Cfd {
        use CfdEvent::*;

        self.version += 1;

        match evt.event {
            ContractSetupCompleted { dlc } => self.dlc = Some(dlc),
            OracleAttestedPostCetTimelock { cet, .. } => self.cet = Some(cet),
            OracleAttestedPriorCetTimelock { timelocked_cet, .. } => {
                self.cet = Some(timelocked_cet);
            }
            ContractSetupFailed { .. } => {
                // TODO: Deal with failed contract setup
            }
            RolloverCompleted { dlc } => {
                self.dlc = Some(dlc);
            }
            RolloverFailed { .. } => todo!(),
            RolloverRejected => todo!(),
            CollaborativeSettlementCompleted { spend_tx, .. } => {
                self.collaborative_settlement_spend_tx = Some(spend_tx)
            }
            CollaborativeSettlementRejected { commit_tx } => self.commit_tx = Some(commit_tx),
            CollaborativeSettlementFailed { commit_tx } => self.commit_tx = Some(commit_tx),

            CetConfirmed => self.cet_finality = true,
            RefundConfirmed => self.refund_finality = true,
            CollaborativeSettlementConfirmed => self.collaborative_settlement_finality = true,
            RefundTimelockConfirmed { .. } => self.refund_timelock_expired = true,
            LockConfirmed => self.lock_finality = true,
            CommitConfirmed => self.commit_finality = true,
            CetTimelockConfirmedPriorOracleAttestation
            | CetTimelockConfirmedPostOracleAttestation { .. } => {
                self.cet_timelock_expired = true;
            }
            OfferRejected => {
                // nothing to do here? A rejection means it should be impossible to issue any
                // commands
            }
            ManualCommit { tx } => self.commit_tx = Some(tx),
            RevokeConfirmed => todo!("Deal with revoke"),
        }

        self
    }
}

pub trait AsBlocks {
    /// Calculates the duration in Bitcoin blocks.
    ///
    /// On Bitcoin there is a block every 10 minutes/600 seconds on average.
    /// It's the caller's responsibility to round the resulting floating point number.
    fn as_blocks(&self) -> f32;
}

impl AsBlocks for Duration {
    fn as_blocks(&self) -> f32 {
        self.as_seconds_f32() / 60.0 / 10.0
    }
}

// Make a `Margin` newtype and call `Margin::long`
/// Calculates the long's margin in BTC
///
/// The margin is the initial margin and represents the collateral the buyer
/// has to come up with to satisfy the contract. Here we calculate the initial
/// long margin as: quantity / (initial_price * leverage)
pub fn calculate_long_margin(price: Price, quantity: Usd, leverage: Leverage) -> Amount {
    quantity / (price * leverage)
}

/// Calculates the shorts's margin in BTC
///
/// The short margin is represented as the quantity of the contract given the
/// initial price. The short side can currently not leverage the position but
/// always has to cover the complete quantity.
pub fn calculate_short_margin(price: Price, quantity: Usd) -> Amount {
    quantity / price
}

pub fn calculate_long_liquidation_price(leverage: Leverage, price: Price) -> Price {
    price * leverage / (leverage + 1)
}

/// Returns the Profit/Loss (P/L) as Bitcoin. Losses are capped by the provided margin
pub fn calculate_profit(
    initial_price: Price,
    closing_price: Price,
    quantity: Usd,
    leverage: Leverage,
    position: Position,
) -> Result<(SignedAmount, Percent)> {
    let inv_initial_price =
        InversePrice::new(initial_price).context("cannot invert invalid price")?;
    let inv_closing_price =
        InversePrice::new(closing_price).context("cannot invert invalid price")?;
    let long_liquidation_price = calculate_long_liquidation_price(leverage, initial_price);
    let long_is_liquidated = closing_price <= long_liquidation_price;

    let long_margin = calculate_long_margin(initial_price, quantity, leverage)
        .to_signed()
        .context("Unable to compute long margin")?;
    let short_margin = calculate_short_margin(initial_price, quantity)
        .to_signed()
        .context("Unable to compute short margin")?;
    let amount_changed = (quantity * inv_initial_price)
        .to_signed()
        .context("Unable to convert to SignedAmount")?
        - (quantity * inv_closing_price)
            .to_signed()
            .context("Unable to convert to SignedAmount")?;

    // calculate profit/loss (P and L) in BTC
    let (margin, payout) = match position {
        // TODO:
        // At this point, long_leverage == leverage, short_leverage == 1
        // which has the effect that the right boundary `b` below is
        // infinite and not used.
        //
        // The general case is:
        //   let:
        //     P = payout
        //     Q = quantity
        //     Ll = long_leverage
        //     Ls = short_leverage
        //     xi = initial_price
        //     xc = closing_price
        //
        //     a = xi * Ll / (Ll + 1)
        //     b = xi * Ls / (Ls - 1)
        //
        //     P_long(xc) = {
        //          0 if xc <= a,
        //          Q / (xi * Ll) + Q * (1 / xi - 1 / xc) if a < xc < b,
        //          Q / xi * (1/Ll + 1/Ls) if xc if xc >= b
        //     }
        //
        //     P_short(xc) = {
        //          Q / xi * (1/Ll + 1/Ls) if xc <= a,
        //          Q / (xi * Ls) - Q * (1 / xi - 1 / xc) if a < xc < b,
        //          0 if xc >= b
        //     }
        Position::Long => {
            let payout = match long_is_liquidated {
                true => SignedAmount::ZERO,
                false => long_margin + amount_changed,
            };
            (long_margin, payout)
        }
        Position::Short => {
            let payout = match long_is_liquidated {
                true => long_margin + short_margin,
                false => short_margin - amount_changed,
            };
            (short_margin, payout)
        }
    };

    let profit = payout - margin;
    let percent = Decimal::from_f64(100. * profit.as_sat() as f64 / margin.as_sat() as f64)
        .context("Unable to compute percent")?;

    Ok((profit, Percent(percent)))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Cet {
    pub tx: Transaction,
    pub adaptor_sig: EcdsaAdaptorSignature,

    // TODO: Range + number of digits (usize) could be represented as Digits similar to what we do
    // in the protocol lib
    pub range: RangeInclusive<u64>,
    pub n_bits: usize,
}

/// Contains all data we've assembled about the CFD through the setup protocol.
///
/// All contained signatures are the signatures of THE OTHER PARTY.
/// To use any of these transactions, we need to re-sign them with the correct secret key.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Dlc {
    pub identity: SecretKey,
    pub identity_counterparty: PublicKey,
    pub revocation: SecretKey,
    pub revocation_pk_counterparty: PublicKey,
    pub publish: SecretKey,
    pub publish_pk_counterparty: PublicKey,
    pub maker_address: Address,
    pub taker_address: Address,

    /// The fully signed lock transaction ready to be published on chain
    pub lock: (Transaction, Descriptor<PublicKey>),
    pub commit: (Transaction, EcdsaAdaptorSignature, Descriptor<PublicKey>),
    pub cets: HashMap<BitMexPriceEventId, Vec<Cet>>,
    pub refund: (Transaction, Signature),

    #[serde(with = "::bdk::bitcoin::util::amount::serde::as_sat")]
    pub maker_lock_amount: Amount,
    #[serde(with = "::bdk::bitcoin::util::amount::serde::as_sat")]
    pub taker_lock_amount: Amount,

    pub revoked_commit: Vec<RevokedCommit>,

    // TODO: For now we store this seperately - it is a duplicate of what is stored in the cets
    // hashmap. The cet hashmap allows storing cets for event-ids with different concern
    // (settlement and liquidation-point). We should NOT make these fields public on the Dlc
    // and create an internal structure that depicts this properly and avoids duplication.
    pub settlement_event_id: BitMexPriceEventId,
    pub refund_timelock: u32,
}

impl Dlc {
    /// Create a close transaction based on the current contract and a settlement proposals
    pub fn close_transaction(
        &self,
        proposal: &crate::model::cfd::SettlementProposal,
    ) -> Result<(Transaction, Signature)> {
        let (lock_tx, lock_desc) = &self.lock;
        let (lock_outpoint, lock_amount) = {
            let outpoint = lock_tx
                .outpoint(&lock_desc.script_pubkey())
                .expect("lock script to be in lock tx");
            let amount = Amount::from_sat(lock_tx.output[outpoint.vout as usize].value);

            (outpoint, amount)
        };
        let (tx, sighash) = maia::close_transaction(
            lock_desc,
            lock_outpoint,
            lock_amount,
            (&self.maker_address, proposal.maker),
            (&self.taker_address, proposal.taker),
        )
        .context("Unable to collaborative close transaction")?;

        let sig = SECP256K1.sign(&sighash, &self.identity);

        Ok((tx, sig))
    }

    pub fn finalize_spend_transaction(
        &self,
        (close_tx, own_sig): (Transaction, Signature),
        counterparty_sig: Signature,
    ) -> Result<Transaction> {
        let own_pk = PublicKey::new(secp256k1_zkp::PublicKey::from_secret_key(
            SECP256K1,
            &self.identity,
        ));

        let (_, lock_desc) = &self.lock;
        let spend_tx = maia::finalize_spend_transaction(
            close_tx,
            lock_desc,
            (own_pk, own_sig),
            (self.identity_counterparty, counterparty_sig),
        )?;

        Ok(spend_tx)
    }

    pub fn refund_amount(&self, role: Role) -> Amount {
        let our_script_pubkey = match role {
            Role::Taker => self.taker_address.script_pubkey(),
            Role::Maker => self.maker_address.script_pubkey(),
        };

        self.refund
            .0
            .output
            .iter()
            .find(|output| output.script_pubkey == our_script_pubkey)
            .map(|output| Amount::from_sat(output.value))
            .unwrap_or_default()
    }

    pub fn script_pubkey_for(&self, role: Role) -> Script {
        match role {
            Role::Maker => self.maker_address.script_pubkey(),
            Role::Taker => self.taker_address.script_pubkey(),
        }
    }

    pub fn signed_refund_tx(&self) -> Result<Transaction> {
        let sig_hash = spending_tx_sighash(
            &self.refund.0,
            &self.commit.2,
            Amount::from_sat(self.commit.0.output[0].value),
        );
        let our_sig = SECP256K1.sign(&sig_hash, &self.identity);
        let our_pubkey = PublicKey::new(bdk::bitcoin::secp256k1::PublicKey::from_secret_key(
            SECP256K1,
            &self.identity,
        ));
        let counterparty_sig = self.refund.1;
        let counterparty_pubkey = self.identity_counterparty;
        let signed_refund_tx = finalize_spend_transaction(
            self.refund.0.clone(),
            &self.commit.2,
            (our_pubkey, our_sig),
            (counterparty_pubkey, counterparty_sig),
        )?;

        Ok(signed_refund_tx)
    }

    pub fn signed_commit_tx(&self) -> Result<Transaction> {
        let sig_hash = spending_tx_sighash(
            &self.commit.0,
            &self.lock.1,
            Amount::from_sat(self.lock.0.output[0].value),
        );
        let our_sig = SECP256K1.sign(&sig_hash, &self.identity);
        let our_pubkey = PublicKey::new(bdk::bitcoin::secp256k1::PublicKey::from_secret_key(
            SECP256K1,
            &self.identity,
        ));

        let counterparty_sig = self.commit.1.decrypt(&self.publish)?;
        let counterparty_pubkey = self.identity_counterparty;

        let signed_commit_tx = finalize_spend_transaction(
            self.commit.0.clone(),
            &self.lock.1,
            (our_pubkey, our_sig),
            (counterparty_pubkey, counterparty_sig),
        )?;

        Ok(signed_commit_tx)
    }

    pub fn signed_cet(
        &self,
        attestation: &oracle::Attestation,
    ) -> Result<Result<Transaction, IrrelevantAttestation>> {
        let cets = match self.cets.get(&attestation.id) {
            Some(cets) => cets,
            None => {
                return Ok(Err(IrrelevantAttestation {
                    id: attestation.id,
                    tx_id: self.lock.0.txid(),
                }))
            }
        };

        let Cet {
            tx: cet,
            adaptor_sig: encsig,
            n_bits,
            ..
        } = cets
            .iter()
            .find(|Cet { range, .. }| range.contains(&attestation.price))
            .context("Price out of range of cets")?;

        let mut decryption_sk = attestation.scalars[0];
        for oracle_attestation in attestation.scalars[1..*n_bits].iter() {
            decryption_sk.add_assign(oracle_attestation.as_ref())?;
        }

        let sig_hash = spending_tx_sighash(
            cet,
            &self.commit.2,
            Amount::from_sat(self.commit.0.output[0].value),
        );
        let our_sig = SECP256K1.sign(&sig_hash, &self.identity);
        let our_pubkey = PublicKey::new(bdk::bitcoin::secp256k1::PublicKey::from_secret_key(
            SECP256K1,
            &self.identity,
        ));

        let counterparty_sig = encsig.decrypt(&decryption_sk)?;
        let counterparty_pubkey = self.identity_counterparty;

        let signed_cet = finalize_spend_transaction(
            cet.clone(),
            &self.commit.2,
            (our_pubkey, our_sig),
            (counterparty_pubkey, counterparty_sig),
        )?;

        Ok(Ok(signed_cet))
    }
}

#[derive(Debug, thiserror::Error)]
#[error("Attestation {id} is irrelevant for DLC {tx_id}")]
pub struct IrrelevantAttestation {
    id: BitMexPriceEventId,
    tx_id: Txid,
}

/// Information which we need to remember in order to construct a
/// punishment transaction in case the counterparty publishes a
/// revoked commit transaction.
///
/// It also includes the information needed to monitor for the
/// publication of the revoked commit transaction.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RevokedCommit {
    // To build punish transaction
    pub encsig_ours: EcdsaAdaptorSignature,
    pub revocation_sk_theirs: SecretKey,
    pub publication_pk_theirs: PublicKey,
    // To monitor revoked commit transaction
    pub txid: Txid,
    pub script_pubkey: Script,
}

/// Used when transactions (e.g. collaborative close) are recorded as a part of
/// CfdState in the cases when we can't solely rely on state transition
/// timestamp as it could have occured for different reasons (like a new
/// attestation in Open state)
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CollaborativeSettlement {
    pub tx: Transaction,
    pub script_pubkey: Script,
    pub timestamp: Timestamp,
    #[serde(with = "::bdk::bitcoin::util::amount::serde::as_sat")]
    payout: Amount,
    price: Price,
}

impl CollaborativeSettlement {
    pub fn new(tx: Transaction, own_script_pubkey: Script, price: Price) -> Result<Self> {
        // Falls back to Amount::ZERO in case we don't find an output that matches out script pubkey
        // The assumption is, that this can happen for cases where we were liquidated
        let payout = match tx
            .output
            .iter()
            .find(|output| output.script_pubkey == own_script_pubkey)
            .map(|output| Amount::from_sat(output.value))
        {
            Some(payout) => payout,
            None => {
                tracing::error!(
                    "Collaborative settlement with a zero amount, this should really not happen!"
                );
                Amount::ZERO
            }
        };

        Ok(Self {
            tx,
            script_pubkey: own_script_pubkey,
            timestamp: Timestamp::now(),
            payout,
            price,
        })
    }

    pub fn payout(&self) -> Amount {
        self.payout
    }
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Completed<P> {
    Succeeded {
        order_id: OrderId,
        payload: P,
    },
    Rejected {
        order_id: OrderId,
        reason: anyhow::Error,
    },
    Failed {
        order_id: OrderId,
        error: anyhow::Error,
    },
}

impl<P> xtra::Message for Completed<P>
where
    P: Send + 'static,
{
    type Result = Result<()>;
}

impl<P> Completed<P> {
    pub fn order_id(&self) -> OrderId {
        *match self {
            Completed::Succeeded { order_id, .. } => order_id,
            Completed::Rejected { order_id, .. } => order_id,
            Completed::Failed { order_id, .. } => order_id,
        }
    }

    pub fn rejected(order_id: OrderId) -> Self {
        Self::Rejected {
            order_id,
            reason: anyhow::format_err!("unknown"),
        }
    }
    pub fn rejected_due_to(order_id: OrderId, reason: anyhow::Error) -> Self {
        Self::Rejected { order_id, reason }
    }
}

pub mod marker {
    /// Marker type for contract setup completion
    #[derive(Debug)]
    pub struct Setup;
    /// Marker type for rollover  completion
    #[derive(Debug)]
    pub struct Rollover;
}

/// Message sent from a setup actor to the
/// cfd actor to notify that the contract setup has finished.
pub type SetupCompleted = Completed<(Dlc, marker::Setup)>;

/// Message sent from a rollover actor to the
/// cfd actor to notify that the rollover has finished (contract got updated).
/// TODO: Roll it out in the maker rollover actor
pub type RolloverCompleted = Completed<(Dlc, marker::Rollover)>;

pub type CollaborativeSettlementCompleted = Completed<CollaborativeSettlement>;

impl Completed<(Dlc, marker::Setup)> {
    pub fn succeeded(order_id: OrderId, dlc: Dlc) -> Self {
        Self::Succeeded {
            order_id,
            payload: (dlc, marker::Setup),
        }
    }
}

impl Completed<(Dlc, marker::Rollover)> {
    pub fn succeeded(order_id: OrderId, dlc: Dlc) -> Self {
        Self::Succeeded {
            order_id,
            payload: (dlc, marker::Rollover),
        }
    }
}

mod hex_transaction {
    use super::*;
    use bdk::bitcoin;
    use serde::Deserializer;
    use serde::Serializer;

    pub fn serialize<S: Serializer>(value: &Transaction, serializer: S) -> Result<S::Ok, S::Error> {
        let bytes = bitcoin::consensus::serialize(value);
        let hex_str = hex::encode(bytes);
        serializer.serialize_str(hex_str.as_str())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Transaction, D::Error>
    where
        D: Deserializer<'de>,
    {
        let hex = String::deserialize(deserializer).map_err(D::Error::custom)?;
        let bytes = hex::decode(hex).map_err(D::Error::custom)?;
        let tx = bitcoin::consensus::deserialize(&bytes).map_err(D::Error::custom)?;
        Ok(tx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn given_default_values_then_expected_liquidation_price() {
        let price = Price::new(dec!(46125)).unwrap();
        let leverage = Leverage::new(5).unwrap();
        let expected = Price::new(dec!(38437.5)).unwrap();

        let liquidation_price = calculate_long_liquidation_price(leverage, price);

        assert_eq!(liquidation_price, expected);
    }

    #[test]
    fn given_leverage_of_one_and_equal_price_and_quantity_then_long_margin_is_one_btc() {
        let price = Price::new(dec!(40000)).unwrap();
        let quantity = Usd::new(dec!(40000));
        let leverage = Leverage::new(1).unwrap();

        let long_margin = calculate_long_margin(price, quantity, leverage);

        assert_eq!(long_margin, Amount::ONE_BTC);
    }

    #[test]
    fn given_leverage_of_one_and_leverage_of_ten_then_long_margin_is_lower_factor_ten() {
        let price = Price::new(dec!(40000)).unwrap();
        let quantity = Usd::new(dec!(40000));
        let leverage = Leverage::new(10).unwrap();

        let long_margin = calculate_long_margin(price, quantity, leverage);

        assert_eq!(long_margin, Amount::from_btc(0.1).unwrap());
    }

    #[test]
    fn given_quantity_equals_price_then_short_margin_is_one_btc() {
        let price = Price::new(dec!(40000)).unwrap();
        let quantity = Usd::new(dec!(40000));

        let short_margin = calculate_short_margin(price, quantity);

        assert_eq!(short_margin, Amount::ONE_BTC);
    }

    #[test]
    fn given_quantity_half_of_price_then_short_margin_is_half_btc() {
        let price = Price::new(dec!(40000)).unwrap();
        let quantity = Usd::new(dec!(20000));

        let short_margin = calculate_short_margin(price, quantity);

        assert_eq!(short_margin, Amount::from_btc(0.5).unwrap());
    }

    #[test]
    fn given_quantity_double_of_price_then_short_margin_is_two_btc() {
        let price = Price::new(dec!(40000)).unwrap();
        let quantity = Usd::new(dec!(80000));

        let short_margin = calculate_short_margin(price, quantity);

        assert_eq!(short_margin, Amount::from_btc(2.0).unwrap());
    }

    #[test]
    fn test_secs_into_blocks() {
        let error_margin = f32::EPSILON;

        let duration = Duration::seconds(600);
        let blocks = duration.as_blocks();
        assert!(blocks - error_margin < 1.0 && blocks + error_margin > 1.0);

        let duration = Duration::seconds(0);
        let blocks = duration.as_blocks();
        assert!(blocks - error_margin < 0.0 && blocks + error_margin > 0.0);

        let duration = Duration::seconds(60);
        let blocks = duration.as_blocks();
        assert!(blocks - error_margin < 0.1 && blocks + error_margin > 0.1);
    }

    #[test]
    fn calculate_profit_and_loss() {
        assert_profit_loss_values(
            Price::new(dec!(10_000)).unwrap(),
            Price::new(dec!(10_000)).unwrap(),
            Usd::new(dec!(10_000)),
            Leverage::new(2).unwrap(),
            Position::Long,
            SignedAmount::ZERO,
            Decimal::ZERO.into(),
            "No price increase means no profit",
        );

        assert_profit_loss_values(
            Price::new(dec!(10_000)).unwrap(),
            Price::new(dec!(20_000)).unwrap(),
            Usd::new(dec!(10_000)),
            Leverage::new(2).unwrap(),
            Position::Long,
            SignedAmount::from_sat(50_000_000),
            dec!(100).into(),
            "A price increase of 2x should result in a profit of 100% (long)",
        );

        assert_profit_loss_values(
            Price::new(dec!(9_000)).unwrap(),
            Price::new(dec!(6_000)).unwrap(),
            Usd::new(dec!(9_000)),
            Leverage::new(2).unwrap(),
            Position::Long,
            SignedAmount::from_sat(-50_000_000),
            dec!(-100).into(),
            "A price drop of 1/(Leverage + 1) x should result in 100% loss (long)",
        );

        assert_profit_loss_values(
            Price::new(dec!(10_000)).unwrap(),
            Price::new(dec!(5_000)).unwrap(),
            Usd::new(dec!(10_000)),
            Leverage::new(2).unwrap(),
            Position::Long,
            SignedAmount::from_sat(-50_000_000),
            dec!(-100).into(),
            "A loss should be capped at 100% (long)",
        );

        assert_profit_loss_values(
            Price::new(dec!(50_400)).unwrap(),
            Price::new(dec!(60_000)).unwrap(),
            Usd::new(dec!(10_000)),
            Leverage::new(2).unwrap(),
            Position::Long,
            SignedAmount::from_sat(3_174_603),
            dec!(31.99999798400001).into(),
            "long position should make a profit when price goes up",
        );

        assert_profit_loss_values(
            Price::new(dec!(50_400)).unwrap(),
            Price::new(dec!(60_000)).unwrap(),
            Usd::new(dec!(10_000)),
            Leverage::new(2).unwrap(),
            Position::Short,
            SignedAmount::from_sat(-3_174_603),
            dec!(-15.99999899200001).into(),
            "short position should make a loss when price goes up",
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn assert_profit_loss_values(
        initial_price: Price,
        current_price: Price,
        quantity: Usd,
        leverage: Leverage,
        position: Position,
        should_profit: SignedAmount,
        should_profit_in_percent: Percent,
        msg: &str,
    ) {
        let (profit, in_percent) =
            calculate_profit(initial_price, current_price, quantity, leverage, position).unwrap();

        assert_eq!(profit, should_profit, "{}", msg);
        assert_eq!(in_percent, should_profit_in_percent, "{}", msg);
    }

    #[test]
    fn test_profit_calculation_loss_plus_profit_should_be_zero() {
        let initial_price = Price::new(dec!(10_000)).unwrap();
        let closing_price = Price::new(dec!(16_000)).unwrap();
        let quantity = Usd::new(dec!(10_000));
        let leverage = Leverage::new(1).unwrap();
        let (profit, profit_in_percent) = calculate_profit(
            initial_price,
            closing_price,
            quantity,
            leverage,
            Position::Long,
        )
        .unwrap();
        let (loss, loss_in_percent) = calculate_profit(
            initial_price,
            closing_price,
            quantity,
            leverage,
            Position::Short,
        )
        .unwrap();

        assert_eq!(profit.checked_add(loss).unwrap(), SignedAmount::ZERO);
        // NOTE:
        // this is only true when long_leverage == short_leverage
        assert_eq!(
            profit_in_percent.0.checked_add(loss_in_percent.0).unwrap(),
            Decimal::ZERO
        );
    }

    #[test]
    fn margin_remains_constant() {
        let initial_price = Price::new(dec!(15_000)).unwrap();
        let quantity = Usd::new(dec!(10_000));
        let leverage = Leverage::new(2).unwrap();
        let long_margin = calculate_long_margin(initial_price, quantity, leverage)
            .to_signed()
            .unwrap();
        let short_margin = calculate_short_margin(initial_price, quantity)
            .to_signed()
            .unwrap();
        let pool_amount = SignedAmount::ONE_BTC;
        let closing_prices = [
            Price::new(dec!(0.15)).unwrap(),
            Price::new(dec!(1.5)).unwrap(),
            Price::new(dec!(15)).unwrap(),
            Price::new(dec!(150)).unwrap(),
            Price::new(dec!(1_500)).unwrap(),
            Price::new(dec!(15_000)).unwrap(),
            Price::new(dec!(150_000)).unwrap(),
            Price::new(dec!(1_500_000)).unwrap(),
            Price::new(dec!(15_000_000)).unwrap(),
        ];

        for price in closing_prices {
            let (long_profit, _) =
                calculate_profit(initial_price, price, quantity, leverage, Position::Long).unwrap();
            let (short_profit, _) =
                calculate_profit(initial_price, price, quantity, leverage, Position::Short)
                    .unwrap();

            assert_eq!(
                long_profit + long_margin + short_profit + short_margin,
                pool_amount
            );
        }
    }

    #[test]
    fn order_id_serde_roundtrip() {
        let id = OrderId::default();

        let deserialized = serde_json::from_str(&serde_json::to_string(&id).unwrap()).unwrap();

        assert_eq!(id, deserialized);
    }

    #[test]
    fn cfd_event_to_json() {
        let event = CfdEvent::ContractSetupFailed;

        let (name, data) = event.to_json();

        assert_eq!(name, "ContractSetupFailed");
        assert_eq!(data, r#"null"#);
    }

    #[test]
    fn cfd_event_from_json() {
        let name = "ContractSetupFailed".to_owned();
        let data = r#"null"#.to_owned();

        let event = CfdEvent::from_json(name, data).unwrap();

        assert_eq!(event, CfdEvent::ContractSetupFailed);
    }

    #[test]
    fn cfd_event_no_data_from_json() {
        let name = "OfferRejected".to_owned();
        let data = r#"null"#.to_owned();

        let event = CfdEvent::from_json(name, data).unwrap();

        assert_eq!(event, CfdEvent::OfferRejected);
    }
}
