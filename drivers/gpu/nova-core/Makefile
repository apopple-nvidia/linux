# SPDX-License-Identifier: GPL-2.0

# TODO: The next bit has been stolen from rust/Makefile, so we need to move it
# somewhere that modules can use rather than cutting and pasting.

ifdef CONFIG_CC_IS_CLANG
bindgen_c_flags = $(c_flags)
else
# bindgen relies on libclang to parse C. Ideally, bindgen would support a GCC
# plugin backend and/or the Clang driver would be perfectly compatible with GCC.
#
# For the moment, here we are tweaking the flags on the fly. This is a hack,
# and some kernel configurations may not work (e.g. `GCC_PLUGIN_RANDSTRUCT`
# if we end up using one of those structs).
bindgen_skip_c_flags := -mno-fp-ret-in-387 -mpreferred-stack-boundary=% \
	-mskip-rax-setup -mgeneral-regs-only -msign-return-address=% \
	-mindirect-branch=thunk-extern -mindirect-branch-register \
	-mfunction-return=thunk-extern -mrecord-mcount -mabi=lp64 \
	-mindirect-branch-cs-prefix -mstack-protector-guard% -mtraceback=no \
	-mno-pointers-to-nested-functions -mno-string \
	-mno-strict-align -mstrict-align -mdirect-extern-access \
	-mexplicit-relocs -mno-check-zero-division \
	-fconserve-stack -falign-jumps=% -falign-loops=% \
	-femit-struct-debug-baseonly -fno-ipa-cp-clone -fno-ipa-sra \
	-fno-partial-inlining -fplugin-arg-arm_ssp_per_task_plugin-% \
	-fno-reorder-blocks -fno-allow-store-data-races -fasan-shadow-offset=% \
	-fzero-call-used-regs=% -fno-stack-clash-protection \
	-fno-inline-functions-called-once -fsanitize=bounds-strict \
	-fstrict-flex-arrays=% -fmin-function-alignment=% \
	-fzero-init-padding-bits=% \
	--param=% --param asan-%

# Derived from `scripts/Makefile.clang`.
BINDGEN_TARGET_x86	:= x86_64-linux-gnu
BINDGEN_TARGET_arm64	:= aarch64-linux-gnu
BINDGEN_TARGET_arm	:= arm-linux-gnueabi
BINDGEN_TARGET_loongarch	:= loongarch64-linux-gnusf
BINDGEN_TARGET_um	:= $(BINDGEN_TARGET_$(SUBARCH))
BINDGEN_TARGET		:= $(BINDGEN_TARGET_$(SRCARCH))

# All warnings are inhibited since GCC builds are very experimental,
# many GCC warnings are not supported by Clang, they may only appear in
# some configurations, with new GCC versions, etc.
bindgen_extra_c_flags = -w --target=$(BINDGEN_TARGET)

# Auto variable zero-initialization requires an additional special option with
# clang that is going to be removed sometime in the future (likely in
# clang-18), so make sure to pass this option only if clang supports it
# (libclang major version < 16).
#
# https://github.com/llvm/llvm-project/issues/44842
# https://github.com/llvm/llvm-project/blob/llvmorg-16.0.0-rc2/clang/docs/ReleaseNotes.rst#deprecated-compiler-flags
ifdef CONFIG_INIT_STACK_ALL_ZERO
libclang_maj_ver=$(shell $(BINDGEN) $(srctree)/scripts/rust_is_available_bindgen_libclang.h 2>&1 | sed -ne 's/.*clang version \([0-9]*\).*/\1/p')
ifeq ($(shell expr $(libclang_maj_ver) \< 16), 1)
bindgen_extra_c_flags += -enable-trivial-auto-var-init-zero-knowing-it-will-be-removed-from-clang
endif
endif

bindgen_c_flags = $(filter-out $(bindgen_skip_c_flags), $(c_flags)) \
	$(bindgen_extra_c_flags)
endif

ifdef CONFIG_LTO
bindgen_c_flags_lto = $(filter-out $(CC_FLAGS_LTO), $(bindgen_c_flags))
else
bindgen_c_flags_lto = $(bindgen_c_flags)
endif

# `-fno-builtin` is passed to avoid `bindgen` from using `clang` builtin
# prototypes for functions like `memcpy` -- if this flag is not passed,
# `bindgen`-generated prototypes use `c_ulong` or `c_uint` depending on
# architecture instead of generating `usize`.
bindgen_c_flags_final = $(bindgen_c_flags_lto) -fno-builtin -D__BINDGEN__

# Each `bindgen` release may upgrade the list of Rust target versions. By
# default, the highest stable release in their list is used. Thus we need to set
# a `--rust-target` to avoid future `bindgen` releases emitting code that
# `rustc` may not understand. On top of that, `bindgen` does not support passing
# an unknown Rust target version.
#
# Therefore, the Rust target for `bindgen` can be only as high as the minimum
# Rust version the kernel supports and only as high as the greatest stable Rust
# target supported by the minimum `bindgen` version the kernel supports (that
# is, if we do not test the actual `rustc`/`bindgen` versions running).
#
# Starting with `bindgen` 0.71.0, we will be able to set any future Rust version
# instead, i.e. we will be able to set here our minimum supported Rust version.
quiet_cmd_bindgen = BINDGEN $@
      cmd_bindgen = \
	$(BINDGEN) $< $(bindgen_target_flags) --rust-target 1.68 \
		--use-core --with-derive-default --ctypes-prefix ffi --no-layout-tests \
		--enable-function-attribute-detection \
		-o $@ -- $(bindgen_c_flags_final) -DMODULE \
		$(bindgen_target_cflags) $(bindgen_target_extra)

