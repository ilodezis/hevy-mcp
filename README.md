<h1 align="center">Hevy MCP Server</h1>

<p align="center">
  <strong>Bridging Claude AI & Hevy Workout Tracker via Model Context Protocol</strong>
</p>

<p align="center">
  <a href="https://opensource.org/licenses/MIT"><img src="https://img.shields.io/badge/License-MIT-yellow.svg" alt="License"></a>
  <a href="https://www.python.org/downloads/"><img src="https://img.shields.io/badge/Python-3.11+-blue.svg" alt="Python"></a>
  <a href="https://modelcontextprotocol.io"><img src="https://img.shields.io/badge/MCP-Supported-orange.svg" alt="MCP"></a>
</p>

<p align="center">
  <a href="#english">English Documentation</a> • 
  <a href="#russian">Русская Документация</a>
</p>

---

<a name="english"></a>
## 🌐 English Version

### 🚀 Overview
**Hevy MCP Server** is a production-grade, remote Model Context Protocol (MCP) connector designed for Claude (both Claude.ai and desktop applications). It enables Claude to interact directly with **Hevy**—a popular fitness tracking platform. 

Without needing a local database, Claude can seamlessly:
* 📖 **Read** your training logs and analyze workout history.
* 🔍 **Search** exercise templates to build tailored routines.
* ✍️ **Log** completed sessions and edit active routines directly inside the conversation.

---

### 🛠️ Exposed MCP Tools

<details>
<summary>📂 Click to view all available tools</summary>

#### 🔍 Read Operations
* `workout_count` — Retrieve the total number of logged workouts.
* `list_workouts(page, pageSize)` — Fetch recent workouts.
* `get_workout(id)` — Get detailed information about a specific workout.
* `list_routines` — Retrieve custom workout routines.
* `get_routine(id)` — Retrieve details for a specific routine.
* `list_routine_folders` — Fetch routine folders.
* `get_routine_folder(id)` — Get a single routine folder by id.
* `search_exercise_templates(query)` — Search for exercises (e.g., "bench press").
* `get_exercise_template(id)` — Get template details for a specific exercise.
* `get_exercise_history(id, start_date, end_date)` — Every set ever performed for one exercise. The tool for progression, PRs and plateaus — one call instead of paging `list_workouts` ten at a time.
* `get_user_info` — Which Hevy account the connector is wired to.

#### ✍️ Write Operations
* `create_routine(name, notes, warmups, workoutExercises)` — Save a new routine.
* `update_routine(id, name, notes, warmups, workoutExercises)` — Modify an existing routine.
* `create_routine_folder(name)` — Create a folder to organize routines.
* `create_workout(title, description, start_time, end_time, exercises)` — Log a completed workout.
* `update_workout(id, title, description, start_time, end_time, exercises)` — Edit a logged workout.
* `create_exercise_template(name, category, equipment)` — Define a new exercise template.

</details>

*Note: The Hevy API does not support deletion (DELETE requests). Thus, deletions cannot be performed via Claude.*

---

### 💻 Local Development

#### 1. Requirements
* Python 3.11+
* **Hevy PRO** subscription (required for developer API access).

#### 2. Install Dependencies
```bash
python -m venv .venv
# Windows:
.venv\Scripts\activate
# Unix/macOS:
source .venv/bin/activate

pip install -r requirements.txt
```

