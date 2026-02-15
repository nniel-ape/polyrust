RALPHEX_IMAGE ?= ralphex-rust

.PHONY: ralphex ralphex-build
ralphex:
	RALPHEX_IMAGE=$(RALPHEX_IMAGE) ralphex-dk $(PLAN)

ralphex-build:
	docker build -t $(RALPHEX_IMAGE) -f .ralphex/Dockerfile .
