// SPDX-License-Identifier: GPL-2.0
#define pr_fmt(fmt) KBUILD_MODNAME ": " fmt
#include <linux/err.h>
#include <linux/init.h>
#include <linux/module.h>
#include "cdev.h"
#include "lib.h"

static struct b1_cdev *b1_main_cdev;

static int __init b1_main_init(void)
{
	int r;

	b1_main_cdev = b1_cdev_new();
	if (IS_ERR(b1_main_cdev)) {
		r = PTR_ERR(b1_main_cdev);
		b1_main_cdev = NULL;
		return r;
	}

	return 0;
}

static void __exit b1_main_deinit(void)
{
	b1_main_cdev = b1_cdev_free(b1_main_cdev);
}

module_init(b1_main_init);
module_exit(b1_main_deinit);
MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("Capability-based IPC");