#### 3. Configuration
Copy the template `.env.example` to `.env` and fill in the parameters:
```bash
cp .env.example .env
```
Key configurations:
* `HEVY_API_KEY`: Generate it at your [Hevy Developer Settings](https://hevy.com/settings?developer).
* `HEVY_MCP_PASSWORD`: The password you will enter once in the browser consent screen when connecting Claude.
* `HEVY_MCP_SESSION_SECRET` & `HEVY_MCP_CLIENT_SECRET`: Generate strong random secrets:
  ```bash
  python -c "import secrets; print(secrets.token_urlsafe(48))"
  ```

#### 4. Spin up
```bash
python run_local.py
```
The server will boot on `http://127.0.0.1:8010`. Health check endpoint is available at `GET /`.
*(Set `HEVY_MCP_COOKIE_SECURE=false` for local HTTP testing).*

#### 5. Run checks
```bash
python check_mcp.py
python -m unittest -v test_mcp
```
The integration suite checks all 17 tools over both legacy and modern MCP,
OAuth login and token exchange, input validation, and Hevy error handling.
It uses test credentials and simulated Hevy responses; no account data changes.
FastMCP is pinned to 4.0.3; FastAPI 0.133+ is required for Starlette 1.x compatibility.

---

### 🐳 Production Deployment

#### 1. Setup Server Directory
Copy the repository files to your VPS (e.g. `/opt/hevy-mcp`). In your production `.env`, ensure you configure:
```env
HEVY_MCP_COOKIE_SECURE=true
```

#### 2. Run with Docker Compose
```bash
docker compose up -d --build
```
The container listens locally at `127.0.0.1:8010`.

#### 3. Configure Nginx Reverse Proxy
Add a server block to handle SSL termination and route traffic to the container:
```nginx
server {
    listen 443 ssl;
    server_name your-subdomain.domain.com;

    ssl_certificate /path/to/fullchain.pem;
    ssl_certificate_key /path/to/privkey.pem;

    location / {
        proxy_pass http://127.0.0.1:8010;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
    }
}
```

#### 4. Link with Claude
1. Navigate to Claude.ai → **Settings** → **Connectors** → **Add custom connector**.
2. Put your connector URL: `https://your-subdomain.domain.com/mcp/`
3. Enter your `HEVY_MCP_PASSWORD` on the consent page and click authorize.

---

<a name="russian"></a>
## 🇷🇺 Русская Версия

### 🚀 Обзор
**Hevy MCP Server** — это готовый к использованию remote-коннектор протокола Model Context Protocol (MCP) для Claude. Он позволяет ИИ напрямую взаимодействовать с вашим аккаунтом в фитнес-трекере **Hevy**.

Без необходимости держать локальную базу данных, Claude может:
* 📖 **Читать** историю тренировок и анализировать ваш прогресс.
* 🔍 **Искать** упражнения по базе Hevy.
* ✍️ **Создавать** шаблоны тренировок (routines) и записывать выполненные сеты (workouts) прямо в чате.

---

### 🛠️ Доступные MCP-инструменты

<details>
<summary>📂 Нажмите, чтобы раскрыть список инструментов</summary>

#### 🔍 Чтение данных
* `workout_count` — Получить общее количество выполненных тренировок.
* `list_workouts(page, pageSize)` — Получить список последних тренировок.
* `get_workout(id)` — Детальная информация о конкретной тренировке.
* `list_routines` — Получить список шаблонов тренировок.
* `get_routine(id)` — Детальная информация о шаблоне тренировки.
* `list_routine_folders` — Список папок с шаблонами.
* `get_routine_folder(id)` — Детальная информация о конкретной папке.
* `search_exercise_templates(query)` — Поиск упражнений (например, "bench press").
* `get_exercise_template(id)` — Детали конкретного упражнения.
* `get_exercise_history(id, start_date, end_date)` — Все подходы по одному упражнению за всё время. Инструмент для прогрессии, рекордов и застоев — один вызов вместо перелистывания `list_workouts` по десять штук.
* `get_user_info` — К какому аккаунту Hevy подключён коннектор.

#### ✍️ Запись данных
* `create_routine(name, notes, warmups, workoutExercises)` — Сохранить новый шаблон тренировки.
* `update_routine(id, name, notes, warmups, workoutExercises)` — Обновить существующий шаблон.
* `create_routine_folder(name)` — Создать папку для шаблонов.
* `create_workout(title, description, start_time, end_time, exercises)` — Записать выполненную тренировку.
* `update_workout(id, title, description, start_time, end_time, exercises)` — Обновить запись тренировки.
* `create_exercise_template(name, category, equipment)` — Создать новое упражнение.

</details>

*Примечание: API сервиса Hevy не поддерживает удаление данных (DELETE-запросы). Удаление данных через Claude невозможно.*

---

### 💻 Локальный запуск

#### 1. Требования
* Python 3.11+
* Подписка **Hevy PRO** (обязательно для доступа к Developer API).

#### 2. Установка зависимостей
```bash
python -m venv .venv
# В Windows:
.venv\Scripts\activate
# В Unix/macOS:
source .venv/bin/activate

pip install -r requirements.txt
```

#### 3. Настройка конфигурации
Скопируйте `.env.example` в `.env` и укажите параметры:
```bash
cp .env.example .env
```
Основные настройки:
* `HEVY_API_KEY`: Токен, полученный в [Hevy Developer Settings](https://hevy.com/settings?developer).
* `HEVY_MCP_PASSWORD`: Пароль для авторизации Claude на веб-странице подтверждения доступа.
* `HEVY_MCP_SESSION_SECRET` и `HEVY_MCP_CLIENT_SECRET`: Сгенерируйте надежные случайные секреты:
  ```bash
  python -c "import secrets; print(secrets.token_urlsafe(48))"
  ```

#### 4. Запуск
```bash
python run_local.py
```
Сервис начнет слушать адрес `http://127.0.0.1:8010`. Health check доступен по пути `GET /`.
*(Для локального запуска без SSL установите `HEVY_MCP_COOKIE_SECURE=false`).*

#### 5. Проверки
```bash
python check_mcp.py
python -m unittest -v test_mcp
```
Тесты проверяют все 17 инструментов через старый и новый протокол MCP,
вход и получение токена OAuth, проверку входных данных и обработку ошибок Hevy.
Используются тестовые ключи и имитация ответов Hevy — данные аккаунта не меняются.
Версия FastMCP закреплена на 4.0.3; для совместимости со Starlette 1.x нужен FastAPI 0.133+.

---

### 🐳 Деплой на VPS

#### 1. Подготовка файлов
Скопируйте файлы проекта на VPS (например, в `/opt/hevy-mcp`). В файле `.env` укажите:
```env
HEVY_MCP_COOKIE_SECURE=true
```

#### 2. Запуск в Docker Compose
```bash
docker compose up -d --build
```
Контейнер будет слушать порт `127.0.0.1:8010`.

#### 3. Настройка Nginx прокси
Настройте серверный блок для терминации SSL и проксирования запросов:
```nginx
server {
    listen 443 ssl;
    server_name your-subdomain.domain.com;

    ssl_certificate /path/to/fullchain.pem;
    ssl_certificate_key /path/to/privkey.pem;

    location / {
        proxy_pass http://127.0.0.1:8010;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
    }
}
```

#### 4. Подключение к Claude
1. В Claude.ai перейдите в **Settings** → **Connectors** → **Add custom connector**.
2. Введите ваш адрес: `https://your-subdomain.domain.com/mcp/`
3. На странице согласия введите `HEVY_MCP_PASSWORD` и подтвердите интеграцию.
