// SPDX-License-Identifier: GPL-2.0
#define pr_fmt(fmt) KBUILD_MODNAME ": " fmt
#include <linux/cleanup.h>
#include <linux/container_of.h>
#include <linux/cred.h>
#include <linux/err.h>
#include <linux/file.h>
#include <linux/fs.h>
#include <linux/init.h>
#include <linux/miscdevice.h>
#include <linux/mutex.h>
#include <linux/poll.h>
#include <linux/rbtree.h>
#include <linux/slab.h>
#include <linux/stat.h>
#include <linux/uidgid.h>
#include <linux/uio.h>
#include <linux/wait.h>
#include <uapi/linux/bus1.h>
#include "cdev.h"
#include "lib.h"

enum b1_uobject_type: unsigned int {
	_B1_UOBJECT_INVALID,
	B1_UOBJECT_NODE,
	B1_UOBJECT_HANDLE,
};

struct b1_uobject {
	unsigned int type;
	u64 id;

	union {
		struct b1_node *node;
		struct b1_handle *handle;
	};

	struct rb_node upeer_rb;
	struct list_head op_link;
};

struct b1_upeer {
	struct b1_peer *peer;
	wait_queue_head_t waitq;

	struct mutex lock;
	struct rb_root objects;
	u64 id_allocator;
};

struct b1_cdev {
	struct b1_acct *acct;
	struct miscdevice misc;
};

/*
 * Verify details of the bus1-cdev-api required by this implementation:
 * - `BUS1_INVALID` must be of type `u64`.
 * - `BUS1_INVALID` must be marked as `managed`, to guarantee it does not
 *   conflict with the unmanaged namespace.
 * - `BUS1_INVALID` must match `(u64)-1` to ensure the id-allocator will never
 *   accidentally return it.
 * - `BUS1_MANAGED` must be `(u64)1` to ensure it can be set/cleared via LSB
 *   and does not conflict with 2-aligned user-space pointers.
 */
static_assert(sizeof(BUS1_INVALID) == sizeof(u64));
static_assert(__alignof(BUS1_INVALID) == __alignof(u64));
static_assert(!!(BUS1_INVALID & BUS1_MANAGED));
static_assert(BUS1_INVALID == ~(u64)0);
static_assert(BUS1_MANAGED == (u64)1);

/*
 * Lock two mutices of the same class with a lock-order according to their
 * memory address. If either mutex is NULL, it is not locked. If both refer to
 * the same mutex, only one is locked.
 */
static void lock2(struct mutex *a, struct mutex *b)
{
	if (a < b) {
		if (a)
			mutex_lock(a);
		if (b && b != a)
			mutex_lock_nested(b, !!a);
	} else {
		if (b)
			mutex_lock(b);
		if (a && a != b)
			mutex_lock_nested(a, !!b);
	}
}

/* Inverse operation of `lock2()`. */
static void unlock2(struct mutex *a, struct mutex *b)
{
	if (a)
		mutex_unlock(a);
	if (b && b != a)
		mutex_unlock(b);
}

static struct b1_uobject *b1_uobject_new(u64 id)
{
	struct b1_uobject *uobject;

	uobject = kzalloc_obj(struct b1_uobject);
	if (!uobject)
		return ERR_PTR(-ENOMEM);

	uobject->id = id;
	RB_CLEAR_NODE(&uobject->upeer_rb);
	INIT_LIST_HEAD(&uobject->op_link);

	return uobject;
}

static struct b1_uobject *
b1_uobject_free(struct b1_uobject *uobject)
{
	if (uobject) {
		WARN_ON(!list_empty(&uobject->op_link));
		WARN_ON(!RB_EMPTY_NODE(&uobject->upeer_rb));

		switch (uobject->type) {
		case B1_UOBJECT_NODE:
			if (uobject->node) {
				b1_node_set_userdata(uobject->node, NULL);
				uobject->node = b1_node_unref(uobject->node);
			}
			break;
		case B1_UOBJECT_HANDLE:
			if (uobject->handle) {
				b1_handle_set_userdata(uobject->handle, NULL);
				uobject->handle = b1_handle_unref(uobject->handle);
			}
			break;
		}

		kfree(uobject);
	}

	return NULL;
}

static bool
b1_uobject_is_linked(struct b1_uobject *uobject)
{
	return !RB_EMPTY_NODE(&uobject->upeer_rb);
}

DEFINE_FREE(
	b1_uobject_free,
	struct b1_uobject *,
	if (!IS_ERR_OR_NULL(_T))
		b1_uobject_free(_T);
)

static bool
b1_uobject_rb_less(struct rb_node *a, const struct rb_node *b)
{
	struct b1_uobject *a_node, *b_node;

	a_node = container_of(a, struct b1_uobject, upeer_rb);
	b_node = container_of(b, struct b1_uobject, upeer_rb);

	return a_node->id < b_node->id;
}

static int
b1_uobject_rb_cmp(const void *k, const struct rb_node *n)
{
	struct b1_uobject *node = container_of(n, struct b1_uobject, upeer_rb);
	const u64 *key = k;

	if (*key < node->id)
		return -1;
	else if (*key > node->id)
		return 1;
	else
		return 0;
}

static struct b1_upeer *
b1_upeer_free(struct b1_upeer *upeer)
{
	if (upeer) {
		WARN_ON(!RB_EMPTY_ROOT(&upeer->objects));
		mutex_destroy(&upeer->lock);
		upeer->peer = b1_peer_unref(upeer->peer);
		kfree(upeer);
	}

	return NULL;
}

