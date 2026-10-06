# SPDX-License-Identifier: GPL-2.0
KDIR ?= $(HOME)/linux
LLVM ?= 1
.PHONY: all clean test tools
all:
	$(MAKE) -C $(KDIR) M=$(CURDIR) LLVM=$(LLVM) modules
clean:
	$(MAKE) -C $(KDIR) M=$(CURDIR) LLVM=$(LLVM) clean
test:
	rustc --edition=2021 --test tests/core.rs -o /tmp/zettbridge-core-tests
	/tmp/zettbridge-core-tests
	rustc --edition=2021 --test tests/consumer.rs -o /tmp/zettbridge-consumer-tests
	/tmp/zettbridge-consumer-tests
tools:
	$(CC) -std=gnu11 -O2 -Wall -Wextra -Werror -pthread -static -Iinclude tools/zbctl.c -o tools/zbctl