ifdef CONFIG_NVIDIA_OPEN_GPU_KERNEL_PATH

include $(CONFIG_NVIDIA_OPEN_GPU_KERNEL_PATH)/version.mk
OPEN_GPU_KERNEL_VERSION := r$(subst .,_,$(NVIDIA_VERSION))

bindgen_c_flags_final += -I$(CONFIG_NVIDIA_OPEN_GPU_KERNEL_PATH)/src/nvidia/inc/kernel
bindgen_c_flags_final += -I$(CONFIG_NVIDIA_OPEN_GPU_KERNEL_PATH)/src/nvidia/generated
bindgen_c_flags_final += -I$(CONFIG_NVIDIA_OPEN_GPU_KERNEL_PATH)/src/nvidia/inc/libraries
bindgen_c_flags_final += -I$(CONFIG_NVIDIA_OPEN_GPU_KERNEL_PATH)/src/nvidia/arch/nvalloc/common/inc
bindgen_c_flags_final += -I$(CONFIG_NVIDIA_OPEN_GPU_KERNEL_PATH)/src/common/inc
bindgen_c_flags_final += -I$(CONFIG_NVIDIA_OPEN_GPU_KERNEL_PATH)/src/nvidia/kernel/inc
bindgen_c_flags_final += -I$(CONFIG_NVIDIA_OPEN_GPU_KERNEL_PATH)/src/nvidia/inc
bindgen_c_flags_final += -I$(CONFIG_NVIDIA_OPEN_GPU_KERNEL_PATH)/src/nvidia/interface
bindgen_c_flags_final += -I$(CONFIG_NVIDIA_OPEN_GPU_KERNEL_PATH)/src/common/nvlink/inband/interface
bindgen_c_flags_final += -I$(CONFIG_NVIDIA_OPEN_GPU_KERNEL_PATH)/src/common/inc/swref/published
bindgen_c_flags_final += -I$(CONFIG_NVIDIA_OPEN_GPU_KERNEL_PATH)/src/common/inc/swref
bindgen_c_flags_final += -I$(CONFIG_NVIDIA_OPEN_GPU_KERNEL_PATH)/src/common/uproc/os/common/include/

# So stddef.h can be found
bindgen_c_flags_final += -I$(srctree)/include/linux
bindgen_c_flags_final += -DPORT_ATOMIC_64_BIT_SUPPORTED=1
bindgen_c_flags_final += -DPORT_IS_KERNEL_BUILD=1
bindgen_c_flags_final += -DPORT_IS_CHECKED_BUILD=1
bindgen_c_flags_final += -DPORT_MODULE_atomic=0
bindgen_c_flags_final += -DPORT_MODULE_core=0
bindgen_c_flags_final += -DPORT_MODULE_cpu=0
bindgen_c_flags_final += -DPORT_MODULE_crypto=0
bindgen_c_flags_final += -DPORT_MODULE_debug=1
bindgen_c_flags_final += -DPORT_MODULE_memory=1
bindgen_c_flags_final += -DPORT_MODULE_safe=0
bindgen_c_flags_final += -DPORT_MODULE_string=0
bindgen_c_flags_final += -DPORT_MODULE_sync=0
bindgen_c_flags_final += -DPORT_MODULE_thread=1
bindgen_c_flags_final += -DPORT_MODULE_util=0
bindgen_c_flags_final += -DPORT_MODULE_example=0
bindgen_c_flags_final += -DPORT_MODULE_mmio=0
bindgen_c_flags_final += -DPORT_MODULE_time=0

$(obj)/nvfw/$(OPEN_GPU_KERNEL_VERSION)_bindings.rs: $(src)/nvfw/$(OPEN_GPU_KERNEL_VERSION)_bindings.h \
    $(srctree)/rust/bindgen_parameters FORCE
	$(Q)mkdir -p $(obj)/nvfw
	$(call if_changed_dep,bindgen)
$(obj)/nvfw/$(OPEN_GPU_KERNEL_VERSION)_bindings.rs: private bindgen_target_flags = \
    $(shell grep -Ev '^#|^$$' $(srctree)/rust/bindgen_parameters)
$(obj)/nvfw/$(OPEN_GPU_KERNEL_VERSION)_bindings.rs: private bindgen_target_flags += \
    $(foreach item,$(shell cat $(src)/nvidia-open-gpu-symbols.txt),\
        $(if $(filter type:%,$(item)),--allowlist-type=$(patsubst type:%,%,$(item)),\
        $(if $(filter var:%,$(item)),--allowlist-var=$(patsubst var:%,%,$(item)))))

$(obj)/nova_core.o: $(obj)/nvfw/$(OPEN_GPU_KERNEL_VERSION)_bindings.rs
obj-$(CONFIG_NOVA_CORE) += nova_core.o

else
$(error "Must specify CONFIG_NVIDIA_OPEN_GPU_KERNEL_PATH")
endif
