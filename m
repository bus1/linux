#!/usr/bin/make -f

#
# Bus1 Maintenance Makefile
#

# Enforce bash with fatal errors.
SHELL			:= /bin/bash -eo pipefail

# Keep intermediates around on failures for better caching.
.SECONDARY:

# Default build and source directories.
BUILDDIR		?= ./build
SRCDIR			?= .

#
# Target: help
#

.PHONY: help
help:
	@# 80-width marker:
	@#     01234567012345670123456701234567012345670123456701234567012345670123456701234567
	@echo "make [TARGETS...]"
	@echo
	@echo "The following targets are provided by this maintenance makefile:"
	@echo
	@echo "    help:               Print this usage information."
	@echo "    kunit:              Short for 'kunit-run'"
	@echo "    man:                Short for 'man-build'"
	@echo "    mod:                Short for 'mod-build'"
	@echo
	@echo "    kunit-build:        Run kunit-build"
	@echo "    kunit-config:       Run kunit-config with a bus1 configuration"
	@echo "    kunit-run:          Run kunit-run on the bus1 test suite"
	@echo
	@echo "    man-build:          Build all man-pages"
	@echo
	@echo "    mod-build:          Build the bus1 kernel module (in-source)"

#
# Target: BUILDDIR
#

$(BUILDDIR)/:
	mkdir -p "$@"

$(BUILDDIR)/%/:
	mkdir -p "$@"

#
# Target: kunit-*
#
# Run the bus1 kunit suite. This will configure and build a UM kernel in
# $(BUILDDIR)/kunit/ with KUNIT tests enabled. When running the kunit-tests,
# only the bus1 test-suite is selected.
#

KUNIT_OPTS	= \
	--build_dir "$(BUILDDIR)/kunit/" \
	--make_options LLVM=1

$(BUILDDIR)/kunit/.kunitconfig: | $(BUILDDIR)/kunit/
	echo "CONFIG_BUS1=y" >>"$@"
	echo "CONFIG_RUST=y" >>"$@"
	echo "CONFIG_KUNIT=y" >>"$@"
	echo "CONFIG_KUNIT_ALL_TESTS=y" >>"$@"
	echo "CONFIG_KUNIT_EXAMPLE_TEST=y" >>"$@"

$(BUILDDIR)/kunit/.config: $(BUILDDIR)/kunit/.kunitconfig | $(BUILDDIR)/kunit/
	$(SRCDIR)/tools/testing/kunit/kunit.py \
		config \
		$(KUNIT_OPTS)

.PHONY: kunit-config
kunit-config: $(BUILDDIR)/kunit/.config

.PHONY: kunit-build
kunit-build: $(BUILDDIR)/kunit/.config | $(BUILDDIR)/kunit/
	$(SRCDIR)/tools/testing/kunit/kunit.py \
		build \
		$(KUNIT_OPTS)

.PHONY: kunit-run
kunit-run: | $(BUILDDIR)/kunit/
	$(SRCDIR)/tools/testing/kunit/kunit.py \
		run \
		$(KUNIT_OPTS) \
		--timeout=8 \
		'bus1*'

.PHONY: kunit
kunit: kunit-run

#
# Target: man-*
#
# The following targets build all packaged man-pages. We use `rst2man` to
# convert our RST sources into TROFF man-pages.
#

MAN_RST = $(wildcard $(SRCDIR)/Documentation/bus1/*.[0123456789].rst)
MAN_TROFF = $(patsubst $(SRCDIR)/Documentation/bus1/%.rst,$(BUILDDIR)/man/%,$(MAN_RST))

$(MAN_TROFF): $(BUILDDIR)/man/%: $(SRCDIR)/Documentation/bus1/%.rst | $(BUILDDIR)/man/
	rst2man "$<" "$@"

.PHONY: man-build
man-build: $(MAN_TROFF)

.PHONY: man
man: man-build

#
# Target: mod-*
#

$(BUILDDIR)/mod/.config: $(BUILDDIR)/kunit/.config | $(BUILDDIR)/mod/
	cp "$(BUILDDIR)/kunit/.config" "$@"

.PHONY: mod-build
mod-build: $(BUILDDIR)/mod/.config | $(BUILDDIR)/mod/
	$(MAKE) \
		-C "$(SRCDIR)" \
		ARCH=um \
		LLVM=1 \
		O="$(BUILDDIR)/mod" \
		"ipc/bus1/"

.PHONY: mod
mod: mod-build
