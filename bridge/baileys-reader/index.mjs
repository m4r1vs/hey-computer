#!/usr/bin/env node
// Strictly read-only WhatsApp Web linked-device reader for hey-computer-bridge.
//
// Invariants:
// - Never calls sendMessage, readMessages, sendPresenceUpdate, or sendReceipt.
// - Runs with markOnlineOnConnect: false (never alters online/read state).
// - Accepts ONLY `{"type":"fetch_history",...}` on stdin in daemon mode for
//   read-only historical message paging.
// - Scrubs libsignal SessionEntry objects before anything reaches stderr.
// - Writes creds.json atomically (0600) with .bak recovery.

import fs from "node:fs";
import path from "node:path";
import readline from "node:readline";
import util from "node:util";

// ---------------------------------------------------------------------------
// 1. SessionEntry key scrubber (must run before Baileys / libsignal imports)
// ---------------------------------------------------------------------------

const SENSITIVE_KEY_PATTERN =
  /SessionEntry|currentRatchet|ephemeralKeyPair|indexInfo|privKey|rootKey|chainKey|baseKey|signedPreKey|noiseKey|registrationId/i;

function isSensitiveObject(val) {
  if (!val || typeof val !== "object") return false;
  const ctorName = val.constructor && val.constructor.name;
  if (ctorName === "SessionEntry" || ctorName === "SessionRecord") return true;
  if (
    Object.prototype.hasOwnProperty.call(val, "currentRatchet") ||
    Object.prototype.hasOwnProperty.call(val, "indexInfo") ||
    Object.prototype.hasOwnProperty.call(val, "ephemeralKeyPair") ||
    Object.prototype.hasOwnProperty.call(val, "privKey") ||
    Object.prototype.hasOwnProperty.call(val, "rootKey")
  ) {
    return true;
  }
  return false;
}

function scrubArg(arg) {
  if (typeof arg === "string") {
    return SENSITIVE_KEY_PATTERN.test(arg) ? "[redacted SessionEntry]" : arg;
  }
  if (isSensitiveObject(arg)) {
    return "[redacted SessionEntry]";
  }
  if (arg && typeof arg === "object") {
    try {
      const formatted = util.inspect(arg, { depth: 2, breakLength: Infinity });
      if (SENSITIVE_KEY_PATTERN.test(formatted)) {
        return "[redacted SessionEntry]";
      }
    } catch {
      return "[redacted]";
    }
  }
  return arg;
}

const rawStderrWrite = process.stderr.write.bind(process.stderr);
process.stderr.write = function scrubbedStderrWrite(chunk, encoding, cb) {
  const str = typeof chunk === "string" ? chunk : chunk.toString("utf8");
  if (SENSITIVE_KEY_PATTERN.test(str)) {
    return rawStderrWrite("[redacted SessionEntry]\n", "utf8", cb);
  }
  return rawStderrWrite(chunk, encoding, cb);
};

for (const method of ["log", "info", "warn", "error", "debug", "trace"]) {
  console[method] = (...args) => {
    const cleaned = args.map(scrubArg);
    const line = util.format(...cleaned);
    if (SENSITIVE_KEY_PATTERN.test(line)) {
      rawStderrWrite("[redacted SessionEntry]\n");
    } else {
      rawStderrWrite(`${line}\n`);
    }
  };
}

// ---------------------------------------------------------------------------
// 2. Dynamic imports (guaranteed to load AFTER console/stderr scrubber runs)
// ---------------------------------------------------------------------------

const {
  default: makeWASocket,
  Browsers,
  DisconnectReason,
  fetchLatestBaileysVersion,
  makeCacheableSignalKeyStore,
  useMultiFileAuthState,
  WAMessageStubType,
} = await import("@whiskeysockets/baileys");
const { default: pino } = await import("pino");
const { default: qrcode } = await import("qrcode-terminal");

// ---------------------------------------------------------------------------
// 3. CLI parsing
// ---------------------------------------------------------------------------

function parseArgs(argv) {
  let sessionDir = "";
  let pairOnly = false;
  let fullHistory = true;
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === "--session") {
      sessionDir = argv[i + 1] ?? "";
      i += 1;
    } else if (arg.startsWith("--session=")) {
      sessionDir = arg.slice("--session=".length);
    } else if (arg === "--pair-only") {
      pairOnly = true;
    } else if (arg === "--no-full-history") {
      fullHistory = false;
    } else if (arg === "--help" || arg === "-h") {
      rawStderrWrite(
        "Usage: hey-computer-baileys-reader --session <dir> [--pair-only] [--no-full-history]\n",
      );
      process.exit(0);
    } else {
      rawStderrWrite(`Unknown argument: ${arg}\n`);
      process.exit(2);
    }
  }
  if (!sessionDir) {
    rawStderrWrite("Missing required --session <dir>\n");
    process.exit(2);
  }
  return { sessionDir, pairOnly, fullHistory };
}

const { sessionDir, pairOnly, fullHistory } = parseArgs(process.argv.slice(2));

