.PHONY: all build release install dev check test clean \
        bump-patch bump-minor bump-major version \
        daemon-start daemon-stop daemon-restart daemon-status \
        help

# ── Конфигурация ──────────────────────────────────────────────────────────────

BINARY     := icode
INSTALL_DIR := $(HOME)/.local/bin
CARGO_FLAGS := -p $(BINARY)

# Цвета для вывода
BOLD  := \033[1m
GREEN := \033[32m
CYAN  := \033[36m
RESET := \033[0m

# ── Сборка ────────────────────────────────────────────────────────────────────

all: release install  ## Сборка + установка (по умолчанию)

build:  ## Debug-сборка
	cargo build $(CARGO_FLAGS)

release:  ## Release-сборка (оптимизированная)
	cargo build --release $(CARGO_FLAGS)

install: release  ## Установить бинарник в $(INSTALL_DIR)
	@mkdir -p $(INSTALL_DIR)
	cp target/release/$(BINARY) $(INSTALL_DIR)/$(BINARY)
	@printf "$(GREEN)✓ Установлено: $(INSTALL_DIR)/$(BINARY)$(RESET)\n"

dev: release install daemon-restart  ## Полный цикл разработки: build → install → restart daemon
	@printf "$(GREEN)✓ Dev-цикл завершён. Индексация запустится автоматически (новый BUILD_NUMBER).$(RESET)\n"

check:  ## Быстрая проверка компиляции без сборки
	cargo check

test:  ## Запустить все тесты
	cargo test

clean:  ## Очистить артефакты сборки
	cargo clean

# ── Версия ────────────────────────────────────────────────────────────────────

version:  ## Показать текущую версию
	@grep '^version' Cargo.toml | head -1 | awk '{print $$3}' | tr -d '"'

bump-patch:  ## Поднять patch-версию (0.9.1 → 0.9.2)
	@$(MAKE) _bump PART=patch

bump-minor:  ## Поднять minor-версию (0.9.1 → 0.10.0)
	@$(MAKE) _bump PART=minor

bump-major:  ## Поднять major-версию (0.9.1 → 1.0.0)
	@$(MAKE) _bump PART=major

_bump:
	@OLD=$$($(MAKE) -s version); \
	MAJOR=$$(echo $$OLD | cut -d. -f1); \
	MINOR=$$(echo $$OLD | cut -d. -f2); \
	PATCH=$$(echo $$OLD | cut -d. -f3); \
	if [ "$(PART)" = "patch" ]; then NEW="$$MAJOR.$$MINOR.$$((PATCH+1))"; \
	elif [ "$(PART)" = "minor" ]; then NEW="$$MAJOR.$$((MINOR+1)).0"; \
	elif [ "$(PART)" = "major" ]; then NEW="$$((MAJOR+1)).0.0"; fi; \
	sed -i "s/^version = \"$$OLD\"/version = \"$$NEW\"/" Cargo.toml; \
	printf "$(GREEN)✓ Версия: $(BOLD)$$OLD → $$NEW$(RESET)\n"; \
	printf "  Не забудь: $(CYAN)git commit -am \"chore: bump version to $$NEW\"$(RESET)\n"

# ── Демон ─────────────────────────────────────────────────────────────────────

daemon-start:  ## Запустить daemon
	$(INSTALL_DIR)/$(BINARY) daemon run &

daemon-stop:  ## Остановить daemon
	$(INSTALL_DIR)/$(BINARY) daemon stop 2>/dev/null || pkill -f "$(BINARY) daemon" || true

daemon-restart: daemon-stop  ## Перезапустить daemon (переиндексация если новый билд)
	@sleep 0.5
	$(INSTALL_DIR)/$(BINARY) daemon run &
	@printf "$(GREEN)✓ Daemon перезапущен$(RESET)\n"

daemon-status:  ## Статус daemon
	$(INSTALL_DIR)/$(BINARY) daemon status 2>/dev/null || printf "daemon не запущен\n"

# ── Help ──────────────────────────────────────────────────────────────────────

help:  ## Показать доступные команды
	@printf "\n$(BOLD)iCode Makefile$(RESET)\n\n"
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
		| awk 'BEGIN {FS = ":.*?## "}; {printf "  $(CYAN)%-20s$(RESET) %s\n", $$1, $$2}'
	@printf "\n$(BOLD)Примеры:$(RESET)\n"
	@printf "  make dev          — собрать, установить, перезапустить daemon\n"
	@printf "  make bump-patch   — поднять версию patch\n"
	@printf "  make test         — запустить тесты\n\n"
