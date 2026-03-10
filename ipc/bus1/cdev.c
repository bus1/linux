// SPDX-License-Identifier: GPL-2.0
#define pr_fmt(fmt) KBUILD_MODNAME ": " fmt
#include <linux/container_of.h>
#include <linux/cred.h>
#include <linux/err.h>
#include <linux/fs.h>
#include <linux/init.h>
#include <linux/miscdevice.h>
#include <linux/mutex.h>
#include <linux/poll.h>
#include <linux/rbtree.h>
#include <linux/slab.h>
#include <linux/stat.h>
#include <uapi/linux/bus1.h>
#include "cdev.h"

struct b1_cdev {
	struct miscdevice misc;
};

static const struct file_operations b1_cdev_fops = {
	.owner			= THIS_MODULE,
};

/**
 * b1_cdev_new() - initialize a new bus1 character device
 *
 * This registers a new bus1 character device and returns it to the caller.
 * Once the object is returned, it will be live and ready.
 *
 * Return: A pointer to the new device is returned, ERR_PTR on failure.
 */
struct b1_cdev *b1_cdev_new(void)
{
	struct b1_cdev *cdev;
	int r;

	cdev = kzalloc(sizeof(*cdev), GFP_KERNEL);
	if (!cdev)
		return ERR_PTR(-ENOMEM);

	cdev->misc = (struct miscdevice){
		.fops = &b1_cdev_fops,
		.minor = MISC_DYNAMIC_MINOR,
		.name = KBUILD_MODNAME,
		.mode = S_IRUGO | S_IWUGO,
	};

	r = misc_register(&cdev->misc);
	if (r < 0) {
		cdev->misc.fops = NULL;
		goto error;
	}

	return cdev;

error:
	b1_cdev_free(cdev);
	return ERR_PTR(r);
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
	if (!cdev)
		return NULL;

	if (cdev->misc.fops)
		misc_deregister(&cdev->misc);
	kfree(cdev);

	return NULL;
}