DEFINE_FREE(
	b1_upeer_free,
	struct b1_upeer *,
	if (!IS_ERR_OR_NULL(_T))
		b1_upeer_free(_T);
)

static struct b1_upeer *
b1_upeer_new(struct b1_acct_actor *actor)
{
	struct b1_upeer *upeer __free(b1_upeer_free) = NULL;
	int r;

	upeer = kzalloc_obj(struct b1_upeer);
	if (!upeer)
		return ERR_PTR(-ENOMEM);

	init_waitqueue_head(&upeer->waitq);
	mutex_init(&upeer->lock);
	upeer->objects = RB_ROOT;

	upeer->peer = b1_peer_new(actor, &upeer->waitq);
	if (IS_ERR(upeer->peer)) {
		r = PTR_ERR(upeer->peer);
		upeer->peer = NULL;
		return ERR_PTR(r);
	}

	return no_free_ptr(upeer);
}

static u64 b1_upeer_allocate_id(struct b1_upeer *upeer)
{
	/*
	 * The namespace with the LSB unset is managed by user-space. For
	 * kernel allocated IDs ("managed IDs"), we use a simple counter, but
	 * shift to the left and set the LSB.
	 */
	lockdep_assert_held(&upeer->lock);
	return (upeer->id_allocator++ << 1) | BUS1_MANAGED;
}

static void
b1_upeer_link(struct b1_upeer *upeer, struct b1_uobject *uobject)
{
	lockdep_assert_held(&upeer->lock);

	if (RB_EMPTY_NODE(&uobject->upeer_rb))
		rb_add(&uobject->upeer_rb, &upeer->objects, b1_uobject_rb_less);
}

static void
b1_upeer_unlink(struct b1_upeer *upeer, struct b1_uobject *uobject)
{
	lockdep_assert_held(&upeer->lock);

	if (!RB_EMPTY_NODE(&uobject->upeer_rb)) {
		rb_erase(&uobject->upeer_rb, &upeer->objects);
		RB_CLEAR_NODE(&uobject->upeer_rb);
	}
}

static struct b1_uobject *
b1_upeer_find(struct b1_upeer *upeer, u64 id)
{
	struct rb_node *node;

	lockdep_assert_held(&upeer->lock);

	node = rb_find(&id, &upeer->objects, b1_uobject_rb_cmp);
	if (node)
		return container_of(node, struct b1_uobject, upeer_rb);

	return NULL;
}

static struct b1_uobject *
b1_upeer_find_handle(struct b1_upeer *upeer, u64 id)
{
	struct b1_uobject *uobject;

	uobject = b1_upeer_find(upeer, id);
	if (uobject && uobject->type != B1_UOBJECT_HANDLE)
		uobject = NULL;

	return uobject;
}

static struct b1_uobject *b1_upeer_new_node(struct b1_upeer *upeer)
{
	struct b1_uobject *unode __free(b1_uobject_free) = NULL;

	lockdep_assert_held(&upeer->lock);

	unode = b1_uobject_new(b1_upeer_allocate_id(upeer));
	if (IS_ERR(unode))
		return unode;

	unode->type = B1_UOBJECT_NODE;

	return no_free_ptr(unode);
}

static struct b1_uobject *b1_upeer_new_handle(struct b1_upeer *upeer)
{
	struct b1_uobject *uhandle __free(b1_uobject_free) = NULL;

	lockdep_assert_held(&upeer->lock);

	uhandle = b1_uobject_new(b1_upeer_allocate_id(upeer));
	if (IS_ERR(uhandle))
		return uhandle;

	uhandle->type = B1_UOBJECT_HANDLE;

	return no_free_ptr(uhandle);
}

static int b1_cdev_open(struct inode *inode, struct file *file)
{
	struct b1_cdev *cdev = container_of(file->private_data,
					    struct b1_cdev, misc);
	struct b1_acct_actor *actor __free(b1_acct_actor_unref) = NULL;
	struct b1_acct_user *user __free(b1_acct_user_unref) = NULL;
	struct b1_upeer *upeer __free(b1_upeer_free) = NULL;

	user = b1_acct_get_user(cdev->acct, __kuid_val(file->f_cred->euid));
	if (IS_ERR(user))
		return PTR_ERR(user);

	actor = b1_acct_actor_new(user);
	if (IS_ERR(actor))
		return PTR_ERR(actor);

	upeer = b1_upeer_new(actor);
	if (IS_ERR(upeer))
		return PTR_ERR(upeer);

	b1_peer_begin(upeer->peer);
	file->private_data = no_free_ptr(upeer);
	return 0;
}

