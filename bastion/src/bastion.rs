use crate::broadcast::{Broadcast, Parent};
use crate::children::{Children, ChildrenRef};
use crate::config::Config;
use crate::message::{BastionMessage, Message};
use crate::supervisor::{Supervisor, SupervisorRef};
use crate::system::SYSTEM;
use bastion_executor::run::run;
use core::future::Future;
use lightproc::proc_stack::ProcStack;
use std::fmt::{self, Debug, Formatter};
use std::thread;

/// A `struct` allowing to access the system's API to initialize it,
/// start, stop and kill it and to create new supervisors and top-level
/// children groups.
///
/// # Example
///
/// ```rust
/// use bastion::prelude::*;
///
/// fn main() {
///     /// Creating the system's configuration...
///     let config = Config::new().hide_backtraces();
///     // ...and initializing the system with it (this is required)...
///     Bastion::init_with(config);
///
///     // Note that `Bastion::init();` would work too and initialize
///     // the system with the default config.
///
///     // Starting the system...
///     Bastion::start();
///
///     // Creating a new supervisor...
///     let supervisor = Bastion::supervisor(|sp| {
///         sp
///         // ...with a specific supervision strategy...
///             .with_strategy(SupervisionStrategy::OneForAll)
///         // ...and some supervised children groups...
///             .children(|children| {
///                 // ...
///                 # children
///             })
///             .children(|children| {
///                 // ...
///                 # children
///             })
///         // ...or even supervised supervisors...
///             .supervisor(|sp| {
///                 // ...
///                 # sp
///             })
///     }).expect("Couldn't create the supervisor.");
///
///     // ...which can start supervising new children groups
///     // later on...
///     supervisor.children(|children| {
///         // ...
///         # children
///     }).expect("Couldn't create the supervised children group.");
///
///     // ...or broadcast messages to all its supervised children
///     // and supervisors...
///     supervisor.broadcast("A message containing data.").expect("Couldn't broadcast the message.");
///
///     // ...and then can even be stopped or killed...
///     supervisor.stop().expect("Couldn't stop the supervisor");
///     // supervisor.kill().expect("Couldn't kill the supervisor");
///
///     // Creating a new top-level children group...
///     let children = Bastion::children(|children| {
///         children
///         // ...containing a defined number of elements...
///             .with_redundancy(4)
///         // ...all executing a similar future...
///             .with_exec(|ctx: BastionContext| {
///                 async move {
///                     // ...receiving and matching messages...
///                     msg! { ctx.recv().await?,
///                         ref msg: &'static str => {
///                             // ...
///                         };
///                         msg: &'static str => {
///                             // ...
///                         };
///                         msg: &'static str =!> {
///                             // ...
///                         };
///                         // ...
///                         _: _ => ();
///                     }
///
///                     // ...
///
///                     Ok(())
///                 }
///             })
///     }).expect("Couldn't create the children group.");
///
///     // ...which can broadcast messages to all its elements...
///     children.broadcast("A message containing data.").expect("Couldn't broadcast the message.");
///
///     // ...and then can even be stopped or killed...
///     children.stop().expect("Couldn't stop the children group.");
///     // children.kill().expect("Couldn't kill the children group.");
///
///     // Create a new top-level children group and getting a list
///     // of reference to its elements...
///     let children = Bastion::children(|children| {
///         // ...
///         # children
///     }).expect("Couldn't create the children group.");
///     let elems: &[ChildRef] = children.elems();
///
///     // ...to then get one of its elements' reference...
///     let child = &elems[0];
///
///     // ...to then "tell" it messages...
///     child.tell("A message containing data.").expect("Couldn't send the message.");
///
///     // ...or "ask" it messages...
///     let answer: Answer = child.ask("A message containing data.").expect("Couldn't send the message.");
///     # async {
///     // ...until the child eventually answers back...
///     let answer: Result<Msg, ()> = answer.await;
///     # };
///
///     // ...and then even stop or kill it...
///     child.stop().expect("Couldn't stop the child.");
///     // child.kill().expect("Couldn't kill the child.");
///
///     // Broadcasting a message to all the system's children...
///     Bastion::broadcast("A message containing data.").expect("Couldn't send the message.");
///
///     // Stopping or killing the system...
///     Bastion::stop();
///     // Bastion::kill();
///
///     // Blocking until the system has stopped (or got killed)...
///     Bastion::block_until_stopped();
/// }
/// ```
pub struct Bastion {
    _priv: (),
}

