// SPDX-License-Identifier: GPL-2.0
#define pr_fmt(fmt) KBUILD_MODNAME ": " fmt
#include <linux/cleanup.h>
#include <linux/err.h>
#include <linux/init.h>
#include <linux/module.h>
#include <linux/sizes.h>
#include "cdev.h"
#include "lib.h"

static struct b1_cdev *b1_main_cdev;

static int __init b1_main_init(void)
{
	struct b1_acct *acct __free(b1_acct_unref) = NULL;
	const b1_acct_value_t maxima[] = {
		[B1_ACCT_SLOT_OBJECTS] = SZ_1M,
		[B1_ACCT_SLOT_BYTES] = SZ_1G,
	};

	acct = b1_acct_new(&maxima);
	if (IS_ERR(acct))
		return PTR_ERR(acct);

	b1_main_cdev = b1_cdev_new(acct);
	if (IS_ERR(b1_main_cdev))
		return PTR_ERR(b1_main_cdev);

	return 0;
}

static void __exit b1_main_deinit(void)
{
	if (!IS_ERR_OR_NULL(b1_main_cdev))
		b1_main_cdev = b1_cdev_free(b1_main_cdev);
}

module_init(b1_main_init);
module_exit(b1_main_deinit);
MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("Capability-based IPC");
