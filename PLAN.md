# PLAN — bidon на Solana (test-first execution)

Стратегия и стек — в `SOLANA_PLAN.md`. Здесь — пошаговый план сборки **test-first**: каждый шаг = сначала
падающий тест, потом код, потом зелёно. Идём сверху вниз, чекбоксы отмечаем по ходу.

**Принципы:** программа минимальная (дешевле аудит); инварианты как на Base (`Σ usdc_in == Σ usdc_out`,
`vault == Σ невозвращённых ставок`); выплаты **pull**; keeper — удобство, средства всегда вынимаются permissionless.
Юнит-тесты Phase 1 гоняем на **локальном валидаторе** (`anchor test`, без devnet/SOL — быстро и бесплатно);
devnet — только для де-риск-спайка (0.3–0.5) и финального e2e (5.1).

---

## ▶ Текущий шаг
Сделано: тулчейн ✅, **0.2** ✅, **1.1–1.5 + 1.7** ✅ (**20 LiteSVM-тестов зелёные**). 1.4 — `anchor-spl`,
USDC-vault, `Proposal`/`Bid`, `transfer_checked`. 1.5 — `raise_bid` (`init_if_needed` Bid) + инкрементальный лидер.
1.7 — `finalize` (permissionless после end_time, помечает завершённым; победитель = `winner_proposal`; идемпотентно;
пустой аукцион). Тест-харнес вынесен в **`tests/common/mod.rs`** (1.8–1.10 переиспользуют).
Дальше (вне порядка, по согласованию) — **1.6** добрать негативы: `raise_bid` ниже min/после end (явные тесты),
граничные случаи. Прим.: «на финализированном — reject» для `place_bid`/`raise_bid` **уже** покрыто временным гейтом
(`now < end_time`), т.к. `finalize` возможен лишь при `now >= end_time` → отдельная проверка `!finalized` избыточна.

**Грабли:** `place_bid`/`raise_bid` упёрлись в лимит стека BPF (4КБ/кадр) после включения `init-if-needed` →
тяжёлые `Account<T>` обёрнуты в **`Box`** (данные на куче). Идемпотентный повторный вызов нельзя слать той же
tx (одинаковая подпись → `AlreadyProcessed`) — в тесте второй вызов другим кранкером.

## Phase 0 — Де-риск (gate перед полным переписком)
- [x] 0.1 Тулчейн в WSL: rust/cargo 1.96, solana-cli 4.0.1, apt-deps (cc/openssl/pkg-config/udev). Anchor — в установке.
- [x] 0.2 `anchor init bidon` → `anchor build` собирается + тесты зелёные (LiteSVM, in-process). **Assumption #3 подтверждено: собираем под Solana.**
- [ ] 0.3 Вход в TMA: минимальный **Web3Auth-Solana** логин на тест-странице → проверить в твоём Telegram (один identity, без попап-зависона). Cheap-first.
- [ ] 0.4 Газлесс: USDC-fee через **Kora/Circle** на тестовой tx (юзер без SOL).
- [ ] 0.5 Helius: получаем события нашей программы (Enhanced API / WS).
- **Гейт:** все 5 зелёные → коммитим полный переписк.

## Phase 1 — Программа Anchor (ядро), строго test-first
Дизайн (порт `Bidon.sol`):
- `Config` (PDA, singleton): owner, fee_bps, fee_receiver, usdc_mint.
- `Auction` (PDA seed `["auction", id]`): creator, min_bid, fee_bps-снапшот, end_time, finalized, creator_paid, winner_proposal.
- `Proposal` (PDA `["proposal", auction, pid]`): hash/текст, total_amount.
- `Bid` (PDA `["bid", auction, pid, bidder]`): amount, returned.
- `Vault` (PDA-ATA): держит USDC аукциона.