impl Bastion {
    /// Initializes the system if it hasn't already been done, using
    /// the default [`Config`].
    ///
    /// **It is required that you call `Bastion::init` or
    /// [`Bastion::init_with`] at least once before using any of
    /// bastion's features.**
    ///
    /// # Example
    ///
    /// ```rust
    /// use bastion::prelude::*;
    ///
    /// fn main() {
    ///     Bastion::init();
    ///
    ///     // You can now use bastion...
    ///     #
    ///     # Bastion::start();
    ///     # Bastion::stop();
    ///     # Bastion::block_until_stopped();
    /// }
    /// ```
    ///
    /// [`Config`]: struct.Config.html
    /// [`Bastion::init_with`]: #method.init_with
    pub fn init() {
        let config = Config::default();
        Bastion::init_with(config)
    }

    /// Initializes the system if it hasn't already been done, using
    /// the specified [`Config`].
    ///
    /// **It is required that you call [`Bastion::init`] or
    /// `Bastion::init_with` at least once before using any of
    /// bastion's features.**
    ///
    /// # Arguments
    ///
    /// * `config` - The configuration used to initialize the system.
    ///
    /// # Example
    ///
    /// ```rust
    /// use bastion::prelude::*;
    ///
    /// fn main() {
    ///     let config = Config::new()
    ///         .show_backtraces();
    ///
    ///     Bastion::init_with(config);
    ///
    ///     // You can now use bastion...
    ///     #
    ///     # Bastion::start();
    ///     # Bastion::stop();
    ///     # Bastion::block_until_stopped();
    /// }
    /// ```
    ///
    /// [`Config`]: struct.Config.html
    /// [`Bastion::init`]: #method.init
    pub fn init_with(config: Config) {
        debug!("Bastion: Initializing with config: {:?}", config);
        if config.backtraces().is_hide() {
            debug!("Bastion: Hiding backtraces.");
            std::panic::set_hook(Box::new(|_| ()));
        }

        // NOTE: this is just to make sure that SYSTEM has been initialized by lazy_static
        SYSTEM.sender().is_closed();
    }

    /// Creates a new [`Supervisor`], passes it through the specified
    /// `init` closure and then sends it to the system for it to
    /// start supervising children.
    ///
    /// This method returns a [`SupervisorRef`] referencing the newly
    /// created supervisor if it succeeded, or `Err(())`
    /// otherwise.
    ///
    /// # Arguments
    ///
    /// * `init` - The closure taking the new [`Supervisor`] as an
    ///     argument and returning it once configured.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use bastion::prelude::*;
    /// #
    /// # fn main() {
    ///     # Bastion::init();
    ///     #
    /// let sp_ref: SupervisorRef = Bastion::supervisor(|sp| {
    ///     // Configure the supervisor...
    ///     sp.with_strategy(SupervisionStrategy::OneForOne)
    ///     // ...and return it.
    /// }).expect("Couldn't create the supervisor.");
    ///     #
    ///     # Bastion::start();
    ///     # Bastion::stop();
    ///     # Bastion::block_until_stopped();
    /// # }
    /// ```
    ///
    /// [`Supervisor`]: supervisor/struct.Supervisor.html
    /// [`SupervisorRef`]: supervisor/struct.SupervisorRef.html
    pub fn supervisor<S>(init: S) -> Result<SupervisorRef, ()>
    where
        S: FnOnce(Supervisor) -> Supervisor,
    {
        debug!("Bastion: Creating supervisor.");
        let parent = Parent::system();
        let bcast = Broadcast::new(parent);

        debug!("Bastion: Initializing Supervisor({}).", bcast.id());
        let supervisor = Supervisor::new(bcast);
        let supervisor = init(supervisor);
        debug!("Supervisor({}): Initialized.", supervisor.id());
        let supervisor_ref = supervisor.as_ref();

        debug!("Bastion: Deploying Supervisor({}).", supervisor.id());
        let msg = BastionMessage::deploy_supervisor(supervisor);
        trace!("Bastion: Sending message: {:?}", msg);
        SYSTEM.sender().unbounded_send(msg).map_err(|_| ())?;

        Ok(supervisor_ref)
    }

