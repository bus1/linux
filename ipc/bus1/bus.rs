//! Bus Management
//!
//! This module implements the core components of the bus. It provides peers,
//! nodes, handles, as well as message handling and atomic operations.

use core::ptr::NonNull;
use kernel::prelude::*;
use kernel::alloc::AllocError;
use kernel::sync::{Arc, ArcBorrow, atomic};
use crate::{acct, capi, util::{self, field, lll, rb, slist}};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MessageError {
    /// Could not allocate required state-tracking.
    Alloc(AllocError),
    /// The target handle or a transfer handle is not owned by the operator.
    HandleForeign,
}

#[pin_data]
struct Peer {
    actor: Arc<acct::Actor>,
    waitq: *mut kernel::bindings::wait_queue_head,
    queue: lll::List<TxNodeRef>,
    queue_committed: atomic::Atomic<usize>,
    shutdown: Arc<Tx>,
    #[pin]
    inner: kernel::sync::Mutex<PeerLocked>,
}

struct PeerLocked {
    queue_ready: slist::List<TxNodeRef>,
    queue_busy: slist::List<TxNodeRef>,
}

#[pin_data]
struct Node {
    owner: Arc<Peer>,
    userdata: atomic::Atomic<usize>,
    op_rb: rb::Node,
    #[pin]
    inner: kernel::sync::Mutex<NodeLocked>,
}

util::field::impl_pin_field!(Node, op_rb, rb::Node);

struct NodeLocked {
    handles: rb::Tree<rb::node_of!(Arc<Handle>, node_rb)>,
}

struct Handle {
    node: Arc<Node>,
    owner: Arc<Peer>,
    userdata: atomic::Atomic<usize>,
    node_rb: rb::Node,
    op_rb: rb::Node,
    release_node: TxNode,
    release_handle: TxNode,
}

util::field::impl_pin_field!(Handle, node_rb, rb::Node);
util::field::impl_pin_field!(Handle, op_rb, rb::Node);
util::field::impl_pin_field!(Handle, release_node, TxNode);
util::field::impl_pin_field!(Handle, release_handle, TxNode);

struct Message {
    via: Arc<Handle>,
    transfers: KBox<[*mut capi::b1_handle]>,
    shared: Arc<MessageShared>,
    op_rb: rb::Node,
    tx_node: TxNode,
}

util::field::impl_pin_field!(Message, op_rb, rb::Node);
util::field::impl_pin_field!(Message, tx_node, TxNode);

struct MessageShared {
    data: KVBox<[u8]>,
}

struct Op {
    operator: Arc<Peer>,
    tx: Arc<Tx>,
    messages: rb::Tree<rb::node_of!(Arc<Message>, op_rb)>,
    nodes: rb::Tree<rb::node_of!(Arc<Node>, op_rb)>,
    handles: rb::Tree<rb::node_of!(Arc<Handle>, op_rb)>,
}

struct Tx {
    committed: atomic::Atomic<bool>,
}

struct TxNode {
    kind: TxNodeKind,
    peer_link: lll::Node,
    // XXX: Switch to atomic pointers once available.
    tx: atomic::Atomic<usize>,
}

util::field::impl_pin_field!(TxNode, peer_link, lll::Node);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TxNodeKind {
    User,
    ReleaseNode,
    ReleaseHandle,
}

#[derive(Clone)]
enum TxNodeRef {
    User(Arc<Message>),
    ReleaseNode(Arc<Handle>),
    ReleaseHandle(Arc<Handle>),
}

impl core::convert::From<AllocError> for MessageError {
    fn from(v: AllocError) -> Self {
        Self::Alloc(v)
    }
}

impl Peer {
    fn new(
        actor: Arc<acct::Actor>,
        waitq: *mut kernel::bindings::wait_queue_head,
    ) -> Result<Arc<Self>, AllocError> {
        let tx = Tx::new()?;
        match Arc::pin_init(
            pin_init!(Self {
                actor,
                waitq,
                queue: lll::List::new(),
                queue_committed: atomic::Atomic::new(0),
                shutdown: tx,
                inner <- kernel::sync::new_mutex!(
                    PeerLocked {
                        queue_ready: slist::List::new(),
                        queue_busy: slist::List::new(),
                    },
                ),
            }),
            GFP_KERNEL,
        ) {
            Ok(v) => Ok(v),
            Err(_) => Err(AllocError),
        }
    }

    /// Turn the reference into a raw pointer.
    ///
    /// This will leak the reference and any pinned resources, unless the
    /// original object is recreated via `Self::from_raw()`.
    fn into_raw(this: Arc<Self>) -> *mut capi::b1_peer {
        Arc::into_raw(this).cast_mut().cast()
    }

    /// Recreate the reference from its raw pointer.
    ///
    /// ## Safety
    ///
    /// The caller must guarantee this pointer was acquired via
    /// `Self::into_raw()`, and they must refrain from using the pointer any
    /// further.
    unsafe fn from_raw(this: *mut capi::b1_peer) -> Arc<Self> {
        // SAFETY: Caller guarantees `this` is from `Self::into_raw()`.
        unsafe { Arc::from_raw(this.cast::<Self>()) }
    }