// Enforce private file permissions for all created session artifacts.
if (typeof process.umask === "function") {
  process.umask(0o077);
}

// ---------------------------------------------------------------------------
// 4. JSONL stdout emitter (chunked well below MAX_BRIDGE_LINE = 128 KiB)
// ---------------------------------------------------------------------------

const MAX_HISTORY_CHUNK_COUNT = 80;
const MAX_BATCH_JSON_BYTES = 92 * 1024;
const MAX_SINGLE_TEXT_CHARS = 24 * 1024;

function emit(event) {
  process.stdout.write(`${JSON.stringify(event)}\n`);
}

function clampString(val, maxChars = MAX_SINGLE_TEXT_CHARS) {
  if (!val || typeof val !== "string") return "";
  return val.length > maxChars ? val.slice(0, maxChars) : val;
}

// ---------------------------------------------------------------------------
// 5. Atomic 0600 creds.json + .bak recovery
// ---------------------------------------------------------------------------

function ensurePrivateDir(dir) {
  fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
  try {
    fs.chmodSync(dir, 0o700);
  } catch {
    // Ignore on filesystems that do not support chmod.
  }
}

function isValidJsonFile(filePath) {
  try {
    const data = fs.readFileSync(filePath, "utf8");
    if (!data.trim()) return false;
    JSON.parse(data);
    return true;
  } catch {
    return false;
  }
}

function recoverCredsFromBackup(dir) {
  const credsPath = path.join(dir, "creds.json");
  const bakPath = path.join(dir, "creds.json.bak");
  if (isValidJsonFile(credsPath)) {
    return;
  }
  if (!isValidJsonFile(bakPath)) {
    return;
  }
  const tmpPath = path.join(dir, `.creds.recover.${process.pid}.tmp`);
  const raw = fs.readFileSync(bakPath);
  const fd = fs.openSync(tmpPath, "w", 0o600);
  try {
    fs.writeFileSync(fd, raw);
    fs.fsyncSync(fd);
  } finally {
    fs.closeSync(fd);
  }
  fs.renameSync(tmpPath, credsPath);
  try {
    fs.chmodSync(credsPath, 0o600);
  } catch {
    // Best-effort.
  }
  rawStderrWrite("Recovered creds.json from creds.json.bak\n");
}

function createAtomicCredsSaver(dir, baseSaveCreds) {
  const credsPath = path.join(dir, "creds.json");
  const bakPath = path.join(dir, "creds.json.bak");
  let writeSeq = 0;
  let chain = Promise.resolve();

  const saveOnce = async () => {
    // Snapshot previous valid creds.json to creds.json.bak before Baileys writes.
    if (isValidJsonFile(credsPath)) {
      const bakTmp = path.join(
        dir,
        `.creds.bak.${process.pid}.${(writeSeq += 1)}.tmp`,
      );
      const prev = fs.readFileSync(credsPath);
      const fd = fs.openSync(bakTmp, "w", 0o600);
      try {
        fs.writeFileSync(fd, prev);
        fs.fsyncSync(fd);
      } finally {
        fs.closeSync(fd);
      }
      fs.renameSync(bakTmp, bakPath);
    }

    await baseSaveCreds();

    // Rewrite creds.json atomically with 0600 + fsync so partial writes cannot corrupt it.
    if (isValidJsonFile(credsPath)) {
      const cur = fs.readFileSync(credsPath);
      const tmpPath = path.join(
        dir,
        `.creds.json.${process.pid}.${(writeSeq += 1)}.tmp`,
      );
      const fd = fs.openSync(tmpPath, "w", 0o600);
      try {
        fs.writeFileSync(fd, cur);
        fs.fsyncSync(fd);
      } finally {
        fs.closeSync(fd);
      }
      fs.renameSync(tmpPath, credsPath);
      try {
        fs.chmodSync(credsPath, 0o600);
      } catch {
        // Best-effort.
      }
    }
  };

  return () => {
    chain = chain.then(saveOnce, saveOnce);
    return chain;
  };
}

// ---------------------------------------------------------------------------
// 6. Bounded recent raw message store for Baileys v7 getMessage E2EE retries
// ---------------------------------------------------------------------------

const MAX_RECENT_RAW_MESSAGES = 256;
const recentRawMessages = new Map();

function rememberRawMessage(key, message) {
  if (!key || !key.remoteJid || !key.id || !message) return;
  const mapKey = `${key.remoteJid}:${key.id}`;
  if (recentRawMessages.has(mapKey)) {
    recentRawMessages.delete(mapKey);
  }
  recentRawMessages.set(mapKey, message);
  while (recentRawMessages.size > MAX_RECENT_RAW_MESSAGES) {
    const oldestKey = recentRawMessages.keys().next().value;
    recentRawMessages.delete(oldestKey);
  }
}

// ---------------------------------------------------------------------------
// 7. Identity, @lid resolution, group/contact caches, and message unwrapping
// ---------------------------------------------------------------------------

