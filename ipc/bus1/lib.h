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
#include <uapi/linux/bus1.h>

typedef __u32 b1_acct_id_t;
typedef __u64 b1_acct_value_t;

struct b1_acct;
struct b1_acct_actor;
struct b1_acct_charge;
struct b1_acct_trace;
struct b1_acct_user;

/* accounting */

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

#endif /* __B1_LIB_H */
