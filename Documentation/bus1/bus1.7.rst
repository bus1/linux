====
bus1
====

----------------------------------------------
Capability-based IPC for Linux
----------------------------------------------

:Manual section: 7
:Manual group: Miscellaneous

SYNOPSIS
========

| ``#include <linux/bus1.h>``

DESCRIPTION
-----------

The bus1 API provides capability-based inter-process communication. Its core
primitive is a multi-producer/single-consumer unidirectional channel that can
transmit arbitrary user messages. The receiving end of the channel is called
a **node**, while the sending end is called a **handle**.

A handle always refers to exactly one node, but there can be many handles
referring to the same node, and those handles can be held by independent
owners. Messages are sent via a handle, meaning it is transmitted to the node
the handle is linked to. A handle to a node is required to transmit a message
to that node.

A sender can attach copies of any handle they hold to a message, and thus
transfer them alongside the message. The copied handles refer to the same node
as their respective original handle.

All nodes and handles have an owning **peer**. A peer is a purely local
concept. The owning peer of a node or handle never affects the externally
visible behavior of them. However, all nodes and handles of a single peer share
a message queue.

When the last handle to a node is released, the owning peer of the node
receives a notification. Similarly, if a node is released, the owning peers of
all handles referring to that node receive a notification. All notifications
are ordered causally with any other ongoing communication.

Communication on the bus happens via transactions. A transaction is an atomic
transmission of messages, which can include release notifications. All message
types can be part of a transaction, and thus can happen atomically with any
other kind of message. A transaction with only a single message or notification
is called a unicast. Any other transaction is called a multicast.

Transactions are causally ordered. That is, if any transaction is a reaction to
any previous transaction, all messages of the reaction transaction will be
received by any peer after the messages that were part of the original
transaction. This is even guaranteed if the causal relationship exists only via
a side-channel outside the scope of bus1. However, messages without causal
relationship have no stable order. This is especially noticeable with
multicasts, where receivers might see independent multicasts in a different
order.

Operations
----------

The user-space API of bus1 is not decided on. This section describes the
available operations as system calls, as they likely would be exposed by any
user-space library. However, for development reasons the actual user-space API
is currently performed via ioctls on a character device.

Peer Creation
^^^^^^^^^^^^^

| ``int bus1_peer_new();``

Peers are independent entities that can be created at will. They are accessed
via file-descriptors, with each peer having its own file-description. Multiple
file-descriptors can refer to the same peer, yet currently all operations will
lock a peer and thus serialize all operations on that peer.

Once the last file-descriptor referring to a peer is closed, the peer is
released. Any resources of that peer are released, and any ongoing transactions
targetting the peer will discard their messages.

File descriptions pin the credentials of the calling process. A peer will use
those pinned credentials for resource accounting. Otherwise, no ambient
resources are used by bus1.

Transfer Command
^^^^^^^^^^^^^^^^

| ``#define BUS1_TRANSFER_FLAG_CREATE 0x1``
|
| ``struct bus1_transfer {``
|         ``uint64_t flags;``
|         ``uint64_t id;``
| ``};``
|
| ``int bus1_cmd_transfer(``
|         ``uint64_t flags,``
|         ``int from,``
|         ``int to,``
|         ``size_t n,``
|         ``struct bus1_transfer *src,``
|         ``struct bus1_transfer *dst``
| ``);``

A transfer command can be used for two different operations. First, it can be
used to create nodes and handles on a peer. Second, it can be used to transfer
a handle from one peer to another, while holding file-descriptors to both
peers.

The command takes ``flags``, which currently is unused and must be 0. ``from``
and ``to`` are file-descriptors referring to the involved peers. ``from`` must
be provided, while ``to`` can be ``-1``, in which case it will refer to the
same peer as ``from``.

``n`` defines the number of transfer operations that are performed atomically.
``src`` and ``dst`` must refer to arrays with ``n`` elements. ``dst`` can be
uninitialized, and will be filled in by the kernel. ``src`` must be initialized
by the caller. ``src[i].flags`` must be 0 or ``BUS1_TRANSFER_FLAG_CREATE``.
``src[i].id`` must refer to an ID of a handle in ``from``. If
``BUS1_TRANSFER_FLAG_CREATE`` is set, ``src[i].id`` must be set to
``BUS1_INVALID``. In this case a new node is create and the ID of the node
is returned in ``src[i].id`` with ``src[i].flags`` cleared to 0.

