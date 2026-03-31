# SPDX-License-Identifier: GPL-2.0

quiet_cmd_bindgen = BINDGEN $@
      cmd_bindgen = \
	$(BINDGEN) $< -o $@ \
		$(addprefix --allowlist-file=,$^) \
		--ctypes-prefix kernel::ffi \
		--no-debug '.*' \
		--no-layout-tests \
		--rust-target 1.68 \
		--use-core \
		-- \
		$(c_flags) \
		-D__BINDGEN__ \
		-DMODULE \
		-fno-builtin

$(obj)/capi.rs: $(src)/lib.h $(srctree)/include/uapi/linux/bus1.h
	$(call cmd,bindgen)

$(obj)/lib.o: $(obj)/capi.rs
$(obj)/lib.o: export BUS1_CAPI_PATH=$(abspath $(obj)/capi.rs)

bus1-y :=		\
	lib.o			\
	main.o

obj-$(CONFIG_BUS1) += bus1.o