Инструкции (каждая: падающий тест → код → зелёно):
- [x] 1.1 `initialize` Config (owner/fee_bps/fee_receiver/usdc_mint, fee ≤ 10%). 2 теста зелёные: поля выставлены; fee>10% отклонён.
- [x] 1.2 `set_config` (owner): 3 теста зелёные — owner меняет fee/receiver; не-владелец отклонён (has_one); fee>10% отклонён.
- [x] 1.3 `create_auction` (id из счётчика, min_bid, duration; снапшот fee_bps; end_time). 3 теста: поля+счётчик; неверный id; нулевая длительность. (vault отложен на 1.4.)
- [x] 1.4 `place_bid` первый (новое предложение). 3 теста: USDC→vault; Proposal+Bid созданы, поля верны, `total_staked`/`proposal_count`++, инвариант `vault == Σ ставок`; ниже min — reject; после end_time — reject. (Vault создан в `create_auction`; `transfer_checked` без deprecation-warning.)
- [x] 1.5 `raise_bid` (**отдельная инструкция** на существующее предложение, `init_if_needed` на Bid) + инкрементальный лидер. 4 теста: накопление своим биддером; новый бэкер чужого предложения; смена лидера (перебой по сумме); после end_time — reject. (Box-аккаунты из-за лимита стека BPF.)
- [ ] 1.6 Валидации `place_bid`: после end_time — reject; ниже min — reject; на финализированном — reject.
- [x] 1.7 `finalize` (permissionless, после end_time). 4 теста: winner = `winner_proposal` (max total); reject до end; идемпотентно (повторный вызов другим кранкером — no-op); пустой аукцион (без победителя). Деньги НЕ двигает — это claim/withdraw.
- [ ] 1.8 `claim_winnings` (creator, pull). Тест: creator получает (pool − fee); fee → fee_receiver; только победный пул; только раз; только creator.
- [ ] 1.9 `withdraw` (проигравший, pull). Тест: лузер получает назад; победитель не может; двойной withdraw нельзя; флаг returned.
- [ ] 1.10 Инварианты (сводные тесты): `Σ in == Σ out`; `vault == Σ невозвращённых`; fee-математика; (опц.) закрытие аккаунтов с возвратом rent.
- [ ] 1.11 `anchor test` целиком зелёный.

## Phase 2 — Фронт (Solana)
- [ ] 2.1 Клиент программы (`@coral-xyz/anchor` TS) + `@solana/web3.js` + `@solana/spl-token`. Хелперы + vitest (переиспользуем чистые: liveFeed/retry/usdc).
- [ ] 2.2 Вход: Web3Auth-Solana (по итогу 0.3) — провайдер, login/logout, адрес, баланс USDC.
- [ ] 2.3 Газлесс-обёртка (Kora/Circle) для всех пользовательских tx.
- [ ] 2.4 Экраны (порт UI): список аукционов; страница аукциона (ставка + лента); /wallet (баланс/пополнение/история); создание.
- [ ] 2.5 Пополнение: адрес + **Solana Pay** QR (USDC на Solana) — L1; онрамп-карточки — L3 (заглушки).

## Phase 3 — Индексер + keeper
- [ ] 3.1 Данные: Helius **Enhanced API** (история/wallet) + стандартный **WS** (лента). Отдельный Ponder не нужен.
- [ ] 3.2 keeper: порт автофинализации/автовозврата на `@solana/web3.js` + relayer; permissionless-крэнк. `node --test` на чистую логику.
- [ ] 3.3 TG-уведомления: переиспользуем логику (`detectOutbids`/`verifyInitData`), вызовы под Solana.

## Phase 4 — Онрамп/пополнение
- [ ] 4.1 Onramper/Transak (Solana, USDC) карточками за флагами + локальный РФ-плейсхолдер; Solana Pay для крипто-пополнения.

## Phase 5 — Devnet e2e + аудит-преп
- [ ] 5.1 Полный сценарий на devnet: создание → ставки → финал → выплаты → возвраты.
- [ ] 5.2 Минимизация программы + security-чеклист (signer/owner/PDA-seeds) + бюджет аудита + заявка на грант Solana Foundation.

## Phase 6 — Mainnet (mainnet-beta) — делает пользователь
- [ ] 6.1 Деплой программы (ключи пользователя — Claude не трогает), authority → **Squads**, repoint фронта/keeper на mainnet-beta + USDC mint mainnet.
