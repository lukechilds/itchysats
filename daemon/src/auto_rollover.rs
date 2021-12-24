use crate::address_map::AddressMap;
use crate::address_map::Stopping;
use crate::cfd_actors::load_cfd;
use crate::connection;
use crate::db;
use crate::model::cfd::CfdEvent;
use crate::model::cfd::OrderId;
use crate::model::cfd::RolloverCompleted;
use crate::monitor;
use crate::monitor::MonitorParams;
use crate::oracle;
use crate::projection;
use crate::rollover_taker;
use crate::Tasks;
use anyhow::bail;
use anyhow::Result;
use async_trait::async_trait;
use maia::secp256k1_zkp::schnorrsig;
use std::time::Duration;
use xtra::Actor as _;
use xtra::Address;
use xtra_productivity::xtra_productivity;

pub struct Actor<O, M> {
    db: sqlx::SqlitePool,
    oracle_pk: schnorrsig::PublicKey,
    projection_actor: Address<projection::Actor>,
    conn_actor: Address<connection::Actor>,
    monitor_actor: Address<M>,
    oracle_actor: Address<O>,
    n_payouts: usize,

    rollover_actors: AddressMap<OrderId, rollover_taker::Actor>,

    tasks: Tasks,
}

impl<O, M> Actor<O, M> {
    pub fn new(
        db: sqlx::SqlitePool,
        oracle_pk: schnorrsig::PublicKey,
        projection_actor: Address<projection::Actor>,
        conn_actor: Address<connection::Actor>,
        monitor_actor: Address<M>,
        oracle_actor: Address<O>,
        n_payouts: usize,
    ) -> Self {
        Self {
            db,
            oracle_pk,
            projection_actor,
            conn_actor,
            monitor_actor,
            oracle_actor,
            n_payouts,
            rollover_actors: AddressMap::default(),
            tasks: Tasks::default(),
        }
    }
}

#[xtra_productivity]
impl<O, M> Actor<O, M>
where
    M: xtra::Handler<monitor::StartMonitoring>,
    O: xtra::Handler<oracle::MonitorAttestation> + xtra::Handler<oracle::GetAnnouncement>,
{
    async fn handle(&mut self, _msg: AutoRollover, ctx: &mut xtra::Context<Self>) -> Result<()> {
        tracing::trace!("Checking all CFDs for rollover eligibility");

        let mut conn = self.db.acquire().await?;
        let cfd_ids = db::load_all_cfd_ids(&mut conn).await?;

        let this = ctx
            .address()
            .expect("actor to be able to give address to itself");

        for id in cfd_ids {
            let disconnected = match self.rollover_actors.get_disconnected(id) {
                Ok(disconnected) => disconnected,
                Err(_) => {
                    tracing::debug!(order_id=%id, "Rollover already in progress");
                    continue;
                }
            };

            // TODO: Shall this have a try_continue?
            let cfd = load_cfd(id, &mut conn).await?;

            let (addr, fut) = rollover_taker::Actor::new(
                (cfd, self.n_payouts),
                self.oracle_pk,
                self.conn_actor.clone(),
                &self.oracle_actor,
                self.projection_actor.clone(),
                &this,
                (&this, &self.conn_actor),
            )
            .create(None)
            .run();

            disconnected.insert(addr);
            self.tasks.add(fut);
        }

        Ok(())
    }
}

#[xtra_productivity(message_impl = false)]
impl<O, M> Actor<O, M>
where
    O: 'static,
    M: 'static,
    M: xtra::Handler<monitor::StartMonitoring>,
    O: xtra::Handler<oracle::MonitorAttestation> + xtra::Handler<oracle::GetAnnouncement>,
{
    async fn handle_rollover_completed(&mut self, msg: RolloverCompleted) -> Result<()> {
        let mut conn = self.db.acquire().await?;
        let order_id = msg.order_id();

        let cfd = load_cfd(order_id, &mut conn).await?;

        let event = match cfd.rollover(msg)? {
            Some(event) => event,
            None => return Ok(()),
        };

        db::append_event(event.clone(), &mut conn).await?;

        self.projection_actor.send(projection::CfdsChanged).await?;

        let dlc = match event.event {
            CfdEvent::RolloverCompleted { dlc } => dlc,
            CfdEvent::RolloverFailed | CfdEvent::RolloverRejected => {
                return Ok(());
            }
            _ => bail!("Unexpected event {:?}", event.event),
        };

        tracing::info!("Setup complete, publishing on chain now");

        self.monitor_actor
            .send(monitor::StartMonitoring {
                id: order_id,
                params: MonitorParams::new(dlc.clone()),
            })
            .await?;

        self.oracle_actor
            .send(oracle::MonitorAttestation {
                event_id: dlc.settlement_event_id,
            })
            .await?;

        Ok(())
    }
}

#[xtra_productivity(message_impl = false)]
impl<O, M> Actor<O, M>
where
    M: xtra::Handler<monitor::StartMonitoring>,
    O: xtra::Handler<oracle::MonitorAttestation> + xtra::Handler<oracle::GetAnnouncement>,
{
    async fn handle_rollover_actor_stopping(&mut self, msg: Stopping<rollover_taker::Actor>) {
        self.rollover_actors.gc(msg);
    }
}

#[async_trait]
impl<O, M> xtra::Actor for Actor<O, M>
where
    O: 'static,
    M: 'static,
    Self: xtra::Handler<AutoRollover>,
{
    async fn started(&mut self, ctx: &mut xtra::Context<Self>) {
        let fut = ctx
            .notify_interval(Duration::from_secs(5 * 60), || AutoRollover)
            .expect("we are alive");

        self.tasks.add(fut);
    }
}

/// Message to trigger roll-over on a regular interval
pub struct AutoRollover;
