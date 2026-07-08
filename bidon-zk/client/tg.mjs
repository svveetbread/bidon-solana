// tg.mjs — Telegram ops-хаб кипера: пуш-алерты (новый аукцион / низкий баланс / рассчитан) + on-demand
// команды (/status, /help). ПОЛНОСТЬЮ best-effort: любой сбой/таймаут Telegram НЕ роняет и НЕ блокирует
// резолв-луп кипера (все вызовы в try/catch + AbortController-таймаут). Конфиг из env:
//   TELEGRAM_BOT_TOKEN — токен бота (секрет; на Railway).
//   TELEGRAM_CHAT_ID   — чат оператора, куда шлём алерты и отвечаем на команды.
// Без токена/чата модуль — no-op (кипер работает как раньше, наблюдаемость просто выключена).
const TOKEN = process.env.TELEGRAM_BOT_TOKEN || '';
const CHAT_ID = process.env.TELEGRAM_CHAT_ID || '';
const API = TOKEN ? `https://api.telegram.org/bot${TOKEN}` : '';

export const tgEnabled = () => Boolean(TOKEN && CHAT_ID);
export const tgToken = () => TOKEN; // для команд-луп (getUpdates работает и без CHAT_ID, чтобы узнать свой id)

async function tgCall(method, body, timeoutMs = 8000) {
  if (!API) return null;
  const ctl = new AbortController();
  const t = setTimeout(() => ctl.abort(), timeoutMs);
  try {
    const r = await fetch(`${API}/${method}`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
      signal: ctl.signal,
    });
    return await r.json();
  } catch {
    return null; // сеть/таймаут — молча (кипер не должен падать из-за Telegram)
  } finally {
    clearTimeout(t);
  }
}

/** Алерт оператору (в TELEGRAM_CHAT_ID). Best-effort. */
export async function tgSend(text) {
  if (!tgEnabled()) return;
  await tgCall('sendMessage', {
    chat_id: CHAT_ID,
    text,
    parse_mode: 'HTML',
    disable_web_page_preview: true,
  });
}

/** Ответ в конкретный чат (для обработки команд). */
export async function tgReply(chatId, text) {
  if (!API) return;
  await tgCall('sendMessage', {
    chat_id: chatId,
    text,
    parse_mode: 'HTML',
    disable_web_page_preview: true,
  });
}

/** long-poll апдейтов. Возвращает {updates, nextOffset}. */
export async function tgGetUpdates(offset, longPollSecs = 25) {
  if (!API) return { updates: [], nextOffset: offset };
  const r = await tgCall(
    'getUpdates',
    { offset, timeout: longPollSecs, allowed_updates: ['message'] },
    (longPollSecs + 8) * 1000,
  );
  if (!r || !r.ok || !Array.isArray(r.result)) return { updates: [], nextOffset: offset };
  let next = offset;
  for (const u of r.result) if (u.update_id >= next) next = u.update_id + 1;
  return { updates: r.result, nextOffset: next };
}
