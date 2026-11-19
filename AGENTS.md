# Endless Drawing backend — правила для агентов

- **Не пушить в `main`.** Любое изменение — отдельная ветка и PR. `main` защищён, и каждый merge в него автоматически выкладывается на endlessdrawing.com (`.github/workflows/deploy.yml`, поток — `docs/deployment.md` §1). Мержить PR только по явному «да» владельца.
- Коммиты только в `docs/**` и `*.md` не выкладываются, но тоже идут через PR.
- Протокол и Admin API — нормативные документы `docs/protocol*.md` и `docs/admin-api.md`; frontend и iOS сверяются с ними.
- Секреты (SSH, Telegram, ключи) — только в GitHub environment secrets или runtime env на VPS, никогда в репозитории.