    /// Borrow a raw reference.
    ///
    /// ## Safety
    ///
    /// The caller must guarantee this pointer was acquired via
    /// `Self::into_raw()`, and they must refrain from releasing it via
    /// `Self::from_raw()` for `'a`.
    unsafe fn borrow_raw<'a>(
        this: *mut capi::b1_peer,
    ) -> ArcBorrow<'a, Self> {
        // SAFETY: Caller guarantees `this` is from `Self::into_raw()` and
        // will not be released for `'a`.
        unsafe { ArcBorrow::from_raw(this.cast::<Self>()) }
    }

    fn owns_node(&self, node: &Node) -> bool {
        core::ptr::eq(self, &*node.owner)
    }

    fn owns_handle(&self, handle: &Handle) -> bool {
        core::ptr::eq(self, &*handle.owner)
    }

    fn wake(&self) {
        // XXX: This needs to be synchronized through begin()/end() and
        // protected via rcu. The waitq should be considered detached on
        // `end()`, but accessible for at least an rcu grace period.
        unsafe {
            kernel::bindings::__wake_up(
                self.waitq,
                kernel::bindings::TASK_INTERRUPTIBLE,
                1,
                core::ptr::null_mut(),
            );
        }
    }

    fn begin(&self) {
        // Called when the owner of the peer considers it set up. Given that
        // peers are standalone, there is nothing to be done here. As long as
        // the caller did not expose it, they still retain full control.
    }

    /// Shutdown operations on this peer.
    ///
    /// Generally, a peer reference can simply be dropped without any shutdown.
    /// As long as all nodes are released, the peer can no longer be reached.
    /// However, due to the parallel nature of the bus, there might be messages
    /// being about to be queued on this peer, even though the involved nodes
    /// have been released. This might leave circular references hanging, if
    /// those messages carry handles (as those handles carry a peer reference
    /// themselves).
    ///
    /// To prevent this scenario, a shutdown will seal the queue of a peer and
    /// ensure any ongoing transactions will discard the messages destined to
    /// this peer (as well as any already queued messages).
    ///
    /// Usually, the peer should not longer be used for bus operations after a
    /// shutdown, and any nodes should have been released before.
    fn end(self: ArcBorrow<'_, Self>) {
        let mut op = core::pin::pin!(Op::with(self.into(), self.shutdown.clone()));

        // Properly release all handles that are currently queued on pending
        // messages.
        let mut q = self.queue.seal();
        while let Some(v) = q.unlink_front() {
            if let Some(m) = TxNodeRef::as_user(&v) {
                for t in &*m.transfers {
                    let b = unsafe { Handle::borrow_raw(*t) };
                    op.as_mut().release_handle(b.into());
                }
            }
        }

        op.commit();
    }

    fn create_node(
        self: ArcBorrow<'_, Self>,
        other: ArcBorrow<'_, Self>,
    ) -> Result<(Arc<Node>, Arc<Handle>), AllocError> {
        let n = Node::new(self.into())?;
        let h = Handle::new(n.clone(), other.into())?;
        Ok((n, h))
    }

    fn create_handle(
        self: ArcBorrow<'_, Self>,
        from: ArcBorrow<'_, Handle>,
    ) -> Result<Arc<Handle>, AllocError> {
        Handle::new(from.node.clone(), self.into())
    }

    fn readable(
        self: ArcBorrow<'_, Self>,
    ) -> bool {
        self.queue_committed.load(atomic::Relaxed) > 0
    }

    fn peek(
        self: ArcBorrow<'_, Self>,
        peek: &mut capi::b1_peer_peek,
    ) -> bool {
        if !self.readable() {
            return false;
        }

        let mut peer_guard = self.inner.lock();
        peer_guard.as_mut().prefetch(&self.queue);
        let (ready, _) = peer_guard.as_mut().unfold_mut();

        let Some(txref) = ready.cursor_mut().get_clone() else {
            return false;
        };

        if let Some(m) = TxNodeRef::as_user(&txref) {
            peek.type_ = capi::bus1_message_type_BUS1_MESSAGE_TYPE_USER;
            peek.u.user.node = Arc::as_ptr(&m.via.node).cast_mut().cast();
            peek.u.user.n_transfers = m.transfers.len() as u64;
            peek.u.user.transfers = m.transfers.as_ptr().cast_mut();
            peek.u.user.n_data = m.shared.data.len() as u64;
            peek.u.user.data = m.shared.data.as_ptr().cast_mut().cast();
            true
        } else if let Some(h) = TxNodeRef::as_release_node(&txref) {
            peek.type_ = capi::bus1_message_type_BUS1_MESSAGE_TYPE_NODE_RELEASE;
            peek.u.node_release.handle = Arc::as_ptr(h).cast_mut().cast();
            true
        } else if let Some(h) = TxNodeRef::as_release_handle(&txref) {
            peek.type_ = capi::bus1_message_type_BUS1_MESSAGE_TYPE_HANDLE_RELEASE;
            peek.u.handle_release.node = Arc::as_ptr(&h.node).cast_mut().cast();
            true
        } else {
            ready.unlink_front();
            self.peek(peek)
        }
    }

    fn pop(self: ArcBorrow<'_, Self>) {
        let mut peer_guard = self.inner.lock();
        let (ready, _) = peer_guard.as_mut().unfold_mut();
        ready.unlink_front();
    }
}

impl PeerLocked {
    fn unfold_mut(
        self: Pin<&mut Self>,
    ) -> (
        &mut slist::List<TxNodeRef>,
        &mut slist::List<TxNodeRef>,
    ) {
        // SAFETY: Nothing is structurally pinned.
        unsafe {
            let inner = Pin::into_inner_unchecked(self);
            (
                &mut inner.queue_ready,
                &mut inner.queue_busy,
            )
        }
    }

    fn prefetch(
        self: Pin<&mut Self>,
        queue: &lll::List<TxNodeRef>,
    ) {
        let (ready, busy) = self.unfold_mut();

        if !ready.is_empty() {
            return;
        }

        // Fetch the entire incoming queue and create an iterator for it. Note
        // that new entries are added at the front, so this queue is in reverse
        // order. But this is exactly what we need. Iterate the queue in this
        // reverse order (so newest entries first). Any entry that is committed
        // is then pushed to the ready list, uncommitted entries are left on
        // the todo list. When done, the `ready` list contains the ready items
        // in chronological order, so oldest message first, but `todo` has the
        // remaining uncommitted items in the same inverse chronological order.
        //
        // This todo list is then saved in `busy` to be iterated on the next
        // prefetch. However, first, any previous leftover busy list is
        // appended to the end of the `todo` queue, with any committed entries
        // moved to `ready`.

        let mut todo = queue.clear();
        let mut c = todo.cursor_mut();

        while let Some(v) = c.get() {
            if v.is_committed() {
                if let Some(v) = c.unlink() {
                    let _ = ready.try_link_front(v);
                    continue;
                }
            }
            c.move_next();
        }

        while let Some(v) = busy.unlink_front() {
            if TxNodeRef::tx_node(&v).is_committed() {
                let _ = ready.try_link_front(v);
            } else {
                let _ = c.try_link(v);
                c.move_next();
            }
        }

        core::mem::swap(busy, &mut todo);
    }
}

impl Node {
    fn new(
        owner: Arc<Peer>,
    ) -> Result<Arc<Self>, AllocError> {
        match Arc::pin_init(
            pin_init!(Self {
                owner,
                userdata: atomic::Atomic::new(0),
                op_rb: rb::Node::new(),
                inner <- kernel::sync::new_mutex!(
                    NodeLocked {
                        handles: rb::Tree::new(),
                    },
                ),
            }),
            GFP_KERNEL,
        ) {
            Ok(v) => Ok(v),
            Err(_) => Err(AllocError),
        }
    }

    /// Turn the reference into a raw pointer.
    ///
    /// This will leak the reference and any pinned resources, unless the
    /// original object is recreated via `Self::from_raw()`.
    fn into_raw(this: Arc<Self>) -> *mut capi::b1_node {
        Arc::into_raw(this).cast_mut().cast()
    }