static int b1_cdev_release(struct inode *inode, struct file *file)
{
	struct b1_upeer *upeer = file->private_data;
	struct b1_op *op __free(b1_op_free) = NULL;
	struct b1_uobject *uobject, *usafe;

	op = b1_op_new(upeer->peer);
	if (!WARN_ON(!op)) {
		rbtree_postorder_for_each_entry_safe(uobject, usafe,
						     &upeer->objects,
						     upeer_rb) {
			if (uobject->type == B1_UOBJECT_NODE) {
				b1_op_release_node(op, uobject->node);
				b1_node_end(uobject->node);
			}
		}
		b1_op_commit(no_free_ptr(op));
	}

	b1_peer_end(upeer->peer);

	op = b1_op_new(upeer->peer);
	WARN_ON(!op);
	rbtree_postorder_for_each_entry_safe(uobject, usafe, &upeer->objects,
					     upeer_rb) {
		RB_CLEAR_NODE(&uobject->upeer_rb);
		if (op) {
			if (uobject->type == B1_UOBJECT_HANDLE) {
				b1_op_release_handle(op, uobject->handle);
				b1_handle_end(uobject->handle);
			}
		}
		b1_upeer_unlink(upeer, uobject);
		b1_uobject_free(uobject);
	}
	upeer->objects = RB_ROOT;
	if (op)
		b1_op_commit(no_free_ptr(op));

	file->private_data = b1_upeer_free(upeer);
	return 0;
}

static unsigned int
b1_cdev_poll(struct file *file, struct poll_table_struct *wait)
{
	struct b1_upeer *upeer = file->private_data;
	unsigned int mask = 0;

	poll_wait(file, &upeer->waitq, wait);

	mask |= POLLOUT | POLLWRNORM;
	if (b1_peer_readable(upeer->peer))
		mask |= POLLIN | POLLRDNORM;

	return mask;
}

static int
b1_cdev_transfer_collect(
	struct b1_upeer *from,
	struct b1_upeer *to,
	struct bus1_cmd_transfer *cmd,
	struct list_head *unodes,
	struct list_head *uhandles
) {
	struct bus1_transfer __user *u_src, __user *u_dst;
	u64 i;

	lockdep_assert_held(&from->lock);
	lockdep_assert_held(&to->lock);

	u_src = BUS1_TO_PTR(cmd->ptr_src);
	u_dst = BUS1_TO_PTR(cmd->ptr_dst);
	if (unlikely(cmd->ptr_src != BUS1_FROM_PTR(u_src) ||
		     cmd->ptr_dst != BUS1_FROM_PTR(u_dst)))
		return -EFAULT;

	for (i = 0; i < cmd->n_transfers; ++i) {
		struct b1_handle *handle __free(b1_handle_unref) = NULL;
		struct b1_node *node __free(b1_node_unref) = NULL;
		struct b1_uobject *unode, *uhandle;
		struct bus1_transfer src, dst;
		u64 ptr_src, ptr_dst;

		BUILD_BUG_ON(sizeof(*u_src) != sizeof(src));
		BUILD_BUG_ON(sizeof(*u_dst) != sizeof(dst));

		if (check_add_overflow(cmd->ptr_src, i, &ptr_src))
			return -EFAULT;
		if (check_add_overflow(cmd->ptr_dst, i, &ptr_dst))
			return -EFAULT;

		u_src = BUS1_TO_PTR(ptr_src);
		u_dst = BUS1_TO_PTR(ptr_dst);
		if (ptr_src != BUS1_FROM_PTR(u_src) ||
		    ptr_dst != BUS1_FROM_PTR(u_dst) ||
		    copy_from_user(&src, u_src, sizeof(src)))
			return -EFAULT;
		if (src.flags & ~BUS1_TRANSFER_FLAG_CREATE)
			return -EINVAL;

		if (src.flags & BUS1_TRANSFER_FLAG_CREATE) {
			if (src.id != BUS1_INVALID)
				return -EINVAL;

			node = b1_peer_new_node(from->peer, to->peer, &handle);
			if (IS_ERR(node))
				return PTR_ERR(node);

			unode = b1_upeer_new_node(from);
			if (IS_ERR(unode))
				return PTR_ERR(unode);

			list_add_tail(&unode->op_link, unodes);
			b1_node_set_userdata(node, unode);
			unode->node = no_free_ptr(node);

			src.flags = 0;
			src.id = unode->id;
			if (copy_to_user(u_src, &src, sizeof(src)))
				return -EFAULT;
		} else {
			uhandle = b1_upeer_find_handle(from, src.id);
			if (!uhandle)
				return -EBADRQC;

			handle = b1_peer_new_handle(to->peer, uhandle->handle);
			if (IS_ERR(handle))
				return PTR_ERR(handle);
		}

		uhandle = b1_upeer_new_handle(to);
		if (IS_ERR(uhandle))
			return PTR_ERR(uhandle);

		list_add_tail(&uhandle->op_link, uhandles);
		b1_handle_set_userdata(handle, uhandle);
		uhandle->handle = no_free_ptr(handle);

		dst.flags = 0;
		dst.id = uhandle->id;
		if (copy_to_user(u_dst, &dst, sizeof(dst)))
			return -EFAULT;
	}

	return 0;
}

static void
b1_cdev_transfer_commit(
	struct b1_upeer *from,
	struct b1_upeer *to,
	struct bus1_cmd_transfer *cmd,
	struct list_head *unodes,
	struct list_head *uhandles
) {
	struct b1_uobject *unode, *uhandle;

	lockdep_assert_held(&from->lock);
	lockdep_assert_held(&to->lock);

	while ((unode = list_first_entry_or_null(unodes, struct b1_uobject,
						 op_link))) {
		list_del_init(&unode->op_link);
		b1_upeer_link(from, unode);
		b1_node_begin(unode->node);
	}

	while ((uhandle = list_first_entry_or_null(uhandles, struct b1_uobject,
						   op_link))) {
		list_del_init(&uhandle->op_link);
		b1_upeer_link(to, uhandle);
		b1_handle_begin(uhandle->handle);
	}
}

