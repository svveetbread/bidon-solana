# Kora relayer для bidon-zk (газлесс, Solana devnet). Rust-бинарь kora-cli 2.0.5.
# Render: New → Web Service → этот репозиторий → rootDir=bidon-zk/kora, Runtime=Docker.
# Секреты в дашборде (НЕ в образе): KORA_PRIVATE_KEY (base58 подписант-релейер), KORA_API_KEY (== фронтовый VITE_KORA_KEY).
#
# ⚠️ Сборка тяжёлая (cargo install ~10-15 мин, много зависимостей). На free-плане может не хватить памяти —
# при сбое поднимите план билда или используйте предсобранный бинарь (GitHub releases kora), если доступен.

FROM rust:1.83-slim AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN cargo install kora-cli --version 2.0.5 --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/cargo/bin/kora /usr/local/bin/kora
WORKDIR /app
COPY kora.toml signers.toml ./

# Render даёт $PORT. RPC — devnet (или свой через SOLANA_RPC). Подписант — из KORA_PRIVATE_KEY (env-секрет;
# signers.toml ссылается на него через private_key_env). API-ключ: для прода задайте KORA_API_KEY и УБЕРИТЕ
# хардкод api_key из kora.toml; на devnet можно оставить как есть.
CMD kora --config kora.toml --rpc-url ${SOLANA_RPC:-https://api.devnet.solana.com} \
    rpc start --port ${PORT:-8080} --signers-config signers.toml