    /// Recreate the reference from its raw pointer.
    ///
    /// ## Safety
    ///
    /// The caller must guarantee this pointer was acquired via
    /// `Self::into_raw()`, and they must refrain from using the pointer any
    /// further.
    unsafe fn from_raw(this: *mut capi::b1_node) -> Arc<Self> {
        // SAFETY: Caller guarantees `this` is from `Self::into_raw()`.
        unsafe { Arc::from_raw(this.cast::<Self>()) }
    }

    /// Borrow a raw reference.
    ///
    /// ## Safety
    ///
    /// The caller must guarantee this pointer was acquired via
    /// `Self::into_raw()`, and they must refrain from releasing it via
    /// `Self::from_raw()` for `'a`.
    unsafe fn borrow_raw<'a>(
        this: *mut capi::b1_node,
    ) -> ArcBorrow<'a, Self> {
        // SAFETY: Caller guarantees `this` is from `Self::into_raw()` and
        // will not be released for `'a`.
        unsafe { ArcBorrow::from_raw(this.cast::<Self>()) }
    }

    fn begin(&self) {
        // Called by the node owner when the node is ready. Since nodes are
        // completely independent, there is nothing to be done. The node will
        // remain isolated for as long as the owner does not pass it along.
    }

    fn end(&self) {
        // Called when a node has been released and a node owner will refrain
        // from using it, anymore. Preferably, we would verify that the node
        // has a transaction assigned (yet might be pending), but so far nodes
        // do not carry such information, only their linked handles. Hence, we
        // perform no validation for now.
    }
}

impl NodeLocked {
    fn unfold_mut(
        self: Pin<&mut Self>,
    ) -> (
        Pin<&mut rb::Tree<rb::node_of!(Arc<Handle>, node_rb)>>,
    ) {
        // SAFETY: Only `Self.handles` is structurally pinned.
        unsafe {
            let inner = Pin::into_inner_unchecked(self);
            (
                Pin::new_unchecked(&mut inner.handles),
            )
        }
    }
}

impl Handle {
    fn new(
        node: Arc<Node>,
        owner: Arc<Peer>,
    ) -> Result<Arc<Handle>, AllocError> {
        Arc::new(
            Self {
                node,
                owner,
                userdata: atomic::Atomic::new(0),
                node_rb: rb::Node::new(),
                op_rb: rb::Node::new(),
                release_node: TxNode::new(TxNodeKind::ReleaseNode),
                release_handle: TxNode::new(TxNodeKind::ReleaseHandle),
            },
            GFP_KERNEL,
        )
    }

    /// Turn the reference into a raw pointer.
    ///
    /// This will leak the reference and any pinned resources, unless the
    /// original object is recreated via `Self::from_raw()`.
    fn into_raw(this: Arc<Self>) -> *mut capi::b1_handle {
        Arc::into_raw(this).cast_mut().cast()
    }

    /// Recreate the reference from its raw pointer.
    ///
    /// ## Safety
    ///
    /// The caller must guarantee this pointer was acquired via
    /// `Self::into_raw()`, and they must refrain from using the pointer any
    /// further.
    unsafe fn from_raw(this: *mut capi::b1_handle) -> Arc<Self> {
        // SAFETY: Caller guarantees `this` is from `Self::into_raw()`.
        unsafe { Arc::from_raw(this.cast::<Self>()) }
    }

    /// Borrow a raw reference.
    ///
    /// ## Safety
    ///
    /// The caller must guarantee this pointer was acquired via
    /// `Self::into_raw()`, and they must refrain from releasing it via
    /// `Self::from_raw()` for `'a`.
    unsafe fn borrow_raw<'a>(
        this: *mut capi::b1_handle,
    ) -> ArcBorrow<'a, Self> {
        // SAFETY: Caller guarantees `this` is from `Self::into_raw()` and
        // will not be released for `'a`.
        unsafe { ArcBorrow::from_raw(this.cast::<Self>()) }
    }

    fn link(
        self: ArcBorrow<'_, Self>,
    ) {
        let mut node_guard = self.node.inner.lock();
        let (handles,) = node_guard.as_mut().unfold_mut();

        // It is safe to call this multiple times. If the entry is already
        // linked, we simply skip this operation. And the slot cannot be
        // occupied, since we never return `Equal`.
        let _ = handles.try_link_by(
            util::arc_pin(self.into()),
            |v, other| {
                match util::ptr_cmp(
                    &*v.owner.as_arc_borrow(),
                    &*other.owner.as_arc_borrow(),
                ) {
                    v @ core::cmp::Ordering::Less => v,
                    _ => core::cmp::Ordering::Greater,
                }
            },
        );
    }

    fn unlink(
        self: ArcBorrow<'_, Self>,
    ) {
        let mut node_guard = self.node.inner.lock();
        let (handles,) = node_guard.as_mut().unfold_mut();

        handles.try_unlink(util::arc_borrow_pin(self).as_ref());
    }

    fn begin(self: ArcBorrow<'_, Self>) {
        // This is called when the handle owner is ready to make use of the
        // handle. Simply link it into its node, to ensure it will take part
        // in the notification system.
        self.link();
    }

    fn end(&self) {
        // This is called when the handle owner released the handle and no
        // longer manages it. We simply verify that it is unlinked, since
        // a proper handle-release will have done that.
        kernel::warn_on!(self.node_rb.is_linked());
    }
}

impl Message {
    fn with(
        via: Arc<Handle>,
        transfers: KBox<[*mut capi::b1_handle]>,
        shared: Arc<MessageShared>,
    ) -> Result<Arc<Self>, AllocError> {
        Arc::new(
            Self {
                via,
                transfers,
                shared,
                op_rb: rb::Node::new(),
                tx_node: TxNode::new(TxNodeKind::User),
            },
            GFP_KERNEL,
        )
    }
}

impl Drop for Message {
    fn drop(&mut self) {
        for t in &*self.transfers {
            let _ = unsafe { Handle::from_raw(*t) };
        }
    }
}

impl MessageShared {
    fn with(
        data: KVBox<[u8]>,
    ) -> Result<Arc<Self>, AllocError> {
        Arc::new(
            Self {
                data,
            },
            GFP_KERNEL,
        )
    }

    /// Turn the reference into a raw pointer.
    ///
    /// This will leak the reference and any pinned resources, unless the
    /// original object is recreated via `Self::from_raw()`.
    fn into_raw(this: Arc<Self>) -> *mut capi::b1_message_shared {
        Arc::into_raw(this).cast_mut().cast()
    }

