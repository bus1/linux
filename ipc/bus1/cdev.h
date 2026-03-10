// SPDX-License-Identifier: GPL-2.0
#ifndef __B1_CDEV_H
#define __B1_CDEV_H

/**
 * DOC: Character Device for Bus1
 *
 * This implements the character-device API for Bus1. It allows full access to
 * the Bus1 communication system through a singleton character device. The
 * character device is named after `KBUILD_MODNAME` and registered with a
 * dynamic minor number. Thus, it can be loaded multiple times under different
 * names, usually for testing.
 *
 * Every file description associated with the character device will represent a
 * single Bus1 peer. IOCTLs on the character device expose the different Bus1
 * operations in a direct mapping.
 */

struct b1_cdev;

struct b1_cdev *b1_cdev_new(void);
struct b1_cdev *b1_cdev_free(struct b1_cdev *cdev);

#endif /* __B1_CDEV_H */