function normalizeJid(jid) {
  if (!jid || typeof jid !== "string") return "";
  const trimmed = jid.trim();
  const at = trimmed.indexOf("@");
  if (at === -1) return trimmed;
  const userPart = trimmed.slice(0, at);
  const domain = trimmed.slice(at + 1).toLowerCase();
  const bareUser = userPart.split(":")[0];
  return `${bareUser}@${domain}`;
}

function isIgnoredJid(jid) {
  return (
    !jid ||
    jid === "status@broadcast" ||
    jid.endsWith("@broadcast") ||
    jid.endsWith("@newsletter")
  );
}

function jidToE164(jid) {
  const norm = normalizeJid(jid);
  if (!norm.endsWith("@s.whatsapp.net")) return null;
  const digits = norm.slice(0, -"@s.whatsapp.net".length).replace(/\D/g, "");
  if (digits.length < 7 || digits.length > 15) return null;
  return `+${digits}`;
}

function anyPhoneToE164(val) {
  if (!val) return null;
  const str = String(val).trim();
  const fromJid = jidToE164(str);
  if (fromJid) return fromJid;
  if (str.includes("@")) return null;
  const digits = str.replace(/\D/g, "");
  if (digits.length < 7 || digits.length > 15) return null;
  return `+${digits}`;
}

function extractLidDigits(jid) {
  const norm = normalizeJid(jid);
  if (!norm) return null;
  if (norm.endsWith("@lid")) {
    const digits = norm.slice(0, -"@lid".length).replace(/\D/g, "");
    return digits || null;
  }
  if (!norm.includes("@")) {
    const digits = norm.replace(/\D/g, "");
    return digits.length >= 7 ? digits : null;
  }
  return null;
}

const lidToPnCache = new Map();
const groupSubjects = new Map();
const contactNames = new Map();

function recordLidPnMapping(lidCandidate, pnCandidate) {
  const lidDigits = extractLidDigits(
    typeof lidCandidate === "string" ? lidCandidate : "",
  );
  const e164 = anyPhoneToE164(pnCandidate);
  if (lidDigits && e164) {
    lidToPnCache.set(lidDigits, e164);
  }
}

function ingestLidMappingEntry(entry) {
  if (!entry) return;
  if (Array.isArray(entry) && entry.length >= 2) {
    recordLidPnMapping(entry[0], entry[1]);
    return;
  }
  if (typeof entry === "object") {
    const lid = entry.lid || entry.lidJid || entry.id || "";
    const pn =
      entry.pn || entry.pnJid || entry.phoneNumber || entry.jid || entry.phone || "";
    recordLidPnMapping(lid, pn);
  }
}

function lookupLidFromDisk(dir, lidDigits) {
  if (!lidDigits) return null;
  if (lidToPnCache.has(lidDigits)) {
    return lidToPnCache.get(lidDigits);
  }
  const reverseFile = path.join(dir, `lid-mapping-${lidDigits}_reverse.json`);
  try {
    const parsed = JSON.parse(fs.readFileSync(reverseFile, "utf8"));
    const candidate =
      typeof parsed === "string"
        ? parsed
        : parsed?.pn || parsed?.jid || parsed?.phone || null;
    const e164 = anyPhoneToE164(candidate);
    if (e164) {
      lidToPnCache.set(lidDigits, e164);
      return e164;
    }
  } catch {
    // Mapping file may not exist yet.
  }
  return null;
}

async function resolveJidToE164(sock, dir, jid, altJid) {
  const direct = jidToE164(jid);
  if (direct) return direct;

  const alt = jidToE164(altJid);
  if (alt) {
    const lidDigits = extractLidDigits(jid);
    if (lidDigits) lidToPnCache.set(lidDigits, alt);
    return alt;
  }

  const lidDigits = extractLidDigits(jid);
  if (!lidDigits) return null;

  const fromDisk = lookupLidFromDisk(dir, lidDigits);
  if (fromDisk) return fromDisk;

  try {
    const repo = sock?.signalRepository?.lidMapping;
    if (repo && typeof repo.getPNForLID === "function") {
      const pn = await repo.getPNForLID(normalizeJid(jid));
      const e164 = anyPhoneToE164(pn);
      if (e164) {
        lidToPnCache.set(lidDigits, e164);
        return e164;
      }
    }
  } catch {
    // Non-fatal: keep LID as fallback identifier.
  }

  return null;
}

function emitChatMetaBatch(items) {
  if (!Array.isArray(items) || items.length === 0) return;
  let chunk = [];
  let bytes = 64;
  for (const item of items) {
    const encodedLen = Buffer.byteLength(JSON.stringify(item), "utf8") + 2;
    if (
      chunk.length > 0 &&
      (chunk.length >= 100 || bytes + encodedLen > MAX_BATCH_JSON_BYTES)
    ) {
      emit({ type: "chat_meta_batch", items: chunk });
      chunk = [];
      bytes = 64;
    }
    chunk.push(item);
    bytes += encodedLen;
  }
  if (chunk.length > 0) {
    emit({ type: "chat_meta_batch", items: chunk });
  }
}

