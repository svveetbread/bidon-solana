# bidon на Solana — продукт, реюз фронта, интерфейс программы

Хэндофф для новой сессии. Вместе с `SOLANA_PLAN.md` (стек/стратегия) и `PLAN.md` (test-first статус) — это **всё,
что нужно**. Base-версия (`D:\bidon-base`) — только СПРАВОЧНО (фронт оттуда переиспользуем; контракты/индексер/keeper — нет).

## Продукт
**bidon — аукционы для авторов контента.** Зрители **стейкают USDC** на текстовые предложения; побеждает предложение
с наибольшей суммой; победный пул → автору (минус комиссия `feeBps`); проигравшие забирают ставки (**pull**).
Сеть — **Solana**, стейбл — **USDC** (нативный, Circle). Газлесс в USDC (relayer Kora/Circle Gas Station), вход —
embedded-кошелёк (cheap-first **Web3Auth-Solana**). Авто-раздачу при окончании делает **keeper** (permissionless
`finalize`/`withdraw` — фолбэк, средства не зависают). Комиссия — только `feeBps` (база 3.7% / промо 0.7% / max 10%).

## Программа Anchor (`D:\bidon-solana\bidon`) — текущий интерфейс
- program id (devnet placeholder): `9GSQvMe9CUV217nSVfhBc3VhoQe5RAGS5VGhuBPDsWMW`
- PDA: `config = ["config"]`, `auction = ["auction", id_le8]`, `vault = ["vault", auction_pubkey]` (PDA-токен-аккаунт, authority = auction),
  `proposal = ["proposal", auction_pubkey, pid_le8]`, `bid = ["bid", auction_pubkey, pid_le8, bidder]`.
- **Готово (28 LiteSVM-тестов зелёные; вкл. 1.6 негативы):** `initialize(fee_bps, fee_receiver, usdc_mint)` · `set_config(fee_bps, fee_receiver)`
  (owner-only, `has_one`) · `create_auction(id, min_bid, duration_secs)` (id == `config.auction_count`, инкремент; **создаёт vault**) ·
  `place_bid(proposal_id, content_hash[32], amount)` (НОВОЕ предложение: `pid == auction.proposal_count`, `transfer_checked` USDC→vault) ·
  `raise_bid(proposal_id, amount)` (на СУЩЕСТВУЮЩЕЕ: `init_if_needed` Bid — новый бэкер создаёт, повторный накапливает; обновляет лидера) ·
  `finalize()` (permissionless после end_time, помечает `finalized`; победитель уже в `winner_proposal`; идемпотентно; деньги НЕ двигает) ·
  `claim_winnings()` (permissionless/keeper после finalize: creator получает `winner_amount − fee`, комиссия → fee_receiver; PDA-подпись vault; только раз).
  Гейты ставок: до end_time, `>= min_bid`, mint == config.usdc_mint. Инвариант `vault == Σ ставок`, лидер инкрементальный (без перебора).
- State: `Config{owner, fee_bps, fee_receiver, usdc_mint, auction_count, bump}` ·
  `Auction{id, creator, min_bid, fee_bps, end_time, finalized, creator_paid, total_staked, proposal_count, winner_proposal, winner_amount, bump}` ·
  `Proposal{auction, id, creator, content_hash, total_amount, bump}` · `Bid{auction, proposal, bidder, amount, returned, bump}`.
- **Дальше по PLAN.md:** `withdraw` (loser забирает свою ставку; победившие — нет) → 1.10 сводные инварианты
  (`Σin==Σout`, `vault == Σ невозвращённых`). Раздача — **pull** + keeper (permissionless-фолбэк).
- **Грабли Anchor 1.0:** `CpiContext::new(program_id: Pubkey, accounts)` (раньше брал `AccountInfo`); `init` НЕ требует `rent`-аккаунт
  (через `Rent::get()`); `litesvm-token` тянет `litesvm 0.12` (dev-dep подняли с 0.10); инструкции с многими аккаунтами + CPI
  упираются в стек BPF (4КБ/кадр) → тяжёлые `Account<T>` в **`Box`** (особенно при `init-if-needed`).

## Реюз готового фронта (взять фронт, сменить бэкенд)
Источник: **`D:\bidon-base\frontend`** (React + Vite + TS, тесты vitest). Скопировать в `D:\bidon-solana\frontend`, затем:

**ОСТАВИТЬ как есть (UI + чистая логика, не зависят от чейна):**
- Экраны/вёрстка/стили: список аукционов; страница аукциона (ставка + лента активности); `/wallet`
  (баланс/пополнение/история); создание аукциона.
- Компоненты: `Funding` (L1 адрес+QR / L2 коннект кошелька / L3 онрамп-карточки), `ActivityLog`, `WalletHistory`
  (вёрстка), `QrBlock`, `SecurityPanel` (MFA).
- Чистые хелперы + их тесты: `lib/liveFeed.ts`, `lib/retry.ts`, `lib/usdc.ts` (формат сумм), `lib/browserNotify.ts`,
  `lib/telegram.ts`; хуки `useVisibilityPoll`, `useOutbidNotice`.

**ЗАМЕНИТЬ (слой блокчейна) на Solana-эквиваленты:**
- `wallet.tsx` (Web3Auth-Base + Safe + Pimlico → **Web3Auth-Solana + @solana/web3.js + relayer Kora/Circle**), `providers.tsx`.
- `lib/aa.ts` (viem AA/UserOp → сборка Solana-tx через **Anchor TS-клиент** `@coral-xyz/anchor`).
- `lib/reads.ts` (viem чтение → чтение Solana-аккаунтов), `lib/contracts.ts` (EVM-адреса → program id + USDC mint + Helius RPC).
- `lib/ponder.ts` (Ponder GraphQL → **Helius Enhanced API**), `lib/live.ts` + `useLiveBids` (viem watch → Helius WS / `logsSubscribe`).
- `extWallet.ts` (MetaMask EOA → **Phantom** для L2-перевода USDC).

**Принцип:** UI и продуктовая логика остаются; меняется только «как читаем/пишем сеть». Тексты по-русски, без тех-терминов наружу.

## Решения, которые ОСТАЮТСЯ (durable)
- Выплаты **pull** + **keeper** авто-раздача (само-исполнения нет ни в EVM, ни в Solana — это норма, как у бирж, только non-custodial).
- Вход одной личностью (почта/телефон) → один кошелёк везде; **SFA-по-Telegram-id НЕ делаем** (это другой кошелёк).
- Онрамп: несколько провайдеров карточками + **плейсхолдер РФ/СНГ**; без KYC — перевод готового USDC (L1/L2).
- Длинные аукционы безопасны (pull + permissionless фолбэк).

## Бэклог (на потом — отдельные программы/фичи)
- **Username-как-NFT:** мы выдаём «безопасные» имена (без модерации произвольных), креатор покупает/владеет своим
  (торгуемо). На Solana — **отдельная программа**: Metaplex-NFT имени + мини-реестр уникальности + «мы выпускаем»
  (курируем пул / привязка к верифицированной соцсети). НЕ трогает аукционную программу. После ядра.
- Грант Solana Foundation (бюджет аудита), Solana Pay (QR-пополнение/ставка по ссылке), Dialect-уведомления, 365-дн аукционы.