    /// Recreate the reference from its raw pointer.
    ///
    /// ## Safety
    ///
    /// The caller must guarantee this pointer was acquired via
    /// `Self::into_raw()`, and they must refrain from using the pointer any
    /// further.
    unsafe fn from_raw(this: *mut capi::b1_message_shared) -> Arc<Self> {
        // SAFETY: Caller guarantees `this` is from `Self::into_raw()`.
        unsafe { Arc::from_raw(this.cast::<Self>()) }
    }

    /// Borrow a raw reference.
    ///
    /// ## Safety
    ///
    /// The caller must guarantee this pointer was acquired via
    /// `Self::into_raw()`, and they must refrain from releasing it via
    /// `Self::from_raw()` for `'a`.
    unsafe fn borrow_raw<'a>(
        this: *mut capi::b1_message_shared,
    ) -> ArcBorrow<'a, Self> {
        // SAFETY: Caller guarantees `this` is from `Self::into_raw()` and
        // will not be released for `'a`.
        unsafe { ArcBorrow::from_raw(this.cast::<Self>()) }
    }
}

impl Op {
    fn with(operator: Arc<Peer>, tx: Arc<Tx>) -> Self {
        Self {
            operator,
            tx,
            messages: rb::Tree::new(),
            nodes: rb::Tree::new(),
            handles: rb::Tree::new(),
        }
    }

    fn new(
        operator: Arc<Peer>,
    ) -> Result<Self, AllocError> {
        Ok(Self::with(operator, Tx::new()?))
    }

    fn unfold_mut(
        self: Pin<&mut Self>,
    ) -> (
        ArcBorrow<'_, Peer>,
        ArcBorrow<'_, Tx>,
        Pin<&mut rb::Tree<rb::node_of!(Arc<Message>, op_rb)>>,
        Pin<&mut rb::Tree<rb::node_of!(Arc<Node>, op_rb)>>,
        Pin<&mut rb::Tree<rb::node_of!(Arc<Handle>, op_rb)>>,
    ) {
        // SAFETY: The trees are structurally pinned.
        unsafe {
            let inner = Pin::into_inner_unchecked(self);
            (
                inner.operator.as_arc_borrow(),
                inner.tx.as_arc_borrow(),
                Pin::new_unchecked(&mut inner.messages),
                Pin::new_unchecked(&mut inner.nodes),
                Pin::new_unchecked(&mut inner.handles),
            )
        }
    }

    fn send_message(
        self: Pin<&mut Self>,
        to: ArcBorrow<'_, Handle>,
        transfers: KBox<[*mut capi::b1_handle]>,
        shared: Arc<MessageShared>,
    ) -> Result<(), MessageError> {
        let msg: Arc<Message>;

        if kernel::warn_on!(!self.operator.owns_handle(&to)) {
            return Err(MessageError::HandleForeign);
        }

        msg = Message::with(to.into(), transfers, shared)?;

        let (_, _, messages, _, _) = self.unfold_mut();
        let r = messages.try_link_last_by(
            util::arc_pin(msg),
            |v, other| {
                util::ptr_cmp(&*v.via.node.owner, &*other.via.node.owner)
            },
        );
        kernel::warn_on!(r.is_err());

        Ok(())
    }

    fn release_node(
        self: Pin<&mut Self>,
        node: ArcBorrow<'_, Node>,
    ) {
        if kernel::warn_on!(!self.operator.owns_node(&node)) {
            return;
        }

        let (_, _, _, nodes, _) = self.unfold_mut();
        if !nodes.contains(&*node) {
            let r = nodes.try_link_by(
                util::arc_pin(node.into()),
                |v, other| util::ptr_cmp(&**v, &*other),
            );
            kernel::warn_on!(r.is_err());
        }
    }

    fn release_handle(
        self: Pin<&mut Self>,
        handle: ArcBorrow<'_, Handle>,
    ) {
        if kernel::warn_on!(!self.operator.owns_handle(&handle)) {
            return;
        }

        let (_, _, _, _, handles) = self.unfold_mut();
        if !handles.contains(&*handle) {
            let r = handles.try_link_last_by(
                util::arc_pin(handle.into()),
                |v, other| {
                    util::ptr_cmp(&*v.node.owner, &*other.node.owner)
                },
            );
            kernel::warn_on!(r.is_err());
        }
    }

    fn commit(self: Pin<&mut Self>) {
        let (
            _,
            tx,
            mut messages,
            mut nodes,
            mut handles,
        ) = self.unfold_mut();

        // Step #1
        //
        // Attach all nodes to the transaction and queue them on the respective
        // receiving peer.

        let mut c = messages.as_mut().cursor_mut_first();
        while let Some(m) = c.get_clone() {
            m.tx_node.set_tx(tx.into());

            let to = m.via.node.owner.as_arc_borrow();
            let txnode = TxNodeRef::new_user(util::arc_unpin(m.clone()));
            if let Err(_) = to.queue.try_link_front(txnode) {
                kernel::warn_on!(true);
                c.try_unlink_and_move_next();
                continue;
            }

            c.move_next();
        }

        let mut c = nodes.as_mut().cursor_mut_first();
        while let Some(n) = c.get_clone() {
            let mut node_guard = n.inner.lock();
            let (handles,) = node_guard.as_mut().unfold_mut();

            let mut c_inner = handles.cursor_mut_first();
            while let Some(h) = c_inner.get_clone() {
                h.release_node.set_tx(tx.into());

                let to = h.owner.as_arc_borrow();
                let txnode = TxNodeRef::new_release_node(util::arc_unpin(h.clone()));
                if let Err(_) = to.queue.try_link_front(txnode) {
                    kernel::warn_on!(true);
                }

                c_inner.move_next();
            }

            drop(node_guard);
            c.move_next();
        }

        let mut c = handles.as_mut().cursor_mut_first();
        while let Some(h) = c.get_clone().map(util::arc_unpin) {
            h.release_handle.set_tx(tx.into());

            let to = h.node.owner.as_arc_borrow();
            let txnode = TxNodeRef::new_release_handle(h.clone());
            if let Err(_) = to.queue.try_link_front(txnode) {
                kernel::warn_on!(true);
                c.try_unlink_and_move_next();
                continue;
            }

            h.as_arc_borrow().unlink();
            c.move_next();
        }

        // Step #2
        //
        // Mark the transaction as committed. From then on, peers might start
        // dequeueing, but no other transaction can jump this one, anymore. The
        // order is then settled.

        tx.committed.store(true, atomic::Relaxed);

        // Step #3
        //
        // With everything queued and committed, we iterate all nodes again and
        // wake the remote peers.

        messages.clear_with(|m| {
            for t in &*m.transfers {
                let b = unsafe { Handle::borrow_raw(*t) };
                b.link();
            }
            m.via.node.owner.queue_committed.add(1, atomic::Relaxed);
            m.via.node.owner.wake();
        });

        nodes.clear_with(|n| {
            let mut node_guard = n.inner.lock();
            let (handles,) = node_guard.as_mut().unfold_mut();

            handles.clear_with(|h| {
                h.owner.queue_committed.add(1, atomic::Relaxed);
                h.owner.wake();
            });
        });

        handles.clear_with(|h| {
            h.node.owner.queue_committed.add(1, atomic::Relaxed);
            h.node.owner.wake();
        });
    }
}