function processContactsList(contacts) {
  const metaItems = [];
  for (const c of contacts || []) {
    if (!c || typeof c !== "object") continue;
    const id = normalizeJid(c.id || "");
    const lid = normalizeJid(c.lid || c.lidJid || "");
    const pn = normalizeJid(c.phoneNumber || c.pnJid || c.jid || "");
    if (lid && pn) recordLidPnMapping(lid, pn);
    if (id.endsWith("@lid") && pn) recordLidPnMapping(id, pn);
    if (lid && id.endsWith("@s.whatsapp.net")) recordLidPnMapping(lid, id);

    const primaryJid = pn || id || lid;
    if (isIgnoredJid(primaryJid)) continue;
    const isGroup = primaryJid.endsWith("@g.us");
    const name = String(c.name || c.notify || c.verifiedName || "").trim();

    if (name) {
      if (isGroup) {
        groupSubjects.set(primaryJid, name);
      } else {
        for (const key of [id, lid, pn]) {
          if (key) contactNames.set(key, name);
        }
      }
    }

    const lidDigits = extractLidDigits(lid || id);
    const resolvedE164 = isGroup
      ? null
      : jidToE164(pn) ||
        jidToE164(id) ||
        (lidDigits ? lookupLidFromDisk(sessionDir, lidDigits) : null);

    const aliases = [];
    for (const a of [id, lid, pn, resolvedE164]) {
      if (a && !aliases.includes(a)) aliases.push(a);
    }

    metaItems.push({
      raw_jid: primaryJid,
      resolved_e164: resolvedE164,
      aliases,
      is_group: isGroup,
      title: name || null,
    });
  }
  return metaItems;
}

function processGroupObjects(groups) {
  const metaItems = [];
  for (const g of groups || []) {
    if (!g || typeof g !== "object") continue;
    const gid = normalizeJid(g.id || "");
    if (!gid || !gid.endsWith("@g.us")) continue;
    const subject = String(g.subject || g.name || "").trim();
    if (subject) {
      groupSubjects.set(gid, subject);
    }
    for (const p of g.participants || []) {
      if (!p || typeof p !== "object") continue;
      const pLid = p.lid || (String(p.id || "").endsWith("@lid") ? p.id : "");
      const pPn =
        p.phoneNumber ||
        p.pn ||
        p.jid ||
        (String(p.id || "").endsWith("@s.whatsapp.net") ? p.id : "");
      recordLidPnMapping(pLid, pPn);
    }
    metaItems.push({
      raw_jid: gid,
      resolved_e164: null,
      aliases: [gid],
      is_group: true,
      title: subject || groupSubjects.get(gid) || null,
    });
  }
  return metaItems;
}

function unwrapMessage(message) {
  let cur = message;
  for (let depth = 0; depth < 6 && cur && typeof cur === "object"; depth += 1) {
    if (cur.ephemeralMessage?.message) {
      cur = cur.ephemeralMessage.message;
    } else if (cur.viewOnceMessage?.message) {
      cur = cur.viewOnceMessage.message;
    } else if (cur.viewOnceMessageV2?.message) {
      cur = cur.viewOnceMessageV2.message;
    } else if (cur.viewOnceMessageV2Extension?.message) {
      cur = cur.viewOnceMessageV2Extension.message;
    } else if (cur.documentWithCaptionMessage?.message) {
      cur = cur.documentWithCaptionMessage.message;
    } else if (cur.editedMessage?.message?.protocolMessage?.editedMessage) {
      cur = cur.editedMessage.message.protocolMessage.editedMessage;
    } else if (cur.editedMessage?.message) {
      cur = cur.editedMessage.message;
    } else {
      break;
    }
  }
  return cur;
}

