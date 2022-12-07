// Copyright 2020 TiKV Project Authors. Licensed under Apache-2.0.

// #[PerformanceCriticalPath]
use std::{
    borrow::Cow,
    cell::RefCell,
    sync::{atomic::AtomicUsize, Arc},
};

use crossbeam::channel::{SendError, TrySendError};
use resource_control::{ResourceController, ResourceType};
use tikv_util::mpsc;

use crate::fsm::{Fsm, FsmScheduler, FsmState, ResourceMetered};

/// A basic mailbox.
///
/// A mailbox holds an FSM owner, and the sending end of a channel to send
/// messages to that owner. Multiple producers share the same mailbox to
/// communicate with a FSM.
///
/// The mailbox's FSM owner needs to be scheduled to a [`Poller`] to handle its
/// pending messages. Therefore, the producer of messages also needs to provide
/// a channel to a poller ([`FsmScheduler`]), so that the mailbox can schedule
/// its FSM owner. When a message is sent to a mailbox, the mailbox will check
/// whether its FSM owner is idle, i.e. not already taken and scheduled. If the
/// FSM is idle, it will be scheduled immediately. By doing so, the mailbox
/// temporarily transfers its ownership of the FSM to the poller. The
/// implementation must make sure the same FSM is returned afterwards via the
/// [`release`] method.
///
/// [`Poller`]: crate::batch::Poller
pub struct BasicMailbox<Owner: Fsm> {
    sender: mpsc::LooseBoundedSender<Owner::Message>,
    state: Arc<FsmState<Owner>>,
    last_msg_group: RefCell<String>,
}

impl<Owner: Fsm> BasicMailbox<Owner> {
    #[inline]
    pub fn new(
        sender: mpsc::LooseBoundedSender<Owner::Message>,
        fsm: Box<Owner>,
        state_cnt: Arc<AtomicUsize>,
    ) -> BasicMailbox<Owner> {
        BasicMailbox {
            sender,
            state: Arc::new(FsmState::new(fsm, state_cnt)),
            last_msg_group: RefCell::new("default".to_string()),
        }
    }

    pub(crate) fn is_connected(&self) -> bool {
        self.sender.is_sender_connected()
    }

    pub(crate) fn release(&self, fsm: Box<Owner>) {
        self.state.release(fsm)
    }

    pub(crate) fn take_fsm(&self) -> Option<Box<Owner>> {
        self.state.take_fsm()
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.sender.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.sender.is_empty()
    }

    #[inline]
    pub fn last_msg_group(&self) -> &str {
        unsafe { &*self.last_msg_group.as_ptr() }
    }

    fn consume(&self, msg: &Owner::Message, resource_ctl: &ResourceController) {
        let mut dominant_group = "default".to_owned();
        let mut max_write_bytes = 0;
        if let Some(mut groups) = msg.get_resource_consumptions() {
            for (group_name, write_bytes) in groups.drain() {
                resource_ctl.consume(&group_name, ResourceType::Bytes(write_bytes));
                if write_bytes > max_write_bytes {
                    dominant_group = group_name;
                    max_write_bytes = write_bytes;
                }
            }
        }
        *self.last_msg_group.borrow_mut() = dominant_group;
    }

    /// Force sending a message despite the capacity limit on channel.
    #[inline]
    pub fn force_send<S: FsmScheduler<Fsm = Owner>>(
        &self,
        msg: Owner::Message,
        scheduler: &S,
    ) -> Result<(), SendError<Owner::Message>> {
        self.consume(&msg, scheduler.resource_ctl());
        self.sender.force_send(msg)?;
        self.state.notify(scheduler, Cow::Borrowed(self));
        Ok(())
    }

    /// Try to send a message to the mailbox.
    ///
    /// If there are too many pending messages, function may fail.
    #[inline]
    pub fn try_send<S: FsmScheduler<Fsm = Owner>>(
        &self,
        msg: Owner::Message,
        scheduler: &S,
    ) -> Result<(), TrySendError<Owner::Message>> {
        self.consume(&msg, scheduler.resource_ctl());
        self.sender.try_send(msg)?;
        self.state.notify(scheduler, Cow::Borrowed(self));
        Ok(())
    }

    /// Close the mailbox explicitly.
    #[inline]
    pub(crate) fn close(&self) {
        self.sender.close_sender();
        self.state.clear();
    }
}

impl<Owner: Fsm> Clone for BasicMailbox<Owner> {
    #[inline]
    fn clone(&self) -> BasicMailbox<Owner> {
        BasicMailbox {
            sender: self.sender.clone(),
            state: self.state.clone(),
            last_msg_group: RefCell::new("default".to_owned()),
        }
    }
}

/// A more high level mailbox that is paired with a [`FsmScheduler`].
pub struct Mailbox<Owner, Scheduler>
where
    Owner: Fsm,
    Scheduler: FsmScheduler<Fsm = Owner>,
{
    mailbox: BasicMailbox<Owner>,
    scheduler: Scheduler,
}

impl<Owner, Scheduler> Mailbox<Owner, Scheduler>
where
    Owner: Fsm,
    Scheduler: FsmScheduler<Fsm = Owner>,
{
    pub fn new(mailbox: BasicMailbox<Owner>, scheduler: Scheduler) -> Mailbox<Owner, Scheduler> {
        Mailbox { mailbox, scheduler }
    }

    /// Force sending a message despite channel capacity limit.
    #[inline]
    pub fn force_send(&self, msg: Owner::Message) -> Result<(), SendError<Owner::Message>> {
        self.mailbox.force_send(msg, &self.scheduler)
    }

    /// Try to send a message.
    #[inline]
    pub fn try_send(&self, msg: Owner::Message) -> Result<(), TrySendError<Owner::Message>> {
        self.mailbox.try_send(msg, &self.scheduler)
    }
}