static int
b1_cdev_ioctl_transfer(struct file *file, struct b1_upeer *upeer, unsigned long arg)
{
	struct bus1_cmd_transfer __user *u_cmd = (void __user *)arg;
	struct list_head uhandles = LIST_HEAD_INIT(uhandles);
	struct list_head unodes = LIST_HEAD_INIT(unodes);
	struct b1_uobject *unode, *uhandle;
	CLASS_INIT(fd, fd, EMPTY_FD);
	struct bus1_cmd_transfer cmd;
	struct b1_upeer *other;
	int r, to;

	BUILD_BUG_ON(_IOC_SIZE(BUS1_CMD_TRANSFER) != sizeof(cmd));
	BUILD_BUG_ON(sizeof(*u_cmd) != sizeof(cmd));

	if (copy_from_user(&cmd, u_cmd, sizeof(cmd)))
		return -EFAULT;
	if (cmd.flags != 0)
		return -EINVAL;

	if (cmd.to != BUS1_INVALID) {
		to = (int)cmd.to;
		if (cmd.to != (u64)to || to < 0)
			return -EBADF;
		fd = fdget(to);
		if (fd_empty(fd))
			return -EBADF;
		if (fd_file(fd)->f_op != file->f_op)
			return -EOPNOTSUPP;
		other = fd_file(fd)->private_data;
	} else {
		other = upeer;
	}

	lock2(&upeer->lock, &other->lock);

	r = b1_cdev_transfer_collect(upeer, other, &cmd, &unodes, &uhandles);
	if (r >= 0)
		b1_cdev_transfer_commit(upeer, other, &cmd, &unodes, &uhandles);

	while ((unode = list_first_entry_or_null(&unodes,
						 struct b1_uobject,
						 op_link))) {
		list_del_init(&unode->op_link);
		b1_uobject_free(unode);
	}
	while ((uhandle = list_first_entry_or_null(&uhandles,
						   struct b1_uobject,
						   op_link))) {
		list_del_init(&uhandle->op_link);
		b1_uobject_free(uhandle);
	}

	unlock2(&upeer->lock, &other->lock);

	return r;
}

static int
b1_cdev_release_collect(
	struct b1_upeer *upeer,
	struct bus1_cmd_release *cmd,
	struct list_head *uobjs
) {
	const u64 __user *u_ids;
	u64 i;

	lockdep_assert_held(&upeer->lock);

	u_ids = BUS1_TO_PTR(cmd->ptr_ids);
	if (unlikely(cmd->ptr_ids != BUS1_FROM_PTR(u_ids)))
		return -EFAULT;

	for (i = 0; i < cmd->n_ids; ++i) {
		struct b1_uobject *uobject;
		const u64 __user *u_id;
		u64 ptr_id, id;

		BUILD_BUG_ON(sizeof(*u_id) != sizeof(id));

		if (check_add_overflow(cmd->ptr_ids, i, &ptr_id))
			return -EFAULT;

		u_id = BUS1_TO_PTR(ptr_id);
		if (ptr_id != BUS1_FROM_PTR(u_id) ||
		    copy_from_user(&id, u_id, sizeof(id)))
			return -EFAULT;

		uobject = b1_upeer_find(upeer, id);
		if (!uobject)
			return -EBADRQC;

		list_add_tail(&uobject->op_link, uobjs);
	}

	return 0;
}

static int
b1_cdev_release_commit(
	struct b1_upeer *upeer,
	struct bus1_cmd_release *cmd,
	struct list_head *uobjs
) {
	struct b1_op *op __free(b1_op_free) = NULL;
	struct b1_uobject *uobject;

	lockdep_assert_held(&upeer->lock);

	op = b1_op_new(upeer->peer);
	if (IS_ERR(op))
		return PTR_ERR(op);

	while ((uobject = list_first_entry_or_null(uobjs,
						   struct b1_uobject,
						   op_link))) {
		list_del_init(&uobject->op_link);

		if (uobject->type == B1_UOBJECT_NODE) {
			b1_op_release_node(op, uobject->node);
			b1_node_end(uobject->node);
		} else if (uobject->type == B1_UOBJECT_HANDLE) {
			b1_op_release_handle(op, uobject->handle);
			b1_handle_end(uobject->handle);
		}

		b1_upeer_unlink(upeer, uobject);
		b1_uobject_free(uobject);
	}

	b1_op_commit(no_free_ptr(op));
	return 0;
}

static int
b1_cdev_ioctl_release(struct b1_upeer *upeer, unsigned long arg)
{
	struct bus1_cmd_release __user *u_cmd = (void __user *)arg;
	struct bus1_cmd_release cmd;
	struct list_head uobjs = LIST_HEAD_INIT(uobjs);
	struct b1_uobject *uobject;
	int r;

	BUILD_BUG_ON(_IOC_SIZE(BUS1_CMD_RELEASE) != sizeof(cmd));
	BUILD_BUG_ON(sizeof(*u_cmd) != sizeof(cmd));

	if (copy_from_user(&cmd, u_cmd, sizeof(cmd)))
		return -EFAULT;
	if (cmd.flags != 0)
		return -EINVAL;

	mutex_lock(&upeer->lock);
	r = b1_cdev_release_collect(upeer, &cmd, &uobjs);
	if (r >= 0)
		b1_cdev_release_commit(upeer, &cmd, &uobjs);
	while ((uobject = list_first_entry_or_null(&uobjs,
						   struct b1_uobject,
						   op_link)))
		list_del_init(&uobject->op_link);
	mutex_unlock(&upeer->lock);

	return 0;
}