function extractTextAndMedia(rawMessage) {
  const msg = unwrapMessage(rawMessage);
  if (!msg || typeof msg !== "object") {
    return { text: "", quotedText: null, mediaPlaceholder: null };
  }

  let text = "";
  let mediaPlaceholder = null;
  let contextInfo = null;

  if (typeof msg.conversation === "string") {
    text = msg.conversation;
  } else if (msg.extendedTextMessage) {
    text = msg.extendedTextMessage.text || "";
    contextInfo = msg.extendedTextMessage.contextInfo || null;
  } else if (msg.imageMessage) {
    text = msg.imageMessage.caption || "";
    mediaPlaceholder = "[image]";
    contextInfo = msg.imageMessage.contextInfo || null;
  } else if (msg.videoMessage) {
    text = msg.videoMessage.caption || "";
    mediaPlaceholder = "[video]";
    contextInfo = msg.videoMessage.contextInfo || null;
  } else if (msg.audioMessage) {
    mediaPlaceholder = msg.audioMessage.ptt ? "[voice note]" : "[audio]";
    contextInfo = msg.audioMessage.contextInfo || null;
  } else if (msg.documentMessage) {
    text = msg.documentMessage.caption || "";
    mediaPlaceholder = "[document]";
    contextInfo = msg.documentMessage.contextInfo || null;
  } else if (msg.stickerMessage) {
    mediaPlaceholder = "[sticker]";
    contextInfo = msg.stickerMessage.contextInfo || null;
  } else if (msg.locationMessage) {
    mediaPlaceholder = "[location]";
    contextInfo = msg.locationMessage.contextInfo || null;
  } else if (msg.contactMessage) {
    mediaPlaceholder = "[contact]";
    contextInfo = msg.contactMessage.contextInfo || null;
  } else if (msg.buttonsResponseMessage) {
    text = msg.buttonsResponseMessage.selectedDisplayText || "";
    contextInfo = msg.buttonsResponseMessage.contextInfo || null;
  } else if (msg.listResponseMessage) {
    text = msg.listResponseMessage.title || "";
    contextInfo = msg.listResponseMessage.contextInfo || null;
  }

  let quotedText = null;
  if (contextInfo?.quotedMessage) {
    const quoted = extractTextAndMedia(contextInfo.quotedMessage);
    const combined = (quoted.text || quoted.mediaPlaceholder || "").trim();
    if (combined) quotedText = clampString(combined, 4096);
  }

  return {
    text: clampString(text.trim()),
    quotedText,
    mediaPlaceholder,
  };
}

function extractTimestampMs(ts) {
  if (ts == null) return Date.now();
  let num;
  if (typeof ts === "number") {
    num = ts;
  } else if (typeof ts === "bigint") {
    num = Number(ts);
  } else if (typeof ts === "object" && typeof ts.toNumber === "function") {
    num = ts.toNumber();
  } else {
    num = Number(ts);
  }
  if (!Number.isFinite(num) || num <= 0) return Date.now();
  return num < 1e12 ? Math.trunc(num * 1000) : Math.trunc(num);
}

async function convertWaMessage(sock, selfJid, selfLid, selfE164, msg, upsertType) {
  if (!msg || !msg.key) return null;

  const remoteJid = normalizeJid(msg.key.remoteJid || "");
  if (isIgnoredJid(remoteJid)) return null;

  if (!msg.message || msg.messageStubType === WAMessageStubType.CIPHERTEXT) {
    return null;
  }

  rememberRawMessage(msg.key, msg.message);

  const { text, quotedText, mediaPlaceholder } = extractTextAndMedia(msg.message);
  if (!text && !mediaPlaceholder) {
    return null;
  }

  const isGroup = remoteJid.endsWith("@g.us");
  const fromMe = Boolean(msg.key.fromMe);
  const remoteJidAlt = normalizeJid(msg.key.remoteJidAlt || "");
  const participant = normalizeJid(msg.key.participant || "");
  const participantAlt = normalizeJid(msg.key.participantAlt || "");

  const resolvedChatE164 = isGroup
    ? null
    : await resolveJidToE164(sock, sessionDir, remoteJid, remoteJidAlt);
  const senderRaw = isGroup ? participant : remoteJid;
  const senderAlt = isGroup ? participantAlt : remoteJidAlt;
  const resolvedSenderE164 = await resolveJidToE164(
    sock,
    sessionDir,
    senderRaw,
    senderAlt,
  );

  const isSelfChat =
    fromMe &&
    !isGroup &&
    (Boolean(selfJid && remoteJid === selfJid) ||
      Boolean(selfLid && remoteJid === selfLid) ||
      Boolean(selfE164 && resolvedChatE164 === selfE164));

  const pushName =
    (typeof msg.pushName === "string" && msg.pushName.trim()) ||
    (!fromMe && senderRaw ? contactNames.get(senderRaw) || null : null);

  const chatTitle = isGroup
    ? groupSubjects.get(remoteJid) || null
    : fromMe
      ? contactNames.get(remoteJid) ||
        (remoteJidAlt ? contactNames.get(remoteJidAlt) : null) ||
        null
      : (typeof msg.pushName === "string" && msg.pushName.trim()) ||
        contactNames.get(remoteJid) ||
        (remoteJidAlt ? contactNames.get(remoteJidAlt) : null) ||
        null;

  return {
    upsert_type: upsertType,
    id: msg.key.id || `${Date.now()}`,
    remote_jid: remoteJid,
    remote_jid_alt: remoteJidAlt || null,
    participant: participant || null,
    participant_alt: participantAlt || null,
    resolved_chat_e164: resolvedChatE164,
    resolved_sender_e164: resolvedSenderE164,
    from_me: fromMe,
    is_group: isGroup,
    is_self_chat: isSelfChat,
    push_name: pushName,
    chat_title: chatTitle,
    timestamp_ms: extractTimestampMs(msg.messageTimestamp),
    text,
    quoted_text: quotedText,
    media_placeholder: mediaPlaceholder,
  };
}

