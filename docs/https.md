# Caddy / HTTPS: сначала закрытая приёмка

## Текущее состояние

На VPS проверены: backend `13f3ab1`, frontend `623c5a6`, `/readyz` 200, `/admin/` 200, admin session API 503 (намеренно выключен). Установлен Caddy **2.11.9**, но основной и API services пока **masked/inactive**, ACME не запускался. Слушают только SSH и app `127.0.0.1:8080`; 80/443 пока без listener. Публичный DNS A при последней проверке 17.11.2026 возвращает Cloudflare-адреса `172.67.169.64` / `104.21.27.115`, AAAA/CAA ответов нет; HTTP и HTTPS `/readyz` снаружи дают Cloudflare 521. Это не доказывает ни правильность, ни ошибку скрытого origin. Нужна проверка записи в панели DNS. UFW уже разрешает 80/tcp, 443/tcp+udp и SSH; эти правила на данном этапе не менялись.

`deploy/caddy/Caddyfile`, private access snippet и systemd drop-in установлены; разрешены **только loopback peers**. Конфигурация успешно проверена native `caddy validate` от пользователя **caddy**, который по-прежнему не может прочитать server `runtime.env`. `/etc/endlessdrawing` остался `0700`, несекретный snippet размещён отдельно в `/etc/caddy/endlessdrawing/access.caddy`. `install-operations.sh` сам по-прежнему кладёт только `.prepared` файлы и не включает Caddy.

Private gate повторно проверен в Docker **от непривилегированного UID**: внешний peer получает 403, подмена X-Forwarded-For/X-Real-IP/Forwarded не помогает; явное разрешение достигает только test upstream. Тест использовал Caddy 2.11.9, HTTP loopback, не настоящий ACME certificate.

## DNS и доступ

1. В панели домена проверить A/origin для `endlessdrawing.com` → VPS `212.147.244.161`. Если есть AAAA, он должен вести на реально настроенный IPv6, иначе удалить/исправить. CAA не должен запрещать выбранный ACME issuer.
2. Для первой private peer-IP приёмки проще временно **DNS only**. При Cloudflare proxy Caddy видит peer Cloudflare, а не владельца. Нельзя ради прохода whitelist добавить весь Cloudflare: это откроет приложение всем через proxy. Альтернативный внешний Access gate требует отдельной корректной настройки/проверки.
3. TCP 80/443 должны доходить до Caddy для HTTPS/ACME. SSH 22 сохраняется, backend 8080 остаётся loopback, PostgreSQL вообще не публикуется. Не делать blind `ufw reset/enable` и не терять SSH; отдельно проверить provider firewall.
4. В `ENDLESSDRAWING_ALLOWED_IPS` разрешить только свой фактический public IP плюс loopback. Default разрешает только loopback и закрывает приложение для всего внешнего мира. Это match по socket peer, не доверие входящему заголовку.

## Установка Caddy без окна публичного приложения

Это действия доверенного root-оператора после review DNS/firewall/конфига. Не заменять существующий multi-site Caddyfile, если к моменту установки на VPS появятся другие сайты.

- Сначала замаскировать **только `caddy.service` и `caddy-api.service`**, затем установить пакет: это не даст package post-install поднять стандартный welcome site до закрытого конфига. На VPS установлен официальный GitHub Release `.deb` **2.11.9 linux/amd64**, проверенный по SHA-256 из authenticated GitHub Release API: `bbea4aeaadc86b233c168cc33bad9daa1cbce756dc08949b783b45398f8e65cb`. Дистрибутивный candidate был старым 2.6.2; не заменять им пакет. Обновления Caddy планировать отдельно с review актуального release/hash — сторонний APT repo не добавлялся.
- Установить подготовленный Caddyfile в `/etc/caddy/Caddyfile`, private access snippet в `/etc/caddy/endlessdrawing/access.caddy` (`root:root 0644`, parent `0755`, доступен caddy user). **Не менять `/etc/endlessdrawing` (0700)**: там серверные credentials; Caddy не должен читать этот каталог. Отдельный `caddy.env` внутри него читает PID 1 через EnvironmentFile, а не caddy user.
- Установить `deploy/caddy/service.conf` в `/etc/systemd/system/caddy.service.d/endlessdrawing.conf`. Он также убирает vendor `--environ`, чтобы startup не логировал environment. Отдельный `/etc/endlessdrawing/caddy/caddy.env` содержит domain/upstream/allowed peer IP, **не runtime.env с DB credentials**.
- Проверить конфигурацию с тем же env, включая native запуск validate **от UID caddy**. Только после сверки DNS/origin снять mask **основного** `caddy.service` и явно включить/запустить его; `caddy-api.service` остаётся masked. Наличие certificate отдельно подтвердить обычным `curl`/браузером, без `-k`; внешний peer до разрешения должен получать 403, spoofed forwarded headers не должны помогать. UFW/SSH и DB/backend exposure не менять.

Пример проверки (Caddy с поддержкой `--envfile`, проверить CLI установленной версии):

```bash
sudo caddy validate --config /etc/caddy/Caddyfile \
  --adapter caddyfile --envfile /etc/endlessdrawing/caddy/caddy.env
```

Caddy автоматически получает/продлевает сертификат и проксирует HTTP + WebSocket на `127.0.0.1:8080`; вручную обслуживать сертификат через openssl/certbot не нужно. Встроенный ACME challenge handler может быть доступен CA, но приложение остаётся за private gate. Не включать HSTS preload/includeSubDomains до принятия конечной конфигурации.

При Cloudflare в дальнейшем нужен **Full (strict)**, корректный origin TLS и WebSocket; не Flexible. Admin HTML/API исключить из CDN caching, не переопределять no-store правилом Cache Everything. Trusted client IP/rate limits через proxy — отдельный проверяемый слой, сейчас произвольному X-Forwarded-For не доверяем.

## Настоящий admin / Touch ID

Только после доверенного HTTPS, без browser ignore-certificate-errors и `curl -k`:

```dotenv
ADMIN_ENABLED=true
ADMIN_ORIGIN=https://endlessdrawing.com
ADMIN_RP_ID=endlessdrawing.com
ALLOWED_ORIGINS=https://endlessdrawing.com
```

Изменить только эти параметры серверного runtime.env через доверенный root editor, не печатать/копировать остальные credentials. Применить штатным ручным Actions deploy, а не обходом pipeline. Затем оператор сам выполняет интерактивный `canvas-admin create`, вводит password/enrollment token в HTTPS admin форму и регистрирует основной/резервный passkeys. Capability и пароль агент не захватывает. [Runbook](admin-operations.md).

Проверить вход обоими credentials, отмену, неверный пароль, logout/recovery, cookie flags, аудит/hide, два реальных клиента и reload. Синхронизированный один iCloud credential на Mac/iPhone не считать двумя независимыми регистрациями. Сервер проверяет UV, не обещает fingerprint-only.

Снятие private gate — отдельное решение **после** [publication checklist](publication.md), не следствие наличия сертификата. Сам HTTPS не подключает публичный холст к backend, не делает внешний backup и не создаёт администратора.