impl Tx {
    fn new(
    ) -> Result<Arc<Self>, AllocError> {
        Arc::new(
            Self {
                committed: atomic::Atomic::new(false),
            },
            GFP_KERNEL,
        )
    }
}

impl TxNode {
    fn new(kind: TxNodeKind) -> Self {
        Self {
            kind,
            peer_link: lll::Node::new(),
            tx: atomic::Atomic::new(0),
        }
    }

    fn tx(&self) -> Option<ArcBorrow<'_, Tx>> {
        // Paired with `Self::set_tx()`. Ensures that previous writes to Tx
        // are visible when loading it.
        let tx_addr = self.tx.load(atomic::Acquire);
        let tx_ptr = tx_addr as *mut Tx;
        let Some(tx_nn) = NonNull::new(tx_ptr) else {
            return None;
        };
        // SAFETY: If `self.tx` is non-NULL, it is a valid Arc and does not
        // change. We borrow `self` for as long as needed, so it cannot vanish
        // while we hand out the ArcBorrow.
        Some(unsafe { ArcBorrow::from_raw(tx_nn.as_ptr()) })
    }

    fn set_tx(&self, tx: Arc<Tx>) {
        let tx_addr = Arc::as_ptr(&tx) as usize;
        if let Ok(_) = self.tx.cmpxchg(
            0,
            tx_addr,
            // Paired with `Self::tx()`. Ensures previous writes to Tx are
            // visible to anyone fetching the Tx.
            atomic::Release,
        ) {
            let _ = Arc::into_raw(tx);
        } else {
            kernel::warn_on!(true);
        }
    }

    fn is_committed(&self) -> bool {
        match self.tx() {
            None => false,
            Some(v) => v.committed.load(atomic::Relaxed),
        }
    }
}

impl core::ops::Drop for TxNode {
    fn drop(&mut self) {
        let tx_addr = self.tx.load(atomic::Acquire);
        let tx_ptr = tx_addr as *mut Tx;
        if let Some(tx_nn) = NonNull::new(tx_ptr) {
            // SAFETY: `self.tx` is either NULL or from `Arc::into_raw()`.
            let _ = unsafe { Arc::from_raw(tx_nn.as_ptr()) };
        }
    }
}

impl TxNodeRef {
    fn new_user(v: Arc<Message>) -> Pin<Self> {
        // SAFETY: `Arc` is always pinned.
        unsafe { core::mem::transmute::<Self, Pin<Self>>(Self::User(v)) }
    }

    fn new_release_node(v: Arc<Handle>) -> Pin<Self> {
        // SAFETY: `Arc` is always pinned.
        unsafe { core::mem::transmute::<Self, Pin<Self>>(Self::ReleaseNode(v)) }
    }

    fn new_release_handle(v: Arc<Handle>) -> Pin<Self> {
        // SAFETY: `Arc` is always pinned.
        unsafe { core::mem::transmute::<Self, Pin<Self>>(Self::ReleaseHandle(v)) }
    }

    fn as_user(this: &Pin<Self>) -> Option<&Message> {
        let inner = unsafe { core::mem::transmute::<&Pin<Self>, &Self>(this) };
        if let Self::User(v) = inner {
            Some(&v)
        } else {
            None
        }
    }

    fn as_release_node(this: &Pin<Self>) -> Option<&Arc<Handle>> {
        let inner = unsafe { core::mem::transmute::<&Pin<Self>, &Self>(this) };
        if let Self::ReleaseNode(v) = inner {
            Some(&v)
        } else {
            None
        }
    }

    fn as_release_handle(this: &Pin<Self>) -> Option<&Arc<Handle>> {
        let inner = unsafe { core::mem::transmute::<&Pin<Self>, &Self>(this) };
        if let Self::ReleaseHandle(v) = inner {
            Some(&v)
        } else {
            None
        }
    }

    fn tx_node(this: &Pin<Self>) -> &TxNode {
        let inner = unsafe { core::mem::transmute::<&Pin<Self>, &Self>(this) };
        match inner {
            Self::User(v) => &v.tx_node,
            Self::ReleaseNode(v) => &v.release_node,
            Self::ReleaseHandle(v) => &v.release_handle,
        }
    }
}

// The incoming queue on `Peer.queue` can take multiple different types as
// nodes. They all use `TxNode` as metadata, but this is embedded in different
// containing types. Thus, the standard `util::intrusive::Link` implementation
// is not applicable. We define our own here, using the reference type
// `TxNodeRef`.
//
// A `TxNodeRef` is an enum containing an `Arc<T>` to the respective containing
// type of the underlying `TxNode`. When acquiring a reference, we validate
// that the enum matches `TxNode.kind`. Given that the kind is a static field,
// it cannot change. Therefore, when releasing (or borrowing) a node, we can
// rely on `TxNode.kind` to know which containing type to convert back to.
//
// SAFETY: Upholds method guarantees.
unsafe impl util::intrusive::Link<lll::Node> for TxNodeRef {
    type Ref = Self;
    type Target = TxNode;