async function fetchVersionWithTimeout(timeoutMs = 15000) {
  let timer = null;
  try {
    const res = await Promise.race([
      fetchLatestBaileysVersion(),
      new Promise((_, reject) => {
        timer = setTimeout(
          () => reject(new Error("fetchLatestBaileysVersion timeout")),
          timeoutMs,
        );
      }),
    ]);
    return res?.version;
  } catch (err) {
    rawStderrWrite(
      `Version lookup failed (${err?.message || "unknown"}), using bundled default\n`,
    );
    return undefined;
  } finally {
    if (timer) clearTimeout(timer);
  }
}

// ---------------------------------------------------------------------------
// 8. Main connection, read-only stdin paging listener, & supervision loop
// ---------------------------------------------------------------------------

const logger = pino({ level: "silent" });
let shuttingDown = false;
let activeSock = null;
let reconnectTimer = null;
let reconnectAttempt = 0;

if (!pairOnly && process.stdin) {
  const rl = readline.createInterface({
    input: process.stdin,
    crlfDelay: Infinity,
  });
  rl.on("line", async (rawLine) => {
    const trimmed = String(rawLine || "").trim();
    if (!trimmed) return;
    let cmd;
    try {
      cmd = JSON.parse(trimmed);
    } catch {
      rawStderrWrite("Rejected non-JSON stdin control line\n");
      return;
    }
    if (!cmd || cmd.type !== "fetch_history") {
      rawStderrWrite(
        `Rejected unsupported stdin control command (${cmd?.type || "unknown"}); bridge is strictly read-only\n`,
      );
      return;
    }
    const chatJid = normalizeJid(cmd.chat_jid || "");
    const oldestMsgId = String(cmd.oldest_msg_id || "").trim();
    const oldestFromMe = Boolean(cmd.oldest_from_me);
    const oldestTsMs = Math.trunc(Number(cmd.oldest_ts_ms));
    const count = Math.max(1, Math.min(200, Math.trunc(Number(cmd.count) || 50)));
    if (!chatJid || !oldestMsgId || !Number.isFinite(oldestTsMs) || oldestTsMs <= 0) {
      rawStderrWrite("Ignoring malformed fetch_history control parameters\n");
      return;
    }
    if (!activeSock || typeof activeSock.fetchMessageHistory !== "function") {
      rawStderrWrite("Cannot fetch_history: WhatsApp socket not ready\n");
      return;
    }
    try {
      await activeSock.fetchMessageHistory(
        count,
        {
          remoteJid: chatJid,
          fromMe: oldestFromMe,
          id: oldestMsgId,
        },
        oldestTsMs,
      );
    } catch (err) {
      rawStderrWrite(
        `fetchMessageHistory failed: ${err?.message || "unknown"}\n`,
      );
    }
  });
  rl.on("error", () => {
    // Ignore stdin pipe close errors during shutdown.
  });
}

function computeBackoffMs(attempt) {
  const base = Math.min(60000, Math.round(2000 * Math.pow(1.8, attempt)));
  const jitter = Math.round(base * 0.2 * Math.random());
  return Math.min(60000, base + jitter);
}

