# Следующий этап после админки: chisel и spray

Запрос владельца зафиксирован. Источник поведения и алгоритма: `../endless-drawing-frontend/docs/brushes.md` от корня backend-репозитория. Сейчас frontend поддерживает эти формы локально; backend пока принимает только round/square. Этот документ не объявляет новые значения доступными в API.

## План

- Добавить `chisel` и `spray` в Rust brush enum, JSON validation и storage decoding/serialization. Не подменять неподдерживаемую форму круглой кистью.
- Расширить PostgreSQL CHECK отдельной новой миграцией, не изменять применённую 0001 и миграцию админки.
- Create payload остаётся прежним; seed/particles/angle/density не добавляются. Spray v1 детерминированно восстанавливается из lowercase UUID, width и points; сохранить UUID/points при ack, retry и snapshot.
- Bounds chisel: консервативно `width / sqrt(2) + 1`; spray: `width / 2 + 1`. Не обрезать запас кисти на краю мира.
- Согласовать общий `docs/protocol.md`, fixtures и frontend transport. До backend-поддержки не отправлять новые brush на старый сервер.
- Проверить идемпотентность, JSONB round-trip, thick-stroke viewport intersection, snapshot/reconnect и порядок seq для смешанных форм.
- Public/admin renderer и hit-test должны использовать одну геометрию; при модерации нельзя изображать новые формы как round.
- Spray: максимум 2048 stamps и 98 304 частиц на штрих по текущему frontend v1. Проверить производительность плотных сцен и защитные лимиты перед публичным запуском.

Алгоритм spray — часть визуального контракта. Изменение PRNG, seed, sampling, радиусов/плотности требует согласования/версии, иначе сохранённые штрихи поменяют вид. Текущий приоритет — закончить passkey/MFA и административное скрытие; расширение кистей выполняется отдельным изменением.
