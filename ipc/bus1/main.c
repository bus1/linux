// SPDX-License-Identifier: GPL-2.0
#define pr_fmt(fmt) KBUILD_MODNAME ": " fmt
#include <linux/init.h>
#include <linux/module.h>
#include "lib.h"

static int __init b1_main_init(void)
{
	return 0;
}

static void __exit b1_main_deinit(void)
{
}

module_init(b1_main_init);
module_exit(b1_main_deinit);
MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("Capability-based IPC");
