# Kora relayer для bidon-zk (газлесс, Solana devnet).
# Базируется на ОФИЦИАЛЬНОМ предсобранном образе Kora (ghcr.io/solana-foundation/kora) — точная версия
# 2.0.5, без вендоринга бинаря в репо и без cargo-сборки на хостинге (она ~15 мин и падает по RAM на free).
# Render просто тянет образ и докидывает наши конфиги.
#
# Render: New → Web Service → репо bidon-solana → rootDir=bidon-zk/kora, Runtime=Docker.
# Секреты в дашборде (НЕ в образе): KORA_PRIVATE_KEY (base58 подписант-релейер), KORA_API_KEY (== VITE_KORA_KEY),
# и (опц.) SOLANA_RPC. signers.toml ссылается на KORA_PRIVATE_KEY через private_key_env.
#
# Обновление версии: поменяй тег ниже (см. github.com/solana-foundation/kora/releases + ghcr-теги).

FROM ghcr.io/solana-foundation/kora:v2.0.5

WORKDIR /app
COPY kora.toml signers.toml ./

# Сбрасываем унаследованный entrypoint и запускаем с нашим конфигом. Render даёт $PORT;
# RPC — devnet по умолчанию (или свой через SOLANA_RPC). Бинарь `kora` уже в образе (на PATH).
ENTRYPOINT []
CMD kora --config kora.toml --rpc-url ${SOLANA_RPC:-https://api.devnet.solana.com} \
    rpc start --signers-config signers.toml --port ${PORT:-8080}
