// SPDX-License-Identifier: GPL-2.0
#ifndef __B1_LIB_H
#define __B1_LIB_H

/**
 * DOC: C API of the Bus1 Rust Module
 *
 * This header exposes the C API of the Bus1 rust module. It provides the
 * necessary hooks for the C module code to call into the rust code.
 */

#include <linux/cleanup.h>
#include <linux/err.h>
#include <linux/errno.h>
#include <linux/types.h>
#include <linux/wait.h>

typedef __u32 b1_acct_id_t;
typedef __u64 b1_acct_value_t;

struct b1_acct;
struct b1_acct_actor;
struct b1_acct_charge;
struct b1_acct_trace;
struct b1_acct_user;
struct b1_handle;
struct b1_node;
struct b1_op;
struct b1_peer;

/* accounting */

static const int B1_ACCT_ERROR_INVALID = EINVAL;
static const int B1_ACCT_ERROR_OOM = ENOMEM;
static const int B1_ACCT_ERROR_USER_QUOTA = EDQUOT;
static const int B1_ACCT_ERROR_ACTOR_QUOTA = EXFULL;

enum: size_t {
	B1_ACCT_SLOT_OBJECTS,
	B1_ACCT_SLOT_BYTES,
	_B1_ACCT_SLOT_N,
};

struct b1_acct_charge {
	struct b1_acct_trace *trace;
	b1_acct_value_t amount[_B1_ACCT_SLOT_N];
};

#define B1_ACCT_CHARGE_INIT() ((struct b1_acct_charge){})

struct b1_acct *b1_acct_new(const b1_acct_value_t (*maxima)[_B1_ACCT_SLOT_N]);
struct b1_acct *b1_acct_ref(struct b1_acct *acct);
struct b1_acct *b1_acct_unref(struct b1_acct *acct);

struct b1_acct_actor *b1_acct_actor_new(struct b1_acct_user *user);
struct b1_acct_actor *b1_acct_actor_ref(struct b1_acct_actor *actor);
struct b1_acct_actor *b1_acct_actor_unref(struct b1_acct_actor *actor);

int b1_acct_actor_charge(
	struct b1_acct_actor *actor,
	struct b1_acct_charge *charge,
	const b1_acct_value_t (*amount)[_B1_ACCT_SLOT_N]
);

struct b1_acct_user *b1_acct_get_user(struct b1_acct *acct, b1_acct_id_t id);
struct b1_acct_user *b1_acct_user_ref(struct b1_acct_user *user);
struct b1_acct_user *b1_acct_user_unref(struct b1_acct_user *user);

void b1_acct_charge_init(struct b1_acct_charge *charge);
void b1_acct_charge_deinit(struct b1_acct_charge *charge);

DEFINE_FREE(
	b1_acct_unref,
	struct b1_acct *,
	if (!IS_ERR_OR_NULL(_T))
		b1_acct_unref(_T);
)

DEFINE_FREE(
	b1_acct_actor_unref,
	struct b1_acct_actor *,
	if (!IS_ERR_OR_NULL(_T))
		b1_acct_actor_unref(_T);
)

DEFINE_FREE(
	b1_acct_user_unref,
	struct b1_acct_user *,
	if (!IS_ERR_OR_NULL(_T))
		b1_acct_user_unref(_T);
)

/* peer */

struct b1_peer_peek {
	u64 type;
	union {
		struct b1_peer_peek_user {
			struct b1_node *node;
			u64 n_transfers;
			struct b1_handle **transfers;
			u64 n_data;
			void *data;
		} user;
		struct b1_peer_peek_node_release {
			struct b1_handle *handle;
		} node_release;
		struct b1_peer_peek_handle_release {
			struct b1_node *node;
		} handle_release;
	};
};

struct b1_peer *b1_peer_new(struct b1_acct_actor *actor, wait_queue_head_t *waitq);
struct b1_peer *b1_peer_ref(struct b1_peer *peer);
struct b1_peer *b1_peer_unref(struct b1_peer *peer);

void b1_peer_begin(struct b1_peer *peer);
void b1_peer_end(struct b1_peer *peer);

struct b1_node *b1_peer_new_node(struct b1_peer *peer, struct b1_peer *other, struct b1_handle **handlep);
struct b1_handle *b1_peer_new_handle(struct b1_peer *peer, struct b1_handle *from);

bool b1_peer_readable(struct b1_peer *peer);
bool b1_peer_peek(struct b1_peer *peer, struct b1_peer_peek *peek);
void b1_peer_pop(struct b1_peer *peer);

DEFINE_FREE(
	b1_peer_unref,
	struct b1_peer *,
	if (!IS_ERR_OR_NULL(_T))
		b1_peer_unref(_T);
)

/* node */

struct b1_node *b1_node_ref(struct b1_node *node);
struct b1_node *b1_node_unref(struct b1_node *node);

void *b1_node_get_userdata(struct b1_node *node);
void b1_node_set_userdata(struct b1_node *node, void *userdata);
void b1_node_begin(struct b1_node *node);
void b1_node_end(struct b1_node *node);

DEFINE_FREE(
	b1_node_unref,
	struct b1_node *,
	if (!IS_ERR_OR_NULL(_T))
		b1_node_unref(_T);
)

/* handle */

struct b1_handle *b1_handle_ref(struct b1_handle *handle);
struct b1_handle *b1_handle_unref(struct b1_handle *handle);

void *b1_handle_get_userdata(struct b1_handle *handle);
void b1_handle_set_userdata(struct b1_handle *handle, void *userdata);
void b1_handle_begin(struct b1_handle *handle);
void b1_handle_end(struct b1_handle *handle);

DEFINE_FREE(
	b1_handle_unref,
	struct b1_handle *,
	if (!IS_ERR_OR_NULL(_T))
		b1_handle_unref(_T);
)

/* message_shared */

struct b1_message_shared *b1_message_shared_new(void *kvdata);
struct b1_message_shared *b1_message_shared_unref(struct b1_message_shared *shared);

/* op */

struct b1_op *b1_op_new(struct b1_peer *peer);
struct b1_op *b1_op_free(struct b1_op *op);

int b1_op_send_message(
	struct b1_op *op,
	size_t n_transfers,
	struct b1_handle **transfers,
	struct b1_message_shared *shared
);

void b1_op_release_node(struct b1_op *op, struct b1_node *node);
void b1_op_release_handle(struct b1_op *op, struct b1_handle *handle);

void b1_op_commit(struct b1_op *op);

DEFINE_FREE(
	b1_op_free,
	struct b1_op *,
	if (!IS_ERR_OR_NULL(_T))
		b1_op_free(_T);
)

#endif /* __B1_LIB_H */