async function startSocket() {
  if (shuttingDown) return;

  ensurePrivateDir(sessionDir);
  recoverCredsFromBackup(sessionDir);

  const { state, saveCreds: rawSaveCreds } =
    await useMultiFileAuthState(sessionDir);
  const saveCreds = createAtomicCredsSaver(sessionDir, rawSaveCreds);
  const version = await fetchVersionWithTimeout(15000);

  if (shuttingDown) return;

  if (!pairOnly) {
    emit({ type: "state", state: "starting" });
  }

  // Use Ubuntu("Desktop") rather than macOS("Desktop"): Baileys maps
  // macOS("Desktop") to WebSubPlatform.DARWIN, which WhatsApp servers reject
  // immediately with HTTP/WS status 428, whereas Ubuntu("Desktop") negotiates
  // PlatformType.DESKTOP + requireFullSync (syncFullHistory) over WEB_BROWSER.
  const desktopBrowser =
    typeof Browsers?.ubuntu === "function"
      ? Browsers.ubuntu("Desktop")
      : ["Ubuntu", "Desktop", "22.04.4"];

  const socketConfig = {
    auth: {
      creds: state.creds,
      keys: makeCacheableSignalKeyStore(state.keys, logger),
    },
    logger,
    printQRInTerminal: false,
    browser: fullHistory ? desktopBrowser : ["Desktop", "Linux", "1.0"],
    syncFullHistory: fullHistory,
    shouldSyncHistoryMessage: () => fullHistory,
    markOnlineOnConnect: false,
    getMessage: async (key) => {
      if (!key || !key.remoteJid || !key.id) {
        return { conversation: "" };
      }
      const stored = recentRawMessages.get(`${key.remoteJid}:${key.id}`);
      return stored || { conversation: "" };
    },
  };
  if (version) {
    socketConfig.version = version;
  }

  const sock = makeWASocket(socketConfig);
  activeSock = sock;

  sock.ev.on("creds.update", () => {
    saveCreds().catch((err) => {
      rawStderrWrite(`Failed to save creds: ${err?.message || "unknown"}\n`);
    });
  });

  sock.ev.on("connection.update", async (update) => {
    const { connection, lastDisconnect, qr } = update;

    if (qr) {
      if (pairOnly) {
        rawStderrWrite(
          "\nScan this QR code in WhatsApp (Settings -> Linked Devices -> Link a Device):\n\n",
        );
        qrcode.generate(qr, { small: true }, (rendered) => {
          rawStderrWrite(`${rendered}\n`);
        });
      } else {
        emit({
          type: "auth_required",
          detail:
            "WhatsApp session is not paired; run `hey-computer-bridge pair whatsapp`.",
        });
        shuttingDown = true;
        try {
          sock.end(undefined);
        } catch {
          // Ignore close errors.
        }
        process.exit(0);
      }
    }

    if (connection === "open") {
      reconnectAttempt = 0;
      const selfJid = normalizeJid(sock.user?.id || state.creds?.me?.id || "");
      const selfLid = normalizeJid(
        sock.user?.lid || state.creds?.me?.lid || "",
      );
      const selfE164 = jidToE164(selfJid);

      if (pairOnly) {
        try {
          await saveCreds();
          // Allow initial pre-key and creds.update writes to flush cleanly.
          await new Promise((resolve) => setTimeout(resolve, 3000));
          await saveCreds();
        } catch (err) {
          rawStderrWrite(
            `Warning: final creds save failed: ${err?.message || "unknown"}\n`,
          );
        }
        emit({
          type: "paired",
          self_jid: selfJid || null,
          self_lid: selfLid || null,
          self_e164: selfE164,
        });
        rawStderrWrite(
          `WhatsApp linked successfully (jid=${selfJid || "unknown"}, lid=${selfLid || "unknown"}).\n`,
        );
        shuttingDown = true;
        try {
          sock.end(undefined);
        } catch {
          // Ignore.
        }
        process.exit(0);
      }

      emit({
        type: "state",
        state: "connected",
        self_jid: selfJid || null,
        self_lid: selfLid || null,
        self_e164: selfE164,
      });

      if (typeof sock.groupFetchAllParticipating === "function") {
        sock
          .groupFetchAllParticipating()
          .then((groupsMap) => {
            const list = Object.values(groupsMap || {});
            const metaItems = processGroupObjects(list);
            emitChatMetaBatch(metaItems);
          })
          .catch((err) => {
            rawStderrWrite(
              `groupFetchAllParticipating skipped: ${err?.message || "unknown"}\n`,
            );
          });
      }
    }

    if (connection === "close") {
      if (shuttingDown) return;

      const statusCode =
        lastDisconnect?.error?.output?.statusCode ??
        lastDisconnect?.error?.statusCode ??
        0;

      if (statusCode === DisconnectReason.loggedOut || statusCode === 401) {
        emit({
          type: "auth_required",
          detail:
            "WhatsApp session unlinked (401); run `hey-computer-bridge pair whatsapp`.",
        });
        shuttingDown = true;
        process.exit(pairOnly ? 1 : 0);
      }

      if (
        statusCode === DisconnectReason.restartRequired ||
        statusCode === 515
      ) {
        reconnectAttempt = 0;
        if (!pairOnly) {
          emit({
            type: "state",
            state: "reconnecting",
            detail: "WhatsApp requested stream restart (515)",
          });
        } else {
          rawStderrWrite("Completing pairing handshake (515 restart)...\n");
        }
        reconnectTimer = setTimeout(() => {
          startSocket().catch(handleFatal);
        }, 1000);
        return;
      }

      const delayMs = computeBackoffMs(reconnectAttempt);
      reconnectAttempt += 1;
      if (!pairOnly) {
        emit({
          type: "state",
          state: "reconnecting",
          detail: `Disconnected (code ${statusCode}); retrying in ${Math.round(delayMs / 1000)}s`,
        });
      } else {
        rawStderrWrite(
          `Disconnected during pairing (code ${statusCode}); retrying in ${Math.round(delayMs / 1000)}s...\n`,
        );
      }
      reconnectTimer = setTimeout(() => {
        startSocket().catch(handleFatal);
      }, delayMs);
    }
  });

  sock.ev.on("groups.upsert", (groups) => {
    emitChatMetaBatch(processGroupObjects(groups));
  });

  sock.ev.on("groups.update", (updates) => {
    emitChatMetaBatch(processGroupObjects(updates));
  });

  sock.ev.on("contacts.upsert", (contacts) => {
    emitChatMetaBatch(processContactsList(contacts));
  });

  sock.ev.on("contacts.update", (contacts) => {
    emitChatMetaBatch(processContactsList(contacts));
  });

  sock.ev.on(
    "messaging-history.set",
    async ({
      chats,
      contacts,
      messages,
      lidPnMappings,
      syncType,
      progress,
      isLatest,
    }) => {
      for (const entry of lidPnMappings || []) {
        ingestLidMappingEntry(entry);
      }

      const metaItems = [];
      metaItems.push(...processContactsList(contacts));

      for (const chat of chats || []) {
        if (!chat || typeof chat !== "object") continue;
        const cid = normalizeJid(chat.id || "");
        if (isIgnoredJid(cid)) continue;
        const lidJid = normalizeJid(chat.lidJid || chat.lid || "");
        const pnJid = normalizeJid(chat.pnJid || chat.phoneNumber || "");
        if (lidJid && pnJid) recordLidPnMapping(lidJid, pnJid);
        if (cid.endsWith("@lid") && pnJid) recordLidPnMapping(cid, pnJid);
        if (lidJid && cid.endsWith("@s.whatsapp.net")) {
          recordLidPnMapping(lidJid, cid);
        }

        const isGroup = cid.endsWith("@g.us");
        const title = String(
          chat.name ||
            chat.subject ||
            (isGroup ? groupSubjects.get(cid) : contactNames.get(cid)) ||
            "",
        ).trim();
        if (title) {
          if (isGroup) {
            groupSubjects.set(cid, title);
          } else {
            contactNames.set(cid, title);
          }
        }

        const lidDigits = extractLidDigits(lidJid || cid);
        const resolvedE164 = isGroup
          ? null
          : jidToE164(pnJid) ||
            jidToE164(cid) ||
            (lidDigits ? lookupLidFromDisk(sessionDir, lidDigits) : null);

        const aliases = [];
        for (const a of [cid, lidJid, pnJid, resolvedE164]) {
          if (a && !aliases.includes(a)) aliases.push(a);
        }
        metaItems.push({
          raw_jid: cid,
          resolved_e164: resolvedE164,
          aliases,
          is_group: isGroup,
          title: title || null,
        });
      }
      emitChatMetaBatch(metaItems);

      const selfJid = normalizeJid(sock.user?.id || state.creds?.me?.id || "");
      const selfLid = normalizeJid(
        sock.user?.lid || state.creds?.me?.lid || "",
      );
      const selfE164 = jidToE164(selfJid);

      const converted = [];
      for (const msg of messages || []) {
        const item = await convertWaMessage(
          sock,
          selfJid,
          selfLid,
          selfE164,
          msg,
          "history",
        );
        if (item) converted.push(item);
      }

      const syncTypeStr = String(syncType ?? "");
      const progressNum =
        typeof progress === "number" && Number.isFinite(progress)
          ? progress
          : null;
      const latestFlag = Boolean(isLatest);

      if (converted.length === 0) {
        if (progressNum !== null || latestFlag) {
          emit({
            type: "history_batch",
            sync_type: syncTypeStr,
            progress: progressNum,
            is_latest: latestFlag,
            messages: [],
          });
        }
        return;
      }

      let chunk = [];
      let bytes = 128;
      for (const item of converted) {
        const itemBytes = Buffer.byteLength(JSON.stringify(item), "utf8") + 2;
        if (
          chunk.length > 0 &&
          (chunk.length >= MAX_HISTORY_CHUNK_COUNT ||
            bytes + itemBytes > MAX_BATCH_JSON_BYTES)
        ) {
          emit({
            type: "history_batch",
            sync_type: syncTypeStr,
            progress: progressNum,
            is_latest: latestFlag,
            messages: chunk,
          });
          chunk = [];
          bytes = 128;
        }
        chunk.push(item);
        bytes += itemBytes;
      }
      if (chunk.length > 0) {
        emit({
          type: "history_batch",
          sync_type: syncTypeStr,
          progress: progressNum,
          is_latest: latestFlag,
          messages: chunk,
        });
      }
    },
  );

  if (!pairOnly) {
    sock.ev.on("messages.upsert", async (upsert) => {
      const upsertType = upsert?.type;
      if (upsertType !== "notify" && upsertType !== "append") {
        return;
      }

      const selfJid = normalizeJid(sock.user?.id || state.creds?.me?.id || "");
      const selfLid = normalizeJid(
        sock.user?.lid || state.creds?.me?.lid || "",
      );
      const selfE164 = jidToE164(selfJid);

      for (const msg of upsert.messages || []) {
        const converted = await convertWaMessage(
          sock,
          selfJid,
          selfLid,
          selfE164,
          msg,
          upsertType,
        );
        if (converted) {
          emit({
            type: "message",
            ...converted,
          });
        }
      }
    });
  }
}

function handleFatal(err) {
  rawStderrWrite(`Fatal baileys-reader error: ${err?.message || "unknown"}\n`);
  process.exit(1);
}

function shutdown() {
  shuttingDown = true;
  if (reconnectTimer) clearTimeout(reconnectTimer);
  try {
    activeSock?.end(undefined);
  } catch {
    // Ignore close errors.
  }
  process.exit(0);
}

process.on("SIGTERM", shutdown);
process.on("SIGINT", shutdown);

startSocket().catch(handleFatal);
