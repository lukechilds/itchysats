use crate::address_map::AddressMap;
use crate::cfd_actors;
use crate::cfd_actors::insert_cfd_and_update_feed;
use crate::cfd_actors::load_cfd;
use crate::collab_settlement_taker;
use crate::connection;
use crate::model::cfd::Cfd;
use crate::model::cfd::CollaborativeSettlement;
use crate::model::cfd::Completed;
use crate::model::cfd::Order;
use crate::model::cfd::OrderId;
use crate::model::cfd::Origin;
use crate::model::cfd::Role;
use crate::model::cfd::SetupCompleted;
use crate::model::Identity;
use crate::model::Position;
use crate::model::Price;
use crate::model::Usd;
use crate::monitor;
use crate::oracle;
use crate::process_manager;
use crate::projection;
use crate::setup_taker;
use crate::wallet;
use crate::Tasks;
use anyhow::Context as _;
use anyhow::Result;
use bdk::bitcoin::secp256k1::schnorrsig;
use xtra::prelude::*;
use xtra::Actor as _;
use xtra_productivity::xtra_productivity;

pub struct CurrentOrder(pub Option<Order>);

pub struct TakeOffer {
    pub order_id: OrderId,
    pub quantity: Usd,
}

pub struct ProposeSettlement {
    pub order_id: OrderId,
    pub current_price: Price,
}

pub struct Commit {
    pub order_id: OrderId,
}

pub struct Actor<O, W> {
    db: sqlx::SqlitePool,
    wallet: Address<W>,
    oracle_pk: schnorrsig::PublicKey,
    projection_actor: Address<projection::Actor>,
    process_manager_actor: Address<process_manager::Actor>,
    conn_actor: Address<connection::Actor>,
    setup_actors: AddressMap<OrderId, setup_taker::Actor>,
    collab_settlement_actors: AddressMap<OrderId, collab_settlement_taker::Actor>,
    oracle_actor: Address<O>,
    n_payouts: usize,
    tasks: Tasks,
    current_order: Option<Order>,
    maker_identity: Identity,
}

impl<O, W> Actor<O, W>
where
    W: xtra::Handler<wallet::TryBroadcastTransaction>
        + xtra::Handler<wallet::Sign>
        + xtra::Handler<wallet::BuildPartyParams>,
{
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: sqlx::SqlitePool,
        wallet: Address<W>,
        oracle_pk: schnorrsig::PublicKey,
        projection_actor: Address<projection::Actor>,
        process_manager_actor: Address<process_manager::Actor>,
        conn_actor: Address<connection::Actor>,
        oracle_actor: Address<O>,
        n_payouts: usize,
        maker_identity: Identity,
    ) -> Self {
        Self {
            db,
            wallet,
            oracle_pk,
            projection_actor,
            process_manager_actor,
            conn_actor,
            oracle_actor,
            n_payouts,
            setup_actors: AddressMap::default(),
            collab_settlement_actors: AddressMap::default(),
            tasks: Tasks::default(),
            current_order: None,
            maker_identity,
        }
    }
}

#[xtra_productivity]
impl<O, W> Actor<O, W>
where
    W: xtra::Handler<wallet::TryBroadcastTransaction>,
{
    async fn handle_current_order(&mut self, msg: CurrentOrder) -> Result<()> {
        let order = msg.0;

        tracing::trace!("new order {:?}", order);
        match order {
            Some(mut order) => {
                order.origin = Origin::Theirs;

                self.current_order = Some(order.clone());

                self.projection_actor
                    .send(projection::Update(Some(order)))
                    .await?;
            }
            None => {
                self.projection_actor.send(projection::Update(None)).await?;
            }
        }
        Ok(())
    }

    async fn handle_commit(&mut self, msg: Commit) -> Result<()> {
        let Commit { order_id } = msg;

        let mut conn = self.db.acquire().await?;
        cfd_actors::handle_commit(order_id, &mut conn, &self.process_manager_actor).await?;
        Ok(())
    }

    async fn handle_propose_settlement(
        &mut self,
        msg: ProposeSettlement,
        ctx: &mut xtra::Context<Self>,
    ) -> Result<()> {
        let ProposeSettlement {
            order_id,
            current_price,
        } = msg;

        let disconnected = self
            .collab_settlement_actors
            .get_disconnected(order_id)
            .with_context(|| format!("Settlement for order {} is already in progress", order_id))?;

        let mut conn = self.db.acquire().await?;
        let cfd = load_cfd(order_id, &mut conn).await?;

        let this = ctx
            .address()
            .expect("actor to be able to give address to itself");
        let (addr, fut) = collab_settlement_taker::Actor::new(
            cfd,
            self.projection_actor.clone(),
            this,
            current_price,
            self.conn_actor.clone(),
            self.n_payouts,
        )?
        .create(None)
        .run();

        disconnected.insert(addr);
        self.tasks.add(fut);

        Ok(())
    }
}