In any case, a new handle in ``to`` is created for every provided transfer. Its
ID is returned in ``dst[i].id`` and ``dst[i].flags`` is set to 0.

Note that both arrays ``src`` and ``dst`` can be partially modified by the
kernel even if the operation fails (even if it fails with a different error
than ``EFAULT``).

Release Command
^^^^^^^^^^^^^^^

| ``int bus1_cmd_release(``
|         ``int peerfd,``
|         ``size_t n_ids,``
|         ``uint64_t *ids``
| ``);``

A release command takes a peer file-descriptor as ``peerfd`` and an array of
node and handle IDs as ``ids`` with ``n_ids`` number of elements. All these
nodes and handles will be released in a single atomic transaction.

The command does not fail, except if invalid arguments are provided.

No subsequent operation on this peer will refer to the IDs once this call
returns. Furthermore, those IDs will never be reused.

Send Command
^^^^^^^^^^^^

| ``enum bus1_message_type: uint64_t {``
|         ``BUS1_MESSAGE_TYPE_USER = 0,``
|         ``BUS1_MESSAGE_TYPE_NODE_RELEASE = 1,``
|         ``BUS1_MESSAGE_TYPE_HANDLE_RELEASE = 2,``
|         ``_BUS1_MESSAGE_TYPE_N,``
| ``}``
|
| ``struct bus1_message {``
|         ``uint64_t flags;``
|         ``uint64_t type;``
|         ``uint64_t n_transfers;   // size_t n_transfers``
|         ``uint64_t ptr_transfers; // struct bus1_transfer *transfers;``
|         ``uint64_t n_data;        // size_t n_data;``
|         ``uint64_t n_data_vecs;   // size_t n_data_vecs;``
|         ``uint64_t ptr_data_vecs; // struct iovec *data_vecs;``
| ``};``
|
| ``int bus1_cmd_send(``
|         ``int peerfd,``
|         ``size_t n_destinations,``
|         ``uint64_t *destinations,``
|         ``int32_t *errors,``
|         ``struct bus1_message *message``
| ``);``

The send command takes a peer file-descriptor as ``peerfd``, the message to
send as ``message``, and an array of destination handles as ``destinations``
(with ``n_destinations`` number of elements).

Additionally, ``errors`` is used to return the individual error code for each
destination. This is only done if the send command returns success. Since
currently partial failure is not exposed, ``errors[i]`` is currently always
set to 0 on success.

All destination IDs must refer to a valid handle of the calling peer.
``EBADRQC`` is returned if an ID did not refer to an handle. Currently, only
a single message can be provided with a single send command, and this message
is transmitted to all destinations in a single atomic transaction.

The message to be transmitted is provided as ``message``. This structure
describes the payload of the message. ``message.flags`` must be 0.
``message.type`` must be ``BUS1_MESSAGE_TYPE_USER``. ``message.n_transfers``
and ``message.ptr_transfers`` refer to an array of ``struct bus1_transfer``
and describe handles to be transferred with the message. The transfers are
used the same as in ``bus1_cmd_transfer(2)``, but ``BUS1_TRANSFER_FLAG_CREATE``
is currently not refused.

``message.n_data_vecs`` and ``message.ptr_data_vecs`` provide the iovecs with
the data to be transmitted with the message. Only the first ``message.n_data``
bytes of the iovecs are considered part of the message. Any trailing bytes
are ignored. The data is copied into kernel buffers and the iovecs are no
longer accessed once the command returns.

Recv Command
^^^^^^^^^^^^

| ``struct bus1_metadata {``
|         ``uint64_t flags;``
|         ``uint64_t id;``
|         ``uint64_t account;``
| ``};``
|
| ``int bus1_cmd_recv(``
|         ``int peerfd,``
|         ``struct bus1_metadata *metadata,``
|         ``struct bus1_message *message``
| ``);``

The recv command takes a peer file-descriptor as ``peerfd`` and fetches the
next message from its queue. If no message is queued ``EAGAIN`` is returned.

