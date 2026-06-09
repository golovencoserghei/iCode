# iCode — Установка и развёртывание

## Linux (systemd / без systemd)

### 1. Сборка бинарника

```bash
git clone <your-repo-url>
cd icode
cargo build --release -p icode
sudo cp target/release/icode /usr/local/bin/
```

### 2. Создание daemon.toml

```bash
mkdir -p ~/.icode
cat > ~/.icode/daemon.toml << EOF
[daemon]
http_port = 15731

[[paths]]
path = "/path/to/your/project"
EOF
```

### 3. Запуск демона

**Вручную (foreground):**

```bash
ICODE_HOME=~/.icode icode daemon run
```

**Через systemd:**

```ini
# /etc/systemd/system/icode-daemon.service
[Unit]
Description=iCode background indexer daemon
After=network.target

[Service]
Type=simple
User=%i
Environment=ICODE_HOME=/home/%i/.icode
ExecStart=/usr/local/bin/icode daemon run
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl enable --now icode-daemon
sudo systemctl status icode-daemon
```

### 4. Проверка

```bash
icode daemon status
```

## Windows (Scheduled Task)

### 1. Скачать бинарник

Скачайте `icode-windows-x64.zip` из GitHub Releases, распакуйте в `C:\tools\icode\`.

### 2. Настроить переменную окружения

```powershell
setx ICODE_HOME "C:\tools\icode"
```

### 3. Создать daemon.toml

```powershell
@"
[daemon]
http_port = 0

[[paths]]
path = "C:\path\to\project"
"@ | Out-File -FilePath "C:\tools\icode\daemon.toml" -Encoding UTF8
```

### 4. Установить автозапуск

```powershell
powershell -ExecutionPolicy Bypass -File scripts\install-daemon-autostart.ps1 `
  -BinaryPath "C:\tools\icode\icode.exe" `
  -ICodeHome "C:\tools\icode" `
  -StartNow
```

## macOS

```bash
# Сборка
cargo build --release -p icode
cp target/release/icode /usr/local/bin/

# Конфиг
mkdir -p ~/.icode
# Создать ~/.icode/daemon.toml (см. Linux выше)

# Запуск (foreground)
ICODE_HOME=~/.icode icode daemon run
```

Для автозапуска через launchd — создайте plist-файл в `~/Library/LaunchAgents/`.

## Интеграция с Claude Code

### Режим stdio (рекомендуется для одного репозитория)

```json
{
  "mcpServers": {
    "icode": {
      "type": "stdio",
      "command": "/usr/local/bin/icode",
      "args": ["serve", "--path", "."]
    }
  }
}
```

### Режим stdio с несколькими репозиториями

```json
{
  "mcpServers": {
    "icode": {
      "type": "stdio",
      "command": "/usr/local/bin/icode",
      "args": ["serve", "--path", "api=/path/to/api", "--path", "frontend=/path/to/frontend"]
    }
  }
}
```

### Режим HTTP (общий процесс для всех сессий)

```bash
icode serve --transport http --port 8011 --path .
```

```json
{
  "mcpServers": {
    "icode": {
      "type": "http",
      "url": "http://127.0.0.1:8011/mcp"
    }
  }
}
```

## Конфигурация проекта (.icode/config.json)

Создаётся автоматически при первой индексации. Можно настроить вручную:

```json
{
  "exclude_dirs": [
    "vendor", "node_modules", ".git",
    "var", "cache", "logs", "storage/framework"
  ],
  "languages": ["php", "javascript", "typescript"],
  "storage_mode": "disk",
  "memory_max_percent": 25,
  "debounce_ms": 1500,
  "batch_ms": 2000,
  "max_file_size": 1048576,
  "max_code_file_size_bytes": 5242880
}
```

## Проверка индексации

```bash
cd /path/to/project
icode stats
# Файлов: 931
# Функций: 4888
# Классов: 769
# Импортов: 4673
# Вызовов: 20195

icode search-function "configureOptions"
icode get-callers "getId"
icode get-file-summary "src/Controller/UserController.php"
```