#[xtra_productivity(message_impl = false)]
impl<O, W> Actor<O, W> {
    async fn handle_setup_completed(&mut self, msg: SetupCompleted) -> Result<()> {
        let mut conn = self.db.acquire().await?;
        let order_id = msg.order_id();

        let cfd = load_cfd(order_id, &mut conn).await?;
        let event = cfd.setup_contract(msg)?;
        if let Err(e) = self
            .process_manager_actor
            .send(process_manager::Event::new(event.clone()))
            .await?
        {
            tracing::error!("Sending event to process manager failed: {:#}", e);
        }

        Ok(())
    }

    async fn handle_settlement_completed(
        &mut self,
        msg: Completed<CollaborativeSettlement>,
    ) -> Result<()> {
        let order_id = msg.order_id();
        let mut conn = self.db.acquire().await?;
        let cfd = load_cfd(order_id, &mut conn).await?;

        let event = cfd.settle_collaboratively(msg)?;
        if let Err(e) = self
            .process_manager_actor
            .send(process_manager::Event::new(event.clone()))
            .await?
        {
            tracing::error!("Sending event to process manager failed: {:#}", e);
        }

        Ok(())
    }

    async fn handle_monitor(&mut self, msg: monitor::Event) {
        if let Err(e) =
            cfd_actors::handle_monitoring_event(msg, &self.db, &self.process_manager_actor).await
        {
            tracing::error!("Unable to handle monotoring event: {:#}", e)
        }
    }

    async fn handle_attestation(&mut self, msg: oracle::Attestation) {
        if let Err(e) =
            cfd_actors::handle_oracle_attestation(msg, &self.db, &self.process_manager_actor).await
        {
            tracing::warn!("Failed to handle oracle attestation: {:#}", e)
        }
    }
}

#[xtra_productivity]
impl<O, W> Actor<O, W>
where
    Self: xtra::Handler<SetupCompleted>,
    O: xtra::Handler<oracle::GetAnnouncement> + xtra::Handler<oracle::MonitorAttestation>,
    W: xtra::Handler<wallet::BuildPartyParams> + xtra::Handler<wallet::Sign>,
{
    async fn handle_take_offer(&mut self, msg: TakeOffer, ctx: &mut Context<Self>) -> Result<()> {
        let TakeOffer { order_id, quantity } = msg;

        let disconnected = self
            .setup_actors
            .get_disconnected(order_id)
            .with_context(|| {
                format!(
                    "Contract setup for order {} is already in progress",
                    order_id
                )
            })?;

        let mut conn = self.db.acquire().await?;

        let current_order = self
            .current_order
            .clone()
            .context("No current order from maker")?;

        tracing::info!("Taking current order: {:?}", &current_order);

        // We create the cfd here without any events yet, only static data
        // Once the contract setup completes (rejected / accepted / failed) the first event will be
        // recorded
        let cfd = Cfd::from_order(
            current_order.clone(),
            Position::Long,
            quantity,
            self.maker_identity,
            Role::Taker,
        );

        insert_cfd_and_update_feed(&cfd, &mut conn, &self.projection_actor).await?;

        // Cleanup own order feed, after inserting the cfd.
        // Due to the 1:1 relationship between order and cfd we can never create another cfd for the
        // same order id.
        self.projection_actor.send(projection::Update(None)).await?;

        let announcement = self
            .oracle_actor
            .send(oracle::GetAnnouncement(current_order.oracle_event_id))
            .await?
            .with_context(|| format!("Announcement {} not found", current_order.oracle_event_id))?;

        let this = ctx
            .address()
            .expect("actor to be able to give address to itself");
        let (addr, fut) = setup_taker::Actor::new(
            (cfd, self.n_payouts),
            (self.oracle_pk, announcement),
            &self.wallet,
            &self.wallet,
            self.conn_actor.clone(),
            &this,
        )
        .create(None)
        .run();

        disconnected.insert(addr);

        self.tasks.add(fut);

        Ok(())
    }
}

impl<O: 'static, W: 'static> xtra::Actor for Actor<O, W> {}
