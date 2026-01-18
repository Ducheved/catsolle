# catsolle

<div align="center">

```
   ╭──────────────────────────────────────╮
   │  (=^･ω･^=)  catsolle  (=^･ω･^=)     │
   │     TUI SSH Client with AI Helper    │
   ╰──────────────────────────────────────╯
```

[![CI](https://github.com/user/catsolle/actions/workflows/ci.yml/badge.svg)](https://github.com/user/catsolle/actions/workflows/ci.yml)
[![License: MPL-2.0](https://img.shields.io/badge/License-MPL%202.0-brightgreen.svg)](https://opensource.org/licenses/MPL-2.0)

**[English](#english)** | **[Русский](#русский)**

</div>

---

## English

### What is catsolle?

**catsolle** is a modern terminal-based SSH client built in Rust. It combines the power of a dual-pane file manager (like Midnight Commander) with SSH connectivity, AI assistance, and session recording — all in one lightweight TUI application.

### Why catsolle?

| Problem | catsolle Solution |
|---------|-------------------|
| Switching between terminal and file manager | Dual-pane SFTP browser built right into the session |
| Forgetting complex commands | AI helper suggests commands based on context |
| Managing many SSH connections | Connection manager with import from `~/.ssh/config` |
| Remembering server passwords | Secure keychain with master password encryption |
| Sharing terminal sessions | Session recording in asciicast format |

### Features

- **Dual-Pane File Manager** — Local ↔ Remote file operations via SFTP
- **AI Assistant** — Get command suggestions, error explanations, step-by-step guides
  - Supports Ollama, OpenAI, OpenRouter, Anthropic
  - Tool execution with user approval
  - Context-aware (sees your terminal output)
- **Connection Manager** — Save, organize, and quickly connect to servers
- **SSH Config Import** — One-click import from `~/.ssh/config`
- **Secure Keychain** — Store passwords encrypted with AES-256-GCM
- **Session Recording** — Record terminal sessions for playback
- **Multi-Language** — English and Russian UI
- **Cross-Platform** — Windows, Linux, macOS

### Installation

#### From Release

Download the latest release for your platform from [Releases](https://github.com/user/catsolle/releases).

| Platform | Package |
|----------|---------|
| Windows | `.msi` installer or `.zip` |
| macOS | `.dmg` (Universal binary) |
| Linux | `.deb`, `.rpm`, or `.AppImage` |

#### From Source

```bash
git clone https://github.com/user/catsolle.git
cd catsolle
cargo build --release
./target/release/catsolle
```

### Quick Start

```bash
# Launch TUI
catsolle

# Quick connect
catsolle connect user@hostname

# Generate SSH key
catsolle keys generate --name id_ed25519 --algorithm ed25519

# Initialize config
catsolle config --init
```

### Keyboard Shortcuts

#### Connection Screen
| Key | Action |
|-----|--------|
| `Enter` | Connect |
| `I` | Import from SSH config |
| `N` | New connection |
| `E` | Edit connection |
| `P` | Set password |
| `R` | Reload |
| `F9` | AI settings |
| `?` | Help |
| `Q` | Quit |

#### Session Screen
| Key | Action |
|-----|--------|
| `F10` | AI Helper |
| `F12` | Toggle file panel |
| `Ctrl+T` | Switch focus (terminal/files) |
| `Tab` | Switch panel |
| `F5` | Copy file |
| `Ctrl+Q` | Quit |

### Configuration

Config file location:
- **Linux/macOS**: `~/.config/catsolle/config.toml`
- **Windows**: `%APPDATA%\catsolle\config.toml`

#### AI Configuration Example

```toml
[ai]
enabled = true
provider = "ollama"           # ollama, openai, openrouter, anthropic
endpoint = "http://localhost:11434"
model = "qwen2.5:3b"
temperature = 0.2
streaming = true
agent_enabled = true
tools_enabled = true
```

---

## Русский

### Что такое catsolle?

**catsolle** — это современный SSH-клиент с текстовым интерфейсом, написанный на Rust. Он объединяет двухпанельный файловый менеджер (как Midnight Commander) с SSH-подключением, AI-ассистентом и записью сессий — всё в одном легковесном TUI-приложении.

### Зачем нужен catsolle?

| Проблема | Решение catsolle |
|----------|------------------|
| Переключение между терминалом и файловым менеджером | Двухпанельный SFTP-браузер прямо в сессии |
| Забываешь сложные команды | AI-помощник подсказывает команды по контексту |
| Много SSH-подключений | Менеджер соединений с импортом из `~/.ssh/config` |
| Запоминать пароли от серверов | Безопасное хранилище с шифрованием AES-256-GCM |
| Показать сессию коллегам | Запись сессий в формате asciicast |

### Возможности

- **Двухпанельный файловый менеджер** — Локальные ↔ Удалённые файлы через SFTP
- **AI-ассистент** — Подсказки команд, объяснение ошибок, пошаговые инструкции
  - Поддержка Ollama, OpenAI, OpenRouter, Anthropic
  - Выполнение инструментов с подтверждением
  - Видит контекст терминала
- **Менеджер подключений** — Сохранение, организация, быстрое подключение
- **Импорт SSH Config** — Импорт в один клик из `~/.ssh/config`
- **Безопасное хранилище** — Пароли зашифрованы AES-256-GCM
- **Запись сессий** — Записывайте терминальные сессии для воспроизведения
- **Мультиязычность** — Английский и русский интерфейс
- **Кроссплатформенность** — Windows, Linux, macOS

### Установка

#### Из релиза

Скачайте последний релиз для вашей платформы из [Releases](https://github.com/user/catsolle/releases).

| Платформа | Пакет |
|-----------|-------|
| Windows | `.msi` установщик или `.zip` |
| macOS | `.dmg` (Universal binary) |
| Linux | `.deb`, `.rpm`, или `.AppImage` |

#### Из исходников

```bash
git clone https://github.com/user/catsolle.git
cd catsolle
cargo build --release
./target/release/catsolle
```

### Быстрый старт

```bash
# Запустить TUI
catsolle

# Быстрое подключение
catsolle connect user@hostname

# Сгенерировать SSH-ключ
catsolle keys generate --name id_ed25519 --algorithm ed25519

# Инициализировать конфиг
catsolle config --init
```

### Горячие клавиши

#### Экран подключений
| Клавиша | Действие |
|---------|----------|
| `Enter` | Подключиться |
| `I` | Импорт из SSH config |
| `N` | Новое подключение |
| `E` | Редактировать |
| `P` | Установить пароль |
| `R` | Обновить |
| `F9` | Настройки AI |
| `?` | Помощь |
| `Q` | Выход |

#### Экран сессии
| Клавиша | Действие |
|---------|----------|
| `F10` | AI-помощник |
| `F12` | Показать/скрыть файлы |
| `Ctrl+T` | Переключить фокус (терминал/файлы) |
| `Tab` | Переключить панель |
| `F5` | Копировать файл |
| `Ctrl+Q` | Выход |

### Конфигурация

Расположение файла конфигурации:
- **Linux/macOS**: `~/.config/catsolle/config.toml`
- **Windows**: `%APPDATA%\catsolle\config.toml`

#### Пример настройки AI

```toml
[ai]
enabled = true
provider = "ollama"           # ollama, openai, openrouter, anthropic
endpoint = "http://localhost:11434"
model = "qwen2.5:3b"
temperature = 0.2
streaming = true
agent_enabled = true
tools_enabled = true
```

---

## Architecture / Архитектура

```
catsolle/
├── catsolle          # Main binary / Главный бинарник
├── catsolle-cli      # CLI commands / CLI команды
├── catsolle-config   # Configuration & i18n / Конфигурация и локализация
├── catsolle-core     # Core logic / Ядро логики
├── catsolle-keychain # Secure storage / Безопасное хранилище
├── catsolle-ssh      # SSH/SFTP client / SSH/SFTP клиент
└── catsolle-tui      # Terminal UI / Терминальный интерфейс
```

## License / Лицензия

[MPL-2.0](LICENSE) — Mozilla Public License 2.0

---

<div align="center">

Made with Rust and (=^･ω･^=)

</div>