The message is returned in ``message``. The caller must set ``message.flags``
to 0 and ``message.type`` to ``BUS1_INVALID``. ``message.n_transfers`` and
``message.ptr_transfers`` refer to an array of ``struct bus1_transfer``
structures used to return the transferred handles of the next message. Upon
return, ``message.n_transfers`` is updated to the actually transferred number
of handles, while ``message.transfers[i]`` is updated as described in
``bus1_cmd_transfer(2)``.

``message.n_data``, ``message.n_data_vecs``, and ``message.ptr_data_vecs``
must be initialized by the caller and provide the space to store the data of
the next message. The iovecs are never modified by the operation.

If the message would exceed ``message.n_transfers`` or ``message.n_data``,
``EMSGSIZE`` is returned and the fields are updated accordingly.

Upon success, ``message`` is updated with data of the received message, with
transferred handles and data written to the transfer array and iovecs.

``metadata`` is updated to contain more data about the message.
``metadata.flags`` is unused and set to 0. ``metadata.id`` contains the ID
of the node the message was received on (or the ID of the handle in case of
``BUS1_MESSAGE_TYPE_NODE_RELEASE``). ``metadata.account`` contains the ID
of the resource context of the sender.

Errors
------

All operations follow a strict error reporting model. If an operation has a
documented error case, then this will be indicated to user-space with a
negative return value (or ``errno`` respectively). Whenever an error appears,
the operation will have been cancelled entirely and have no observable affect
on the bus. User space can safely assume the system to be in the same state as
if the operation was not invoked, unless explicitly documented.

One major exception is ``EFAULT``. The ``EFAULT`` error code is returned
whenever user-space supplied malformed pointers to the kernel, and the kernel
was unable to fetch information from, or return information to, user-space.
This indicates a misbehaving client, and usually there is no way to recover
from this, unless user-space intentionally triggered this behavior. User-space
should treat ``EFAULT`` as an assertion failure and not try to recover. If the
bus1 API is used in a correct manner, ``EFAULT`` will never be returned by any
operation.

Resource Accounting
-------------------

Every peer has an associated resource context used to account claimed
resources. This resource context is determined at the time the peer is created
and it will never change over its lifetime. The default, and at this time only,
accounting model is based on UNIX ``UIDs``. That is, each peer gets assigned
the resource-context of the ``Effective UID`` of the process that creates it.
From then on any resource consumption of the peer is accounted on this
resource-context, and thus shared with all other peers of the same ``UID``.

All allocations have upper limits which cannot be exceeded. An operation will
return ``EDQUOT`` if the quota limits prevent an operation from being
performed. User-space is expected to treat this as an administration or
configuration error, since there is generally no meaningful way to recover.
Applications should expect to be spawned with suitable resource limits
pre-configured. However, this is not enforced and user-space is free to react
to ``EDQUOT`` as it wishes.

Unlike all other bus properties, resource accounting is not part of the bus
atomicity and ordering guarantees, nor does it implement strict rollback. This
means, if an operation allocates multiple resources, the resource counters are
updated before the operation will happen on the bus. Hence, the resource
counter modifications are visible to the system before the operation itself is.
Furthermore, while any failing operation will correctly revert any temporary
resource allocations, the allocations will have been visible to the system
for the time of this (failed) operation. Therefore, even a failed operation
can have (temporary) visible side-effects. But similar to the atomicity
guarantees, these do not affect any other bus properties, but only the resource
accounting.

However, note that monitoring of bus accounting is not considered a
programmatic interface, nor are any explicit accounting APIs exposed. Thus, the
only visible effect of resource accounting is getting ``EDQUOT`` if a counter
is exceeded.

Additionally to standard resource accounting, a peer can also allocate remote
resources. This happens whenever a transaction transmits resources from
a sender to a receiver. All such transactions are always accounted on the
receiver at the time of *send*. To prevent senders from exhausting resources
of a receiver, a peer only ever gets access to a subset of the resources of any
other resource-context that does not match its own.

The exact quotas are
calculated at runtime and dynamically adapt to the number of different users
that currently partake. The ideal is a fair linear distribution of the
available resources, and the algorithm guarantees a quasi-linear distribution.
Yet, the details are implementation specific and can change over time.

Additionally, a second layer resource accounting separates peers of the same
resource context. This is done to prevent malfunctioning peers from exceeding
all resources of their resource context, and thus affecting other peers with
the same resource context. This uses a much less strict quota system, since
it does not span security domains.