struct b1_umessage {
	struct b1_upeer *upeer;
	u64 n_transfers;
	struct b1_handle **transfers;
	struct b1_message_shared *shared;
};

static struct b1_umessage *
b1_umessage_free(struct b1_umessage *umessage)
{
	if (umessage) {
		umessage->shared = b1_message_shared_unref(umessage->shared);
		kfree(umessage->transfers);
		kfree(umessage);
	}

	return NULL;
}

DEFINE_FREE(
	b1_umessage_free,
	struct b1_umessage *,
	if (!IS_ERR_OR_NULL(_T))
		b1_umessage_free(_T);
)

static struct b1_umessage *
b1_umessage_new(struct b1_upeer *upeer, u64 n_transfers)
{
	struct b1_umessage *umessage __free(b1_umessage_free) = NULL;
	size_t n_transfers_sz = n_transfers;

	if ((u64)n_transfers_sz != n_transfers)
		return ERR_PTR(-ENOMEM);

	umessage = kzalloc_obj(struct b1_umessage);
	if (!umessage)
		return ERR_PTR(-ENOMEM);

	umessage->upeer = upeer;

	umessage->transfers = kzalloc_objs(struct b1_handle *, n_transfers_sz);
	if (!umessage->transfers)
		return ERR_PTR(-ENOMEM);

	return no_free_ptr(umessage);
}

static struct b1_umessage *
b1_cdev_send_import(
	struct b1_upeer *upeer,
	struct bus1_cmd_send *cmd
) {
	struct b1_umessage *umessage __free(b1_umessage_free) = NULL;
	const struct bus1_transfer __user *u_transfers;
	const struct bus1_message __user *u_message;
	const struct iovec __user *u_data_vecs;
	struct iovec iov_stack[UIO_FASTIOV];
	struct iovec *iov_vec __free(kfree) = NULL;
	void *data __free(kvfree) = NULL;
	struct bus1_message message;
	unsigned int u_n_data_vecs;
	struct iov_iter iov_iter;
	ssize_t n;
	u64 i;
	int r;

	lockdep_assert_held(&upeer->lock);

	BUILD_BUG_ON(sizeof(*u_message) != sizeof(message));

	u_message = BUS1_TO_PTR(cmd->ptr_message);
	if (unlikely(cmd->ptr_message != BUS1_FROM_PTR(u_message) ||
		     copy_from_user(&message, u_message, sizeof(message))))
		return ERR_PTR(-EFAULT);
	if (message.flags != 0 || message.type != BUS1_MESSAGE_TYPE_USER)
		return ERR_PTR(-EINVAL);

	u_transfers = BUS1_TO_PTR(message.ptr_transfers);
	u_n_data_vecs = message.n_data_vecs;
	u_data_vecs = BUS1_TO_PTR(message.ptr_data_vecs);
	if (unlikely(message.ptr_transfers != BUS1_FROM_PTR(u_transfers) ||
		     message.n_data_vecs != (u64)u_n_data_vecs ||
		     message.ptr_data_vecs != BUS1_FROM_PTR(u_data_vecs)))
		return ERR_PTR(-EFAULT);
	if (unlikely(message.n_data > MAX_RW_COUNT))
		return ERR_PTR(-EMSGSIZE);

	umessage = b1_umessage_new(upeer, message.n_transfers);
	if (IS_ERR(umessage))
		return ERR_CAST(umessage);

	/* Import the message data. */

	iov_vec = iov_stack;
	n = import_iovec(
		ITER_SOURCE,
		u_data_vecs,
		u_n_data_vecs,
		ARRAY_SIZE(iov_stack),
		&iov_vec,
		&iov_iter
	);
	if (n < 0)
		return ERR_PTR(n);
	if (n < message.n_data)
		return ERR_PTR(-EMSGSIZE);

	data = kvmalloc(n, GFP_KERNEL);
	if (!data)
		return ERR_PTR(-ENOMEM);
	if (!copy_from_iter_full(data, n, &iov_iter))
		return ERR_PTR(-EFAULT);

	umessage->shared = b1_message_shared_new(no_free_ptr(data));
	if (IS_ERR(umessage->shared)) {
		r = PTR_ERR(umessage->shared);
		umessage->shared = NULL;
		return ERR_PTR(r);
	}

	/* Import the handle transfers. */

	for (i = 0; i < message.n_transfers; ++i) {
		struct b1_uobject *uhandle;
		struct bus1_transfer __user *u_transfer;
		struct bus1_transfer transfer;
		u64 ptr_transfer;

		BUILD_BUG_ON(sizeof(*u_transfer) != sizeof(transfer));

		if (check_add_overflow(message.ptr_transfers, i, &ptr_transfer))
			return ERR_PTR(-EFAULT);

		u_transfer = BUS1_TO_PTR(ptr_transfer);
		if (ptr_transfer != BUS1_FROM_PTR(u_transfer) ||
		    copy_from_user(&transfer, u_transfer, sizeof(transfer)))
			return ERR_PTR(-EFAULT);
		if (transfer.flags != 0)
			return ERR_PTR(-EINVAL);

		uhandle = b1_upeer_find_handle(upeer, transfer.id);
		if (!uhandle)
			return ERR_PTR(-EBADRQC);

		umessage->transfers[i] = uhandle->handle;
		++umessage->n_transfers;
	}

	return no_free_ptr(umessage);
}

