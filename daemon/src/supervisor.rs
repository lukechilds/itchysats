use crate::Tasks;
use async_trait::async_trait;
use std::fmt;
use xtra::Address;
use xtra::Context;
use xtra_productivity::xtra_productivity;

/// A supervising actor reacts to messages from the actor it is supervising and restarts it based on
/// a given policy.
pub struct Actor<T, R> {
    context: Context<T>,
    ctor: Box<dyn Fn(Address<Self>) -> T + Send + 'static>,
    tasks: Tasks,
    restart_policy: Box<dyn FnMut(R) -> bool + Send + 'static>,
}

impl<T, R> Actor<T, R>
where
    T: xtra::Actor,
    R: fmt::Display + 'static,
{
    /// Construct a new supervisor.
    ///
    /// The supervisor needs to know two things:
    /// 1. How to construct an instance of the actor.
    /// 2. When to construct an instance of the actor.
    pub fn new(
        ctor: impl (Fn(Address<Self>) -> T) + Send + 'static,
        restart_policy: impl (FnMut(R) -> bool) + Send + 'static,
    ) -> (Self, Address<T>) {
        let (address, context) = Context::new(None);

        let supervisor = Self {
            context,
            ctor: Box::new(ctor),
            tasks: Tasks::default(),
            restart_policy: Box::new(restart_policy),
        };

        (supervisor, address)
    }

    fn spawn_new(&mut self, ctx: &mut Context<Self>) {
        tracing::info!("Spawning new instance of actor"); // TODO: Include name

        let this = ctx.address().expect("we are alive");
        let actor = (self.ctor)(this);

        self.tasks.add(self.context.attach(actor));
    }
}

#[async_trait]
impl<T, R> xtra::Actor for Actor<T, R>
where
    T: xtra::Actor,
    R: fmt::Display + 'static,
{
    async fn started(&mut self, ctx: &mut Context<Self>) {
        self.spawn_new(ctx);
    }
}

#[xtra_productivity]
impl<T, R> Actor<T, R>
where
    T: xtra::Actor,
    R: fmt::Display + 'static,
{
    pub fn handle(&mut self, msg: Stopped<R>, ctx: &mut Context<Self>) {
        let reason = msg.reason;

        tracing::info!("Actor stopped: {}", reason); // TODO: Include name of actor

        let should_restart = (self.restart_policy)(reason);

        tracing::debug!("Restart actor? {}", should_restart);

        if should_restart {
            self.spawn_new(ctx)
        }
    }
}

/// Tell the supervisor that the actor was stopped.
///
/// The given `reason` will be passed to the `restart_policy` configured in the supervisor. If it
/// yields `true`, a new instance of the actor will be spawned.
pub struct Stopped<R> {
    pub reason: R,
}