    fn acquire(v: Pin<Self::Ref>) -> NonNull<lll::Node> {
        // SAFETY: `Pin` guarantees layout stability. We do not move out of `v`
        // when accessing the inner pointer.
        let v_inner = unsafe { core::mem::transmute::<Pin<Self::Ref>, Self::Ref>(v) };

        match v_inner {
            Self::User(v) => {
                kernel::warn_on!(v.tx_node.kind != TxNodeKind::User);
                let v_msg = Arc::into_raw(v);
                // SAFETY: `Arc::into_raw()` guarantees that the allocation is
                // valid and we can perform a field projection.
                unsafe {
                    NonNull::new_unchecked(
                        (&raw const (*v_msg).tx_node.peer_link).cast_mut(),
                    )
                }
            },
            Self::ReleaseNode(v) => {
                kernel::warn_on!(v.release_node.kind != TxNodeKind::ReleaseNode);
                let v_handle = Arc::into_raw(v);
                // SAFETY: `Arc::into_raw()` guarantees that the allocation is
                // valid and we can perform a field projection.
                unsafe {
                    NonNull::new_unchecked(
                        (&raw const (*v_handle).release_node.peer_link).cast_mut(),
                    )
                }
            },
            Self::ReleaseHandle(v) => {
                kernel::warn_on!(v.release_handle.kind != TxNodeKind::ReleaseHandle);
                let v_handle = Arc::into_raw(v);
                // SAFETY: `Arc::into_raw()` guarantees that the allocation is
                // valid and we can perform a field projection.
                unsafe {
                    NonNull::new_unchecked(
                        (&raw const (*v_handle).release_handle.peer_link).cast_mut(),
                    )
                }
            },
        }
    }

    unsafe fn release(v: NonNull<lll::Node>) -> Pin<Self::Ref> {
        // SAFETY: Caller guarantees `v` is from `acquire()`, thus must be
        // embedded in a `TxNode` and convertible to a reference.
        let v_txnode = unsafe {
            field::base_of_nn::<field::field_of!(TxNode, peer_link)>(v)
        };
        // SAFETY: Caller guarantees `v` is from `acquire()` and thus
        // convertible to a reference.
        let kind = unsafe { v_txnode.as_ref().kind };

        let r = match kind {
            TxNodeKind::User => {
                // SAFETY: Caller guarantees `v` is from `acquire()`, and thus
                // `kind` has been verified and we can rely on it here.
                let v_msg = unsafe {
                    field::base_of_nn::<field::field_of!(Message, tx_node)>(v_txnode)
                };
                // SAFETY: Caller guarantees `v` is from `acquire()`, thus
                // ultimately from `Arc::into_raw()`.
                unsafe { Self::User(Arc::from_raw(v_msg.as_ptr())) }
            },
            TxNodeKind::ReleaseNode => {
                // SAFETY: Caller guarantees `v` is from `acquire()`, and thus
                // `kind` has been verified and we can rely on it here.
                let v_handle = unsafe {
                    field::base_of_nn::<field::field_of!(Handle, release_node)>(v_txnode)
                };
                // SAFETY: Caller guarantees `v` is from `acquire()`, thus
                // ultimately from `Arc::into_raw()`.
                unsafe { Self::ReleaseNode(Arc::from_raw(v_handle.as_ptr())) }
            },
            TxNodeKind::ReleaseHandle => {
                // SAFETY: Caller guarantees `v` is from `acquire()`, and thus
                // `kind` has been verified and we can rely on it here.
                let v_handle = unsafe {
                    field::base_of_nn::<field::field_of!(Handle, release_handle)>(v_txnode)
                };
                // SAFETY: Caller guarantees `v` is from `acquire()`, thus
                // ultimately from `Arc::into_raw()`.
                unsafe { Self::ReleaseHandle(Arc::from_raw(v_handle.as_ptr())) }
            },
        };

        // SAFETY: `Pin` guarantees layout stability. Since `v` was from
        // `acquire()`, it is pinned.
        unsafe { core::mem::transmute::<Self::Ref, Pin<Self::Ref>>(r) }
    }

    fn project(v: &Self::Target) -> NonNull<lll::Node> {
        NonNull::from_ref(&v.peer_link)
    }

    unsafe fn borrow<'a>(v: NonNull<lll::Node>) -> Pin<&'a Self::Target> {
        // SAFETY: Caller guarantees `v` is from `acquire()`, thus must be
        // embedded in a `TxNode`, pinned, and convertible to a reference.
        unsafe {
            Pin::new_unchecked(
                field::base_of_nn::<
                    field::field_of!(TxNode, peer_link),
                >(v).as_ref(),
            )
        }
    }
}

#[export_name = "b1_peer_new"]
unsafe extern "C" fn peer_new(
    actor: *mut capi::b1_acct_actor,
    waitq: *mut kernel::bindings::wait_queue_head,
) -> *mut capi::b1_peer {
    // SAFETY: Caller guarantees `actor` is valid.
    let actor = unsafe { acct::Actor::borrow_raw(actor) };
    match Peer::new(actor.into(), waitq) {
        Ok(v) => Peer::into_raw(v),
        Err(AllocError) => ENOMEM.to_ptr(),
    }
}

#[export_name = "b1_peer_ref"]
unsafe extern "C" fn peer_ref(
    this: *mut capi::b1_peer,
) -> *mut capi::b1_peer {
    if let Some(this_nn) = core::ptr::NonNull::new(this) {
        // SAFETY: Caller guarantees `this` is valid.
        let this_b = unsafe { Peer::borrow_raw(this_nn.as_ptr()) };
        core::mem::forget(Into::<Arc<Peer>>::into(this_b));
    }
    this
}

#[export_name = "b1_peer_unref"]
unsafe extern "C" fn peer_unref(
    this: *mut capi::b1_peer,
) -> *mut capi::b1_peer {
    if let Some(this_nn) = core::ptr::NonNull::new(this) {
        // SAFETY: Caller guarantees `this` is valid and no longer used.
        let _ = unsafe { Peer::from_raw(this_nn.as_ptr()) };
    }
    core::ptr::null_mut()
}

#[export_name = "b1_peer_begin"]
unsafe extern "C" fn peer_begin(
    this: *mut capi::b1_peer,
) {
    // SAFETY: Caller guarantees `this` is valid.
    let this_b = unsafe { Peer::borrow_raw(this) };
    this_b.begin();
}

#[export_name = "b1_peer_end"]
unsafe extern "C" fn peer_end(
    this: *mut capi::b1_peer,
) {
    // SAFETY: Caller guarantees `this` is valid.
    let this_b = unsafe { Peer::borrow_raw(this) };
    this_b.end();
}

#[export_name = "b1_peer_new_node"]
unsafe extern "C" fn peer_new_node(
    this: *mut capi::b1_peer,
    other: *mut capi::b1_peer,
    handlep: *mut *mut capi::b1_handle,
) -> *mut capi::b1_node {
    // SAFETY: Caller guarantees `this` is valid.
    let this_b = unsafe { Peer::borrow_raw(this) };
    // SAFETY: Caller guarantees `other` is valid.
    let other_b = unsafe { Peer::borrow_raw(other) };
    // SAFETY: Caller guarantees `handlep` is valid.
    let handlep_b = unsafe { &mut *handlep };

    match this_b.create_node(other_b) {
        Ok((n, h)) => {
            *handlep_b = Handle::into_raw(h);
            Node::into_raw(n)
        },
        Err(AllocError) => ENOMEM.to_ptr(),
    }
}

