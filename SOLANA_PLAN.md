# SOLANA_PLAN — миграция bidon с Base на Solana

Статус: **решение принимается** (склоняемся к Solana). Это план «всё, с чем столкнёмся» + как выжать
преимущества Solana. Главный принцип: **сначала проверяем (Phase 0), потом переписываем.** Это **последняя**
смена стека (после TON → USDT-на-TON → Base → Solana) — дальше строим и релизим.

> Концепт продукта НЕ меняется: аукционы для авторов, зрители **стейкают USDC** на текстовые предложения,
> победный пул → автору (минус `feeBps`), проигравшие забирают ставки (pull). Меняется только сеть и стек.

---

## 0. Почему Solana (и почему сейчас)

- **USDC на Solana — нативный и первоклассный** (Circle выпускает прямо на Solana, ~$8.6B, не обёртка) →
  пополнение/вывод/онрамп резко проще (наша главная боль на Base, где USDC нишевый). [Everstake](https://everstake.one/resources/blog/usdc-on-solana-stablecoin-volume)
- **Розничная аудитория и узнаваемость** выше Base/Polygon (active addresses ~4–5M/день, DEX-объём часто #1). [BitKE](https://bitcoinke.io/2026/03/solana-in-2026-so-far/)
- **Комиссии ~$0.0005**, газлесс в USDC есть (Kora/Circle Gas Station).
- **Стек, вероятно, упрощается:** уходит связка Safe + ERC-4337 + Pimlico-paymaster (источник половины наших
  болей: EOA≠Safe, онрамп-на-Safe). Embedded-кошелёк держит USDC напрямую, relayer платит комиссию.
- **Тайминг:** менять чейн можно только ДО мейннета. Сейчас последствий почти нет; после запуска — кошмар.

Минусы (честно): аудит/Rust дороже и реже Solidity; Rust медленнее писать; теряем зрелость EVM-тулинга
(Foundry/OZ) и прямой Coinbase-онрамп-на-Base; нужно ПЕРЕпроверить кошелёк/газлесс/TMA (на Base обожглись на вере).

---

## 1. Карта стека: Base (сейчас) → Solana (станет)

| Слой | Base сейчас | Solana станет | Риск переписка |
|---|---|---|---|
| Контракт | Solidity/Foundry `Bidon.sol` | **Rust/Anchor** программа (единый реестр) | высокий (новый язык) |
| Стейбл | USDC (Base) | **USDC (Solana, нативный Circle)** | нет (тот же актив) |
| Вход/кошелёк | Web3Auth embedded + **Safe** | embedded-кошелёк (**Privy**/Dynamic), **без Safe-слоя** | средний |
| Газлесс | Pimlico USDC-paymaster + ERC-4337 | **fee-payer relayer** (Kora/Circle Gas Station), оплата в USDC | средний |
| Фронт ончейн-слой | viem/wagmi/OnchainKit | **@solana/web3.js + @solana/spl-token** + SDK кошелька | средний |
| Индексер | Ponder (GraphQL) | **Helius webhooks** (+ наш store) | низкий |
| Keeper (автофинализация/возврат) | Node + viem | Node + @solana/web3.js (логика та же) | низкий |
| Онрамп | Transak/Coinbase→Base | Onramper/Transak/Coinbase→Solana (USDC первоклассный) | низкий |
| Сеть | Base Sepolia / mainnet | Solana **devnet** / **mainnet-beta** | — |
| Мультисиг казны/апгрейда | Gnosis Safe | **Squads** (Solana multisig) | низкий |

---

## 2. Что переносится «бесплатно» (не трогаем)

Продуктовый дизайн и большая часть фронта: React-компоненты/верстка, экраны (аукцион, /wallet, создание),
правила аукциона, тексты, лента активности, **логика keeper** (автовозврат всем при окончании), **логика
TG-уведомлений** (детект перебоев, `verifyInitData`), docs. Меняется только слой «как говорим с блокчейном и
кошельком».

---

## 3. Ключевые решения Solana (и как выжать преимущества)

### 3.1 Программа (Anchor) — дизайн
- **Единая программа** (как единый реестр `Bidon.sol`, не «контракт-на-аукцион»).
- **Аккаунты (PDA):** `Auction` (состояние лота), `Proposal`/`Bid` (ставка/предложение), **`Vault`** —
  PDA-owned ATA, держит застейканный USDC.
- **Инструкции:** `create_auction`, `place_bid` (transfer USDC → vault), `finalize`, `claim_winnings`
  (creator), `withdraw` (проигравший, **pull**). Permissionless `finalize`/`withdraw` как fallback (как сейчас).
- **Инварианты те же:** Σusdc_in == Σusdc_out; held == Σ amount. Тесты — первыми (test-first).
- Проторённый паттерн: PDA-vault + SPL transfer (есть готовые USDC-vault примеры).
  [anchor-escrow](https://github.com/ironaddicteddog/anchor-escrow) · [sol-vault](https://github.com/Clish254/sol-vault)
- Преимущество: эскроу прозрачно на PDA; программу держим **минимальной** (дешевле аудит).

### 3.2 Вход + кошелёк — решаем cheap-first тестом в Phase 0
**Важно:** преимущества Solana (газлесс/USDC/дешёвые комиссии/простой стек) от провайдера кошелька НЕ зависят —
получим их с любым. Поэтому кошелёк выбираем по **цене + работе в TMA**, не переплачивая.
- **Кандидат №1 — Web3Auth** (он же теперь «MetaMask Embedded Wallets»): дёшево (**$69/мес за 3000 MAW**, ~1000
  бесплатно), мы его уже знаем, Solana поддерживает, и **наш существующий проект, вероятно, переиспользуется**
  (Web3Auth чейн-агностичен) → ноль нового сетапа. Риск: именно его попап в TMA был нашей болью → **первым делом
  проверить вход в твоём Telegram** (redirect / их TMA-режим). Работает — берём, это самый дешёвый путь.
- **Фолбэк (если TMA у Web3Auth не зайдёт) — провайдер с inline-входом (без попапа):** Openfort (free 2000
  операций/мес), Dynamic (есть TMA-поддержка), Privy (inline OTP, но дорого — **$299/мес за 2500 MAU**, берём
  только если дешёвые не тянут TMA). [обзор/цены](https://www.openfort.io/blog/best-solana-wallets-for-developers)
- **Принцип, который чинит TMA:** одна личность (почта/телефон) + ввод **без попапа** (inline или рабочий redirect)
  → один кошелёк на ПК и в Telegram. **Проверяем в Phase 0, не верим на слово.**
- **Safe-слой не нужен** — embedded-кошелёк держит USDC напрямую.

### 3.3 Газлесс в USDC
- **Kora** (Solana Foundation) или **Circle Gas Station**: relayer = fee payer, co-sign, юзер без SOL,
  комиссия в USDC. [QuickNode/Kora](https://www.quicknode.com/guides/solana-development/transactions/kora) · [Circle](https://www.circle.com/blog/how-circles-gas-station-uses-fee-payers-to-enable-gasless-transactions-on-solana)
- Проще ERC-4337; но проверить связку с выбранным кошельком (Phase 0).

### 3.4 Индексер / данные — Helius (отдельный Ponder-сервис, скорее всего, НЕ нужен)
- **История + /wallet + keeper:** **Helius Enhanced Transactions API** (Parse Transaction / Parse History по
  адресу/программе) — on-demand парсинг, покрывает историю ставок/пополнений и опрос keeper'ом (это те URL, что прислал юзер).
- **Live-лента ставок (браузер):** **LaserStream WebSocket** `transactionSubscribe` на нашу программу → пуш новых
  ставок (заменяет наш wss watchContractEvent).
- **Пуш-триггеры в keeper:** **Webhooks** (Helius шлёт распарсенную tx на endpoint) — нужны idempotency/retry.
- **LaserStream gRPC** (firehose блоков/тx/аккаунтов) — для низколатентных бэкендов/трейдинга; **нам overkill**, не берём.
  [LaserStream](https://www.helius.dev/blog/introducing-laserstream) · [Helius webhooks](https://www.helius.dev/docs/webhooks)
- **Итог:** Helius заменяет и Ponder, и часть поллинга keeper → меньше узлов в системе.

### 3.5 Онрамп / пополнение
- USDC-Solana первоклассный → Onramper/Transak/Coinbase легко шлют на Solana-адрес. + локальный РФ-плейсхолдер.
- Без KYC — перевод готового USDC на Solana-адрес (аналог L1/L2). Концепт «несколько онрампов карточками» сохраняем.

---

## 4. Solana-специфика, с которой ТОЧНО столкнёмся (то, чего нет в EVM)

- **Rent:** аккаунты стоят SOL за хранение (rent-exempt минимум). **Решить: кто платит rent** за `Auction`/`Bid`
  PDA — юзер, relayer или казна. (Можно закрывать аккаунты и возвращать rent после выплат.)
- **Аккаунтная модель:** нет mapping'ов — всё аккаунты/PDA; размер аккаунта фиксируется при создании (или realloc).
- **Явные signer/owner-проверки:** нет `msg.sender` — все аккаунты передаются явно; безопасность = ручные
  проверки signer/owner/PDA-seeds. [Solana security patterns](https://angrypacifist.substack.com/p/solana-security-patterns)
- **Compute budget** (лимит вычислений на tx) и **лимит размера транзакции** (~1232 байта) → батчинг иначе,
  чем в EVM (возможно Address Lookup Tables + versioned tx).
- **SPL-токены:** USDC = свой mint, 6 знаков; нужны ATA (associated token accounts), их создание/оплата.
- **Reentrancy** в Solana-модели нет, но есть свои классы багов (account confusion, missing signer/owner check).
- **Апгрейд программы:** upgradeable BPF; authority апгрейда на mainnet → **Squads-мультисиг** (аналог Safe-owner).
- **Devnet USDC:** тестовый mint/faucet (не тот же, что mainnet).

---

## 5. Фазы

- **Phase 0 — Де-риск-спайк (1–3 дня), GO/NO-GO.** Проверить сквозняком 4 убийственных допущения:
  1. Вход embedded-кошельком (почта/OTP) **в Telegram-мини-аппе** (Privy → проверить inline без попапа).
  2. Газлесс в USDC (Kora/Circle) с этим кошельком — юзер без SOL.
  3. Скелет Anchor-программы (place_bid → vault, withdraw) + 1 тест на devnet.
  4. Helius отдаёт нужные события.
  → Всё сошлось → коммитим. Сломалось → нашли сейчас, дёшево.
- **Phase 1 — Программа (Anchor), test-first.** Порт логики `Bidon` на инструкции + unit/инвариант-тесты.
- **Phase 2 — Фронт-интеграция.** viem/wagmi/Web3Auth/Safe/Pimlico → Privy + @solana/web3.js + relayer; перенос экранов.
- **Phase 3 — Индексер + keeper.** Helius (или RPC-поллинг) + вызовы keeper на Solana.
- **Phase 4 — Онрамп/пополнение** (Onramper + локальный РФ-плейсхолдер).
- **Phase 5 — Devnet e2e + подготовка к аудиту** (минимальная программа, паттерны, бюджет аудита).
- **Phase 6 — Mainnet (mainnet-beta).** Ключи/деплой — **пользователь** (Claude mainnet-ключи не трогает);
  authority → Squads-мультисиг.

---

## 6. Открытые вопросы (закрыть в Phase 0 / по ходу)

- Кошелёк: **Web3Auth cheap-first** (переиспользуем проект) → проверить TMA-вход в Phase 0; не зайдёт — inline-фолбэк
  (Openfort/Dynamic; Privy дорогой — крайний случай). Критерий: работает в TMA + дёшево.
- Газлесс: **Kora vs Circle Gas Station**.
- **Кто платит rent** за аккаунты (и закрываем ли их после выплат, возвращая rent).
- Devnet USDC — источник тест-токена.
- Аудитор и бюджет (Anchor-программа).
- Lock-in провайдера кошелька: проверить экспорт ключа/некостадиальность.

---

## 7. Что НЕ меняется

Концепт продукта, комиссия `feeBps` (база/промо/max), **pull-выплаты**, автовозврат всем через keeper,
UX и тексты, политика длинных аукционов. Меняется сеть и технический слой — не продукт.
