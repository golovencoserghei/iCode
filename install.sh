#!/usr/bin/env sh
# iCode installer — скачивает бинарник и запускает setup.
# Использование: curl -sSf https://raw.githubusercontent.com/YOUR/icode/main/install.sh | sh
set -e

REPO="YOUR_GITHUB_ORG/icode"
BINARY="icode"
INSTALL_DIR="${ICODE_INSTALL_DIR:-$HOME/.local/bin}"

# ── Определить платформу ─────────────────────────────────────────────────────

OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Linux)
    case "$ARCH" in
      x86_64)  TARGET="x86_64-unknown-linux-musl" ;;
      aarch64) TARGET="aarch64-unknown-linux-musl" ;;
      *)       echo "Неподдерживаемая архитектура: $ARCH"; exit 1 ;;
    esac
    ;;
  Darwin)
    case "$ARCH" in
      x86_64)  TARGET="x86_64-apple-darwin" ;;
      arm64)   TARGET="aarch64-apple-darwin" ;;
      *)       echo "Неподдерживаемая архитектура: $ARCH"; exit 1 ;;
    esac
    ;;
  *)
    echo "Неподдерживаемая ОС: $OS"
    echo "Windows: скачайте бинарник вручную со страницы Releases."
    exit 1
    ;;
esac

# ── Получить последнюю версию ────────────────────────────────────────────────

LATEST=$(curl -sSf "https://api.github.com/repos/${REPO}/releases/latest" \
  | grep '"tag_name"' | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')

if [ -z "$LATEST" ]; then
  echo "Не удалось получить последнюю версию. Проверьте интернет-соединение."
  exit 1
fi

echo "Устанавливаем iCode ${LATEST} (${TARGET})..."

# ── Скачать ──────────────────────────────────────────────────────────────────

URL="https://github.com/${REPO}/releases/download/${LATEST}/${BINARY}-${TARGET}.tar.gz"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

echo "Скачиваем: $URL"
curl -sSfL "$URL" | tar -xz -C "$TMP"

# ── Установить ───────────────────────────────────────────────────────────────

mkdir -p "$INSTALL_DIR"
mv "$TMP/$BINARY" "$INSTALL_DIR/$BINARY"
chmod +x "$INSTALL_DIR/$BINARY"

echo "✓ Установлено: $INSTALL_DIR/$BINARY"

# ── Проверить PATH ───────────────────────────────────────────────────────────

if ! echo "$PATH" | grep -q "$INSTALL_DIR"; then
  echo ""
  echo "  Добавьте в PATH (и перезапустите терминал):"
  echo "    export PATH=\"\$PATH:$INSTALL_DIR\""
fi

# ── Готово ───────────────────────────────────────────────────────────────────

echo ""
echo "Следующий шаг — настройка для вашего проекта:"
echo ""
echo "  cd /path/to/your/project"
echo "  icode setup"
echo ""