static int
b1_cdev_send_commit(
	struct b1_upeer *upeer,
	struct bus1_cmd_send *cmd,
	struct b1_umessage *umessage
) {
	struct b1_op *op __free(b1_op_free) = NULL;
	const u64 __user *u_destinations;
	u32 __user *u_errors;
	u64 i;
	int r;

	lockdep_assert_held(&upeer->lock);

	u_destinations = BUS1_TO_PTR(cmd->ptr_destinations);
	u_errors = BUS1_TO_PTR(cmd->ptr_errors);
	if (unlikely(cmd->ptr_destinations != BUS1_FROM_PTR(u_destinations) ||
		     cmd->ptr_errors != BUS1_FROM_PTR(u_errors)))
		return -EFAULT;

	op = b1_op_new(upeer->peer);

	for (i = 0; i < cmd->n_destinations; ++i) {
		struct b1_uobject *uhandle;
		__u64 __user *u_dst, *u_error;
		u64 ptr_dst, ptr_error, dst, error;

		BUILD_BUG_ON(sizeof(*u_dst) != sizeof(dst));
		BUILD_BUG_ON(sizeof(*u_error) != sizeof(error));

		if (check_add_overflow(cmd->ptr_destinations, i, &ptr_dst) ||
		    check_add_overflow(cmd->ptr_errors, i, &ptr_error))
			return -EFAULT;

		u_dst = BUS1_TO_PTR(ptr_dst);
		u_error = BUS1_TO_PTR(ptr_error);
		if (ptr_dst != BUS1_FROM_PTR(u_dst) ||
		    ptr_error != BUS1_FROM_PTR(u_error) ||
		    get_user(dst, u_dst))
			return -EFAULT;

		uhandle = b1_upeer_find_handle(upeer, dst);
		if (!uhandle)
			return -EBADRQC;

		r = b1_op_send_message(
			op,
			umessage->n_transfers,
			umessage->transfers,
			umessage->shared
		);
		if (r < 0)
			return r;

		error = 0;
		if (put_user(error, u_error))
			return -EFAULT;
	}

	b1_op_commit(no_free_ptr(op));
	return 0;
}

static int
b1_cdev_ioctl_send(struct b1_upeer *upeer, unsigned long arg)
{
	struct bus1_cmd_send __user *u_cmd = (void __user *)arg;
	struct bus1_cmd_send cmd;
	struct b1_umessage *umessage;
	int r;

	BUILD_BUG_ON(_IOC_SIZE(BUS1_CMD_SEND) != sizeof(cmd));
	BUILD_BUG_ON(sizeof(*u_cmd) != sizeof(cmd));

	if (copy_from_user(&cmd, u_cmd, sizeof(cmd)))
		return -EFAULT;
	if (cmd.flags != 0)
		return -EINVAL;

	mutex_lock(&upeer->lock);
	umessage = b1_cdev_send_import(upeer, &cmd);
	if (IS_ERR(umessage))
		r = PTR_ERR(umessage);
	else
		r = b1_cdev_send_commit(upeer, &cmd, umessage);
	umessage = b1_umessage_free(umessage);
	mutex_unlock(&upeer->lock);

	return r;
}

static int
b1_cdev_recv_export_transfers(
	struct b1_upeer *upeer,
	struct b1_peer_peek_user *peek,
	struct bus1_message *message
) {
	struct bus1_transfer __user *u_transfers;
	u64 i;

	lockdep_assert_held(&upeer->lock);

	u_transfers = BUS1_TO_PTR(message->ptr_transfers);
	if (unlikely(message->ptr_transfers != BUS1_FROM_PTR(u_transfers)))
		return -EFAULT;

	for (i = 0; i < peek->n_transfers; ++i) {
		struct b1_uobject *uhandle;
		struct bus1_transfer __user *u_transfer;
		struct bus1_transfer transfer;
		u64 ptr_transfer;

		BUILD_BUG_ON(sizeof(*u_transfer) != sizeof(transfer));

		if (check_add_overflow(message->ptr_transfers, i, &ptr_transfer))
			return -EFAULT;

		u_transfer = BUS1_TO_PTR(ptr_transfer);
		if (ptr_transfer != BUS1_FROM_PTR(u_transfer))
			return -EFAULT;

		uhandle = b1_handle_get_userdata(peek->transfers[i]);
		if (!uhandle) {
			uhandle = b1_upeer_new_handle(upeer);
			if (IS_ERR(uhandle))
				return PTR_ERR(uhandle);

			b1_handle_set_userdata(peek->transfers[i], uhandle);
			uhandle->handle = b1_handle_ref(peek->transfers[i]);
		}

		transfer.flags = 0;
		transfer.id = uhandle->id;
		if (copy_to_user(u_transfer, &transfer, sizeof(transfer)))
			return -EFAULT;
	}

	return 0;
}