#[export_name = "b1_peer_new_handle"]
unsafe extern "C" fn peer_new_handle(
    this: *mut capi::b1_peer,
    from: *mut capi::b1_handle,
) -> *mut capi::b1_handle {
    // SAFETY: Caller guarantees `this` is valid.
    let this_b = unsafe { Peer::borrow_raw(this) };
    // SAFETY: Caller guarantees `from` is valid.
    let from_b = unsafe { Handle::borrow_raw(from) };

    match this_b.create_handle(from_b) {
        Ok(v) => Handle::into_raw(v),
        Err(AllocError) => ENOMEM.to_ptr(),
    }
}

#[export_name = "b1_peer_readable"]
unsafe extern "C" fn peer_readable(
    this: *mut capi::b1_peer,
) -> bool {
    // SAFETY: Caller guarantees `this` is valid.
    let this_b = unsafe { Peer::borrow_raw(this) };
    this_b.readable()
}

#[export_name = "b1_peer_peek"]
unsafe extern "C" fn peer_peek(
    this: *mut capi::b1_peer,
    peek: *mut capi::b1_peer_peek,
) -> bool {
    // SAFETY: Caller guarantees `this` is valid.
    let this_b = unsafe { Peer::borrow_raw(this) };
    // SAFETY: Caller guarantees `peek` is valid.
    let peek_b = unsafe { &mut *peek };
    this_b.peek(peek_b)
}

#[export_name = "b1_peer_pop"]
unsafe extern "C" fn peer_pop(
    this: *mut capi::b1_peer,
) {
    // SAFETY: Caller guarantees `this` is valid.
    let this_b = unsafe { Peer::borrow_raw(this) };
    this_b.pop();
}

#[export_name = "b1_node_ref"]
unsafe extern "C" fn node_ref(
    this: *mut capi::b1_node,
) -> *mut capi::b1_node {
    if let Some(this_nn) = core::ptr::NonNull::new(this) {
        // SAFETY: Caller guarantees `this` is valid.
        let this_b = unsafe { Node::borrow_raw(this_nn.as_ptr()) };
        core::mem::forget(Into::<Arc<Node>>::into(this_b));
    }
    this
}

#[export_name = "b1_node_unref"]
unsafe extern "C" fn node_unref(
    this: *mut capi::b1_node,
) -> *mut capi::b1_node {
    if let Some(this_nn) = core::ptr::NonNull::new(this) {
        // SAFETY: Caller guarantees `this` is valid and no longer used.
        let _ = unsafe { Node::from_raw(this_nn.as_ptr()) };
    }
    core::ptr::null_mut()
}

#[export_name = "b1_node_get_userdata"]
unsafe extern "C" fn node_get_userdata(
    this: *mut capi::b1_node,
) -> *mut kernel::ffi::c_void {
    // SAFETY: Caller guarantees `this` is valid.
    let this_b = unsafe { Node::borrow_raw(this) };
    this_b.userdata.load(atomic::Relaxed) as *mut kernel::ffi::c_void
}

#[export_name = "b1_node_set_userdata"]
unsafe extern "C" fn node_set_userdata(
    this: *mut capi::b1_node,
    userdata: *mut kernel::ffi::c_void,
) {
    // SAFETY: Caller guarantees `this` is valid.
    let this_b = unsafe { Node::borrow_raw(this) };
    this_b.userdata.store(userdata as usize, atomic::Relaxed);
}

#[export_name = "b1_node_begin"]
unsafe extern "C" fn node_begin(
    this: *mut capi::b1_node,
) {
    // SAFETY: Caller guarantees `this` is valid.
    let this_b = unsafe { Node::borrow_raw(this) };
    this_b.begin();
}

#[export_name = "b1_node_end"]
unsafe extern "C" fn node_end(
    this: *mut capi::b1_node,
) {
    // SAFETY: Caller guarantees `this` is valid.
    let this_b = unsafe { Node::borrow_raw(this) };
    this_b.end();
}

#[export_name = "b1_handle_ref"]
unsafe extern "C" fn handle_ref(
    this: *mut capi::b1_handle,
) -> *mut capi::b1_handle {
    if let Some(this_nn) = core::ptr::NonNull::new(this) {
        // SAFETY: Caller guarantees `this` is valid.
        let this_b = unsafe { Handle::borrow_raw(this_nn.as_ptr()) };
        core::mem::forget(Into::<Arc<Handle>>::into(this_b));
    }
    this
}

#[export_name = "b1_handle_unref"]
unsafe extern "C" fn handle_unref(
    this: *mut capi::b1_handle,
) -> *mut capi::b1_handle {
    if let Some(this_nn) = core::ptr::NonNull::new(this) {
        // SAFETY: Caller guarantees `this` is valid and no longer used.
        let _ = unsafe { Handle::from_raw(this_nn.as_ptr()) };
    }
    core::ptr::null_mut()
}

#[export_name = "b1_handle_get_userdata"]
unsafe extern "C" fn handle_get_userdata(
    this: *mut capi::b1_handle,
) -> *mut kernel::ffi::c_void {
    // SAFETY: Caller guarantees `this` is valid.
    let this_b = unsafe { Handle::borrow_raw(this) };
    this_b.userdata.load(atomic::Relaxed) as *mut kernel::ffi::c_void
}

#[export_name = "b1_handle_set_userdata"]
unsafe extern "C" fn handle_set_userdata(
    this: *mut capi::b1_handle,
    userdata: *mut kernel::ffi::c_void,
) {
    // SAFETY: Caller guarantees `this` is valid.
    let this_b = unsafe { Handle::borrow_raw(this) };
    this_b.userdata.store(userdata as usize, atomic::Relaxed);
}

#[export_name = "b1_handle_begin"]
unsafe extern "C" fn handle_begin(
    this: *mut capi::b1_handle,
) {
    // SAFETY: Caller guarantees `this` is valid.
    let this_b = unsafe { Handle::borrow_raw(this) };
    this_b.begin();
}

#[export_name = "b1_handle_end"]
unsafe extern "C" fn handle_end(
    this: *mut capi::b1_handle,
) {
    // SAFETY: Caller guarantees `this` is valid.
    let this_b = unsafe { Handle::borrow_raw(this) };
    this_b.end();
}

