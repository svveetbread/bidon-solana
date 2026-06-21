# Kora relayer для bidon-zk (газлесс, Solana devnet).
# Использует ПРЕДСОБРАННЫЙ бинарь kora-bin (kora-cli 2.0.5, x86_64-linux, glibc≥2.34, strip 16МБ) — Render
# просто копирует его, БЕЗ Rust-сборки на хостинге (она долгая ~15 мин, требует свежий Rust и падает по RAM
# на free-плане). bookworm даёт glibc 2.36 ≥ 2.34 + libssl3 (OpenSSL 3) — бинарь совместим.
#
# Render: New → Web Service → репо bidon-solana → rootDir=bidon-zk/kora, Runtime=Docker.
# Секреты в дашборде (НЕ в образе): KORA_PRIVATE_KEY (base58 подписант-релейер), KORA_API_KEY (== VITE_KORA_KEY).
#
# Пересборка бинаря (Linux x86_64, Rust ≥1.8x; локально собрано 1.96):
#   cargo install kora-cli --version 2.0.5 && strip "$(which kora)" -o bidon-zk/kora/kora-bin
# (glibc собранного бинаря должен быть ≤ glibc базового образа — bookworm=2.36).

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY kora-bin /usr/local/bin/kora
COPY kora.toml signers.toml ./
RUN chmod +x /usr/local/bin/kora

# Render даёт $PORT. RPC — devnet (или свой через SOLANA_RPC). Подписант — из KORA_PRIVATE_KEY (env-секрет;
# signers.toml ссылается на него через private_key_env). API-ключ: для прода задайте KORA_API_KEY и УБЕРИТЕ
# хардкод api_key из kora.toml; на devnet можно оставить как есть.
CMD kora --config kora.toml --rpc-url ${SOLANA_RPC:-https://api.devnet.solana.com} \
    rpc start --port ${PORT:-8080} --signers-config signers.toml