static int
b1_cdev_recv_export_data(
	struct b1_upeer *upeer,
	struct b1_peer_peek_user *peek,
	struct bus1_message *message
) {
	const struct iovec __user *u_data_vecs;
	struct iovec iov_stack[UIO_FASTIOV];
	struct iovec *iov_vec __free(kfree) = NULL;
	unsigned int u_n_data_vecs;
	struct iov_iter iov_iter;
	ssize_t n;

	lockdep_assert_held(&upeer->lock);

	u_n_data_vecs = message->n_data_vecs;
	u_data_vecs = BUS1_TO_PTR(message->ptr_data_vecs);
	if (unlikely(message->n_data_vecs != (u64)u_n_data_vecs ||
		     message->ptr_data_vecs != BUS1_FROM_PTR(u_data_vecs)))
		return -EFAULT;

	iov_vec = iov_stack;
	n = import_iovec(
		ITER_DEST,
		u_data_vecs,
		u_n_data_vecs,
		ARRAY_SIZE(iov_stack),
		&iov_vec,
		&iov_iter
	);
	if (n < 0)
		return n;
	if (n < message->n_data)
		return -EMSGSIZE;
	if (message->n_data < peek->n_data)
		return -EMSGSIZE;
	if (!copy_to_iter_full(peek->data, peek->n_data, &iov_iter))
		return -EFAULT;

	return 0;
}

static int
b1_cdev_recv_user(
	struct b1_upeer *upeer,
	struct b1_peer_peek_user *peek,
	struct bus1_metadata *metadata,
	struct bus1_message *message
) {
	struct b1_uobject *unode, *uhandle;
	u64 i;
	int r;

	lockdep_assert_held(&upeer->lock);

	unode = b1_node_get_userdata(peek->node);
	if (!unode) {
		b1_peer_pop(upeer->peer);
		return 0;
	}

	r = b1_cdev_recv_export_transfers(upeer, peek, message);
	if (r >= 0)
		r = b1_cdev_recv_export_data(upeer, peek, message);
	for (i = 0; i < peek->n_transfers; ++i) {
		uhandle = b1_handle_get_userdata(peek->transfers[i]);
		if (!uhandle || b1_uobject_is_linked(uhandle))
			continue;
		if (r >= 0) {
			b1_upeer_link(upeer, uhandle);
			b1_handle_begin(uhandle->handle);
		} else {
			b1_handle_set_userdata(peek->transfers[i], NULL);
			b1_uobject_free(uhandle);
		}
	}
	if (r < 0)
		return r;

	*metadata = (struct bus1_metadata){
		.flags = 0,
		.id = unode->id,
		.account = BUS1_INVALID,
	};
	*message = (struct bus1_message){
		.flags = 0,
		.type = BUS1_MESSAGE_TYPE_USER,
		.n_transfers = peek->n_transfers,
		.ptr_transfers = message->ptr_transfers,
		.n_data = peek->n_data,
		.n_data_vecs = message->n_data_vecs,
		.ptr_data_vecs = message->ptr_data_vecs,
	};

	return 1;
}

static int
b1_cdev_recv_node_release(
	struct b1_upeer *upeer,
	struct b1_handle *handle,
	struct bus1_metadata *metadata,
	struct bus1_message *message
) {
	struct b1_uobject *uhandle;

	lockdep_assert_held(&upeer->lock);

	uhandle = b1_handle_get_userdata(handle);
	if (!uhandle) {
		b1_peer_pop(upeer->peer);
		return 0;
	}

	*metadata = (struct bus1_metadata){
		.flags = 0,
		.id = uhandle->id,
		.account = BUS1_INVALID,
	};
	*message = (struct bus1_message){
		.flags = 0,
		.type = BUS1_MESSAGE_TYPE_NODE_RELEASE,
		.n_transfers = 0,
		.ptr_transfers = message->ptr_transfers,
		.n_data = 0,
		.n_data_vecs = message->n_data_vecs,
		.ptr_data_vecs = message->ptr_data_vecs,
	};

	b1_handle_end(uhandle->handle);
	b1_upeer_unlink(upeer, uhandle);
	b1_uobject_free(uhandle);
	return 1;
}

static int
b1_cdev_recv_handle_release(
	struct b1_upeer *upeer,
	struct b1_node *node,
	struct bus1_metadata *metadata,
	struct bus1_message *message
) {
	struct b1_uobject *unode;

	lockdep_assert_held(&upeer->lock);

	unode = b1_node_get_userdata(node);
	if (!unode) {
		b1_peer_pop(upeer->peer);
		return 0;
	}

	*metadata = (struct bus1_metadata){
		.flags = 0,
		.id = unode->id,
		.account = BUS1_INVALID,
	};
	*message = (struct bus1_message){
		.flags = 0,
		.type = BUS1_MESSAGE_TYPE_HANDLE_RELEASE,
		.n_transfers = 0,
		.ptr_transfers = message->ptr_transfers,
		.n_data = 0,
		.n_data_vecs = message->n_data_vecs,
		.ptr_data_vecs = message->ptr_data_vecs,
	};

	b1_node_end(unode->node);
	b1_upeer_unlink(upeer, unode);
	b1_uobject_free(unode);
	return 1;
}