#[export_name = "b1_message_shared_new"]
unsafe extern "C" fn message_shared_new(
    n_data: u64,
    data: *mut kernel::ffi::c_void,
) -> *mut capi::b1_message_shared {
    let data_v = core::ptr::slice_from_raw_parts_mut::<u8>(
        data.cast(),
        n_data as usize,
    );

    // SAFETY: Caller guarantees `data` is an owned KVBox of length `n_data`.
    let data_kv = unsafe { KVBox::from_raw(data_v) };

    match MessageShared::with(data_kv) {
        Ok(v) => MessageShared::into_raw(v),
        Err(AllocError) => ENOMEM.to_ptr(),
    }
}

#[export_name = "b1_message_shared_ref"]
unsafe extern "C" fn message_shared_ref(
    this: *mut capi::b1_message_shared,
) -> *mut capi::b1_message_shared {
    if let Some(this_nn) = core::ptr::NonNull::new(this) {
        // SAFETY: Caller guarantees `this` is valid.
        let this_b = unsafe { MessageShared::borrow_raw(this_nn.as_ptr()) };
        core::mem::forget(Into::<Arc<MessageShared>>::into(this_b));
    }
    this
}

#[export_name = "b1_message_shared_unref"]
unsafe extern "C" fn message_shared_unref(
    this: *mut capi::b1_message_shared,
) -> *mut capi::b1_message_shared {
    if let Some(this_nn) = core::ptr::NonNull::new(this) {
        // SAFETY: Caller guarantees `this` is valid and no longer used.
        let _ = unsafe { MessageShared::from_raw(this_nn.as_ptr()) };
    }
    core::ptr::null_mut()
}

#[export_name = "b1_op_new"]
unsafe extern "C" fn op_new(
    peer: *mut capi::b1_peer,
) -> *mut capi::b1_op {
    // SAFETY: Caller guarantees `peer` is valid.
    let peer_b = unsafe { Peer::borrow_raw(peer) };

    let op = match Op::new(peer_b.into()) {
        Ok(v) => v,
        Err(AllocError) => return ENOMEM.to_ptr(),
    };

    let op_box = match KBox::pin(op, GFP_KERNEL) {
        Ok(v) => v,
        Err(AllocError) => return ENOMEM.to_ptr(),
    };

    // SAFETY: `capi::b1_op` is treated as pinned.
    let op_nopin = unsafe { Pin::into_inner_unchecked(op_box) };
    KBox::into_raw(op_nopin).cast()
}

#[export_name = "b1_op_free"]
unsafe extern "C" fn op_free(
    this: *mut capi::b1_op,
) -> *mut capi::b1_op {
    if let Some(this_nn) = core::ptr::NonNull::new(this) {
        // SAFETY: Caller guarantees `this` is valid, pinned and no longer used.
        let _ = unsafe {
            Pin::new_unchecked(
                KBox::from_raw(this_nn.as_ptr().cast::<Op>())
            )
        };
    }
    core::ptr::null_mut()
}

#[export_name = "b1_op_send_message"]
unsafe extern "C" fn op_send_message(
    this: *mut capi::b1_op,
    to: *mut capi::b1_handle,
    n_transfers: u64,
    transfers: *mut *mut capi::b1_handle,
    shared: *mut capi::b1_message_shared,
) -> kernel::ffi::c_int {
    // SAFETY: Caller guarantees `this` is valid and pinned.
    let this_b = unsafe { Pin::new_unchecked(&mut *this.cast::<Op>()) };
    // SAFETY: Caller guarantees `to` is valid.
    let to_b = unsafe { Handle::borrow_raw(to) };
    // Caller guarantees `n_transfers` represents an allocation.
    let n_transfers_sz = n_transfers as usize;
    let transfers_p = core::ptr::slice_from_raw_parts_mut::<*mut capi::b1_handle>(
        transfers.cast(),
        n_transfers_sz,
    );
    // SAFETY: Caller guarantees `transfers` is a valid slice with `n_transfers`
    // handle references.
    let transfers_v = unsafe { &*transfers_p };
    // SAFETY: Caller guarantees `shared` is valid.
    let shared_b = unsafe { MessageShared::borrow_raw(shared) };

    let Ok(mut xfers_vec) = KVec::with_capacity(n_transfers_sz, GFP_KERNEL) else {
        return ENOMEM.to_errno();
    };

    for t in transfers_v {
        // SAFETY: Caller guarantees `transfers` has valid handles.
        let handle = unsafe { Handle::borrow_raw(*t) };
        if kernel::warn_on!(!this_b.operator.owns_handle(&handle)) {
            return ENOTRECOVERABLE.to_errno();
        }

        let Ok(new) = Handle::new(handle.node.clone(), to_b.node.owner.clone()) else {
            return ENOMEM.to_errno();
        };

        let Ok(_) = xfers_vec.push(Handle::into_raw(new), GFP_KERNEL) else {
            return ENOMEM.to_errno();
        };
    }

    let Ok(xfers_box) = xfers_vec.into_boxed_slice() else {
        return ENOMEM.to_errno();
    };

    match this_b.send_message(to_b, xfers_box, shared_b.into()) {
        Ok(()) => 0,
        Err(MessageError::Alloc(AllocError)) => ENOMEM.to_errno(),
        Err(MessageError::HandleForeign) => ENOTRECOVERABLE.to_errno(),
    }
}

#[export_name = "b1_op_release_node"]
unsafe extern "C" fn op_release_node(
    this: *mut capi::b1_op,
    node: *mut capi::b1_node,
) {
    // SAFETY: Caller guarantees `this` is valid and pinned.
    let this_b = unsafe { Pin::new_unchecked(&mut *this.cast::<Op>()) };
    // SAFETY: Caller guarantees `node` is valid.
    let node_b = unsafe { Node::borrow_raw(node) };

    this_b.release_node(node_b);
}

#[export_name = "b1_op_release_handle"]
unsafe extern "C" fn op_release_handle(
    this: *mut capi::b1_op,
    handle: *mut capi::b1_handle,
) {
    // SAFETY: Caller guarantees `this` is valid and pinned.
    let this_b = unsafe { Pin::new_unchecked(&mut *this.cast::<Op>()) };
    // SAFETY: Caller guarantees `handle` is valid.
    let handle_b = unsafe { Handle::borrow_raw(handle) };

    this_b.release_handle(handle_b);
}

#[export_name = "b1_op_commit"]
unsafe extern "C" fn op_commit(
    this: *mut capi::b1_op,
) {
    // SAFETY: Caller guarantees `this` is valid, pinned and no longer used.
    let mut this_o = unsafe {
        Pin::new_unchecked(KBox::from_raw(this.cast::<Op>()))
    };
    this_o.as_mut().commit();
}