    /// Creates a new [`Children`], passes it through the specified
    /// `init` closure and then sends it to the system's default
    /// supervisor for it to start supervising it.
    ///
    /// This methods returns a [`ChildrenRef`] referencing the newly
    /// created children group it it succeeded, or `Err(())`
    /// otherwise.
    ///
    /// Note that the "system supervisor" is a supervisor created
    /// by the system at startup.
    ///
    /// # Arguments
    ///
    /// * `init` - The closure taking the new [`Children`] as an
    ///     argument and returning it once configured.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use bastion::prelude::*;
    /// #
    /// # fn main() {
    ///     # Bastion::init();
    ///     #
    /// let children_ref: ChildrenRef = Bastion::children(|children| {
    ///     // Configure the children group...
    ///     children.with_exec(|ctx: BastionContext| {
    ///         async move {
    ///             // Send and receive messages...
    ///             let opt_msg: Option<Msg> = ctx.try_recv().await;
    ///             // ...and return `Ok(())` or `Err(())` when you are done...
    ///             Ok(())
    ///
    ///             // Note that if `Err(())` was returned, the supervisor would
    ///             // restart the children group.
    ///         }
    ///     })
    ///     // ...and return it.
    /// }).expect("Couldn't create the children group.");
    ///     #
    ///     # Bastion::start();
    ///     # Bastion::stop();
    ///     # Bastion::block_until_stopped();
    /// # }
    /// ```
    ///
    /// [`Children`]: children/struct.Children.html
    /// [`ChildrenRef`]: children/struct.ChildrenRef.html
    pub fn children<C>(init: C) -> Result<ChildrenRef, ()>
    where
        C: FnOnce(Children) -> Children,
    {
        debug!("Bastion: Creating children group.");
        SYSTEM.supervisor().children(init)
    }

    /// Sends a message to the system which will then send it to all
    /// the root-level supervisors and their supervised children and
    /// supervisors, etc.
    ///
    /// This method returns `()` if it succeeded, or `Err(msg)`
    /// otherwise.
    ///
    /// # Arguments
    ///
    /// * `msg` - The message to send.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use bastion::prelude::*;
    /// #
    /// # fn main() {
    ///     # Bastion::init();
    ///     #
    /// let msg = "A message containing data.";
    /// Bastion::broadcast(msg).expect("Couldn't send the message.");
    ///
    ///     # Bastion::children(|children| {
    ///         # children.with_exec(|ctx: BastionContext| {
    ///             # async move {
    /// // And then in every children groups's elements' future...
    /// msg! { ctx.recv().await?,
    ///     ref msg: &'static str => {
    ///         assert_eq!(msg, &"A message containing data.");
    ///     };
    ///     // We are only broadcasting a `&'static str` in this
    ///     // example, so we know that this won't happen...
    ///     _: _ => ();
    /// }
    ///                 #
    ///                 # Ok(())
    ///             # }
    ///         # })
    ///     # }).unwrap();
    ///     #
    ///     # Bastion::start();
    ///     # Bastion::stop();
    ///     # Bastion::block_until_stopped();
    /// # }
    /// ```
    pub fn broadcast<M: Message>(msg: M) -> Result<(), M> {
        debug!("Bastion: Broadcasting message: {:?}", msg);
        let msg = BastionMessage::broadcast(msg);
        trace!("Bastion: Sending message: {:?}", msg);
        // FIXME: panics?
        SYSTEM
            .sender()
            .unbounded_send(msg)
            .map_err(|err| err.into_inner().into_msg().unwrap())
    }

