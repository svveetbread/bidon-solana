// Side-effect import: loads ../../.env (D:\bidon-solana\.env, gitignored) before other
// modules read process.env. Import this FIRST in entrypoints (e2e.mjs, probe-photon.mjs).
import dotenv from 'dotenv';
import { fileURLToPath } from 'url';
dotenv.config({ path: fileURLToPath(new URL('../../.env', import.meta.url)) });