static int
b1_cdev_recv_peek(
	struct b1_upeer *upeer,
	struct bus1_metadata *metadata,
	struct bus1_message *message
) {
	struct b1_peer_peek peek;

	lockdep_assert_held(&upeer->lock);

	if (!b1_peer_peek(upeer->peer, &peek))
		return -EAGAIN;

	switch (peek.type) {
	case BUS1_MESSAGE_TYPE_USER:
		return b1_cdev_recv_user(
			upeer,
			&peek.u.user,
			metadata,
			message
		);
	case BUS1_MESSAGE_TYPE_NODE_RELEASE:
		return b1_cdev_recv_node_release(
			upeer,
			peek.u.node_release.handle,
			metadata,
			message
		);
	case BUS1_MESSAGE_TYPE_HANDLE_RELEASE:
		return b1_cdev_recv_handle_release(
			upeer,
			peek.u.handle_release.node,
			metadata,
			message
		);
	default:
		WARN_ONCE(1, "invalid message type: %llu", peek.type);
		b1_peer_pop(upeer->peer);
		return -ENOTRECOVERABLE;
	}
}

static int
b1_cdev_ioctl_recv(struct b1_upeer *upeer, unsigned long arg)
{
	struct bus1_cmd_recv __user *u_cmd = (void __user *)arg;
	struct bus1_metadata __user *u_metadata;
	struct bus1_message __user *u_message;
	struct bus1_metadata metadata;
	struct bus1_message message;
	struct bus1_cmd_recv cmd;
	int r;

	BUILD_BUG_ON(_IOC_SIZE(BUS1_CMD_RECV) != sizeof(cmd));
	BUILD_BUG_ON(sizeof(*u_cmd) != sizeof(cmd));

	if (copy_from_user(&cmd, u_cmd, sizeof(cmd)))
		return -EFAULT;
	if (cmd.flags != 0)
		return -EINVAL;

	u_metadata = BUS1_TO_PTR(cmd.ptr_metadata);
	u_message = BUS1_TO_PTR(cmd.ptr_message);
	if (unlikely(cmd.ptr_metadata != BUS1_FROM_PTR(u_metadata) ||
		     cmd.ptr_message != BUS1_FROM_PTR(u_message)))
		return -EFAULT;

	if (copy_from_user(&message, u_message, sizeof(message)))
		return -EFAULT;
	if (message.flags != 0 || message.type != BUS1_INVALID)
		return -EINVAL;

	memset(&metadata, 0, sizeof(metadata));

	mutex_lock(&upeer->lock);
	do {
		r = b1_cdev_recv_peek(upeer, &metadata, &message);
	} while (!r);
	if (r > 0) {
		if (copy_to_user(u_metadata, &metadata, sizeof(metadata)) ||
		    copy_to_user(u_message, &message, sizeof(message)))
			r = -EFAULT;
		else
			b1_peer_pop(upeer->peer);
	}
	mutex_unlock(&upeer->lock);

	return r;
}

static long
b1_cdev_ioctl(struct file *file, unsigned int cmd, unsigned long arg)
{
	struct b1_upeer *upeer = file->private_data;
	int r;

	switch (cmd) {
	case BUS1_CMD_TRANSFER:
		r = b1_cdev_ioctl_transfer(file, upeer, arg);
		break;
	case BUS1_CMD_RELEASE:
		r = b1_cdev_ioctl_release(upeer, arg);
		break;
	case BUS1_CMD_SEND:
		r = b1_cdev_ioctl_send(upeer, arg);
		break;
	case BUS1_CMD_RECV:
		r = b1_cdev_ioctl_recv(upeer, arg);
		break;
	default:
		r = -ENOTTY;
		break;
	}

	return r;
}

static const struct file_operations b1_cdev_fops = {
	.owner			= THIS_MODULE,
	.open			= b1_cdev_open,
	.release		= b1_cdev_release,
	.poll			= b1_cdev_poll,
	.unlocked_ioctl		= b1_cdev_ioctl,
	.compat_ioctl		= b1_cdev_ioctl,
};

/**
 * b1_cdev_new() - initialize a new bus1 character device
 * @acct:		accounting system to use for this character device
 *
 * This registers a new bus1 character device and returns it to the caller.
 * Once the object is returned, it will be live and ready.
 *
 * Return: A pointer to the new device is returned, ERR_PTR on failure.
 */
struct b1_cdev *b1_cdev_new(struct b1_acct *acct)
{
	struct b1_cdev *cdev __free(b1_cdev_free) = NULL;
	int r;

	cdev = kzalloc_obj(struct b1_cdev);
	if (!cdev)
		return ERR_PTR(-ENOMEM);

	cdev->acct = b1_acct_ref(acct);
	cdev->misc = (struct miscdevice){
		.fops = &b1_cdev_fops,
		.minor = MISC_DYNAMIC_MINOR,
		.name = KBUILD_MODNAME,
		.mode = S_IRUGO | S_IWUGO,
	};

	r = misc_register(&cdev->misc);
	if (r < 0) {
		cdev->misc.fops = NULL;
		return ERR_PTR(r);
	}

	return no_free_ptr(cdev);
}

/**
 * b1_cdev_free() - destroy a bus1 character device
 * @cdev:		character device to operate on, or NULL
 *
 * This unregisters and frees a previously registered bus1 character device.
 *
 * If you pass NULL, this is a no-op.
 *
 * Return: NULL is returned.
 */
struct b1_cdev *b1_cdev_free(struct b1_cdev *cdev)
{
	if (cdev) {
		if (cdev->misc.fops)
			misc_deregister(&cdev->misc);
		cdev->acct = b1_acct_unref(cdev->acct);
		kfree(cdev);
	}

	return NULL;
}