    /// Sends a message to the system to tell it to start
    /// handling messages and running children.
    ///
    /// # Example
    ///
    /// ```rust
    /// use bastion::prelude::*;
    ///
    /// fn main() {
    ///     Bastion::init();
    ///
    ///     // Use bastion, spawn children and supervisors...
    ///
    ///     Bastion::start();
    ///
    ///     // The system will soon start, messages will
    ///     // now be handled...
    ///     #
    ///     # Bastion::stop();
    ///     # Bastion::block_until_stopped();
    /// }
    /// ```
    pub fn start() {
        debug!("Bastion: Starting.");
        let msg = BastionMessage::start();
        trace!("Bastion: Sending message: {:?}", msg);
        // FIXME: Err(Error)
        SYSTEM.sender().unbounded_send(msg).ok();
    }

    /// Sends a message to the system to tell it to stop
    /// every running children groups and supervisors.
    ///
    /// # Example
    ///
    /// ```rust
    /// use bastion::prelude::*;
    ///
    /// fn main() {
    ///     Bastion::init();
    ///
    ///     // Use bastion, spawn children and supervisors...
    ///
    ///     Bastion::start();
    ///
    ///     // Send messages to children and/or do some
    ///     // work until you decide to stop the system...
    ///
    ///     Bastion::stop();
    ///     # Bastion::block_until_stopped();
    /// }
    /// ```
    pub fn stop() {
        debug!("Bastion: Stopping.");
        let msg = BastionMessage::stop();
        trace!("Bastion: Sending message: {:?}", msg);
        // FIXME: Err(Error)
        SYSTEM.sender().unbounded_send(msg).ok();
    }

    /// Sends a message to the system to tell it to kill every
    /// running children groups and supervisors
    ///
    /// # Example
    ///
    /// ```rust
    /// use bastion::prelude::*;
    ///
    /// fn main() {
    ///     Bastion::init();
    ///
    ///     // Use bastion, spawn children and supervisors...
    ///
    ///     Bastion::start();
    ///     // Send messages to children and/or do some
    ///     // work until you decide to kill the system...
    ///
    ///     Bastion::kill();
    ///     # Bastion::block_until_stopped();
    /// }
    /// ```
    pub fn kill() {
        debug!("Bastion: Killing.");
        let msg = BastionMessage::kill();
        trace!("Bastion: Sending message: {:?}", msg);
        // FIXME: Err(Error)
        SYSTEM.sender().unbounded_send(msg).ok();

        // FIXME: panics
        let mut system = SYSTEM.handle().lock().wait().unwrap();
        if let Some(system) = system.take() {
            debug!("Bastion: Cancelling system handle.");
            system.cancel();
        }
    }

    /// Blocks the current thread until the system is stopped
    /// (either by calling [`Bastion::stop()`] or
    /// [`Bastion::kill`]).
    ///
    /// # Example
    ///
    /// ```rust
    /// use bastion::prelude::*;
    ///
    /// fn main() {
    ///     Bastion::init();
    ///
    ///     // Use bastion, spawn children and supervisors...
    ///
    ///     Bastion::start();
    ///     // Send messages to children and/or do some
    ///     // work...
    ///
    ///     # Bastion::stop();
    ///     Bastion::block_until_stopped();
    ///     // The system is now stopped. A child might have
    ///     // stopped or killed it...
    /// }
    /// ```
    ///
    /// [`Bastion::stop()`]: #method.stop
    /// [`Bastion::kill()`]: #method.kill
    pub fn block_until_stopped() {
        debug!("Bastion: Blocking until system is stopped.");
        run(
            async {
                loop {
                    // FIXME: panics
                    let system = SYSTEM.handle().lock().wait().unwrap();
                    if system.is_none() {
                        debug!("Bastion: Unblocking because system is stopped.");
                        return;
                    }

                    // thread::yield_now();
                }
            },
            ProcStack::default(),
        )
    }
}

impl Debug for Bastion {
    fn fmt(&self, fmt: &mut Formatter) -> fmt::Result {
        fmt.debug_struct("Bastion").finish()
    }
}
