# bidon — e2e против задеплоенной программы

Полный жизненный цикл аукциона живыми транзакциями на реальном рантайме Solana
(локальный валидатор = тот же рантайм, что devnet/mainnet; SPL-программы настоящие).
Клиент на `@coral-xyz/anchor` — он же основа клиента фронта (Phase 2).

## Запуск (локальный валидатор)

В WSL (тулчейн там):
```bash
export PATH=$HOME/.cargo/bin:$HOME/.local/share/solana/install/active_release/bin:$PATH

# 1) поднять валидатор (отдельный долгоживущий процесс)
solana-test-validator --ledger /tmp/bidon-ledger --reset

# 2) в другом окне: собрать (если нужно) и задеплоить
cd /mnt/d/bidon-solana/bidon
anchor build                              # генерит target/idl/bidon.json
solana airdrop 100 -u http://127.0.0.1:8899
anchor deploy --provider.cluster localnet
```

В Windows-шелле (node стоит на `D:\nodejs`; WSL-localhost виден из Windows):
```bash
cd e2e
npm install
node e2e.mjs        # полный сценарий + проверки балансов
node check.mjs      # минимальный смоук (initialize + чтение config)
```

`RPC` можно переопределить: `RPC=https://api.devnet.solana.com node e2e.mjs`
(для публичного devnet нужен SOL на деплойере — фасет блокирует агентов,
см. память `solana-devnet-sol`: PoW-фасет / приватный RPC).

## Сценарий e2e.mjs
p0 = 4 USDC (b1, лузер) · p1 = 10 (b2) + 5 (b3) = 15 USDC (победитель). Итого 19.
Проверяет: лидера, финал по времени, `claim` (14.445 креатору + 0.555 комиссия),
`withdraw` (лузер вернул, победитель — нет), `vault → 0`. Всё зелёное.
