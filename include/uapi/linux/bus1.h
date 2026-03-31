/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _UAPI_LINUX_BUS1_H
#define _UAPI_LINUX_BUS1_H

#include <linux/ioctl.h>
#include <linux/types.h>

#define BUS1_IOCTL_MAGIC			(0x97)
#define BUS1_INVALID				((__u64)-1)
#define BUS1_MANAGED				((__u64)0x1)

#define BUS1_FROM_PTR(_v) ((__u64)(void *)(_v))
#define BUS1_TO_PTR(_v) ((void *)(__u64)(_v))

#define BUS1_TRANSFER_FLAG_CREATE		(((__u64)1) << 0)

struct bus1_transfer {
	__u64 flags;
	__u64 id;
} __attribute__((__aligned__(8)));

struct bus1_metadata {
	__u64 flags;
	__u64 id;
	__u64 account;
} __attribute__((__aligned__(8)));

enum bus1_message_type: u64 {
	BUS1_MESSAGE_TYPE_USER			= 0,
	BUS1_MESSAGE_TYPE_NODE_RELEASE		= 1,
	BUS1_MESSAGE_TYPE_HANDLE_RELEASE	= 2,
	_BUS1_MESSAGE_TYPE_N,
};

struct bus1_message {
	__u64 flags;
	__u64 type;
	__u64 n_transfers;
	__u64 ptr_transfers;
	__u64 n_data;
	__u64 n_data_vecs;
	__u64 ptr_data_vecs;
} __attribute__((__aligned__(8)));

struct bus1_cmd_transfer {
	__u64 flags;
	__u64 to;
	__u64 n_transfers;
	__u64 ptr_src;
	__u64 ptr_dst;
} __attribute__((__aligned__(8)));

struct bus1_cmd_release {
	__u64 flags;
	__u64 n_ids;
	__u64 ptr_ids;
} __attribute__((__aligned__(8)));

struct bus1_cmd_send {
	__u64 flags;
	__u64 n_destinations;
	__u64 ptr_destinations;
	__u64 ptr_errors;
	__u64 ptr_message;
} __attribute__((__aligned__(8)));

struct bus1_cmd_recv {
	__u64 flags;
	__u64 ptr_metadata;
	__u64 ptr_message;
} __attribute__((__aligned__(8)));

#define BUS1_CMD_TRANSFER \
	(_IOWR(BUS1_IOCTL_MAGIC, 0x00, struct bus1_cmd_transfer))
#define BUS1_CMD_RELEASE \
	(_IOWR(BUS1_IOCTL_MAGIC, 0x01, struct bus1_cmd_release))
#define BUS1_CMD_SEND \
	(_IOWR(BUS1_IOCTL_MAGIC, 0x02, struct bus1_cmd_send))
#define BUS1_CMD_RECV \
	(_IOWR(BUS1_IOCTL_MAGIC, 0x03, struct bus1_cmd_recv))

#endif /* _UAPI_LINUX_BUS1_H */
