#!/usr/bin/env node
// Strictly read-only WhatsApp Web linked-device reader for hey-computer-bridge.
//
// Invariants:
// - Never calls sendMessage, readMessages, sendPresenceUpdate, or sendReceipt.
// - Runs with markOnlineOnConnect: false and syncFullHistory: false.
// - Ignores stdin completely in daemon mode (one-way JSONL output on stdout).
// - Scrubs libsignal SessionEntry objects before anything reaches stderr.
// - Writes creds.json atomically (0600) with .bak recovery.

import fs from "node:fs";
import path from "node:path";
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
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === "--session") {
      sessionDir = argv[i + 1] ?? "";
      i += 1;
    } else if (arg.startsWith("--session=")) {
      sessionDir = arg.slice("--session=".length);
    } else if (arg === "--pair-only") {
      pairOnly = true;
    } else if (arg === "--help" || arg === "-h") {
      rawStderrWrite(
        "Usage: hey-computer-baileys-reader --session <dir> [--pair-only]\n",
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
  return { sessionDir, pairOnly };
}

const { sessionDir, pairOnly } = parseArgs(process.argv.slice(2));

// Daemon mode never reads stdin; pause it explicitly.
if (!pairOnly && process.stdin) {
  process.stdin.pause();
}

// Enforce private file permissions for all created session artifacts.
if (typeof process.umask === "function") {
  process.umask(0o077);
}

// ---------------------------------------------------------------------------
// 4. One-way JSONL stdout emitter
// ---------------------------------------------------------------------------

function emit(event) {
  process.stdout.write(`${JSON.stringify(event)}\n`);
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
// 7. Identity, @lid resolution, and message unwrapping
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

function jidToE164(jid) {
  const norm = normalizeJid(jid);
  if (!norm.endsWith("@s.whatsapp.net")) return null;
  const digits = norm.slice(0, -"@s.whatsapp.net".length).replace(/\D/g, "");
  if (digits.length < 7 || digits.length > 15) return null;
  return `+${digits}`;
}

function extractLidDigits(jid) {
  const norm = normalizeJid(jid);
  if (!norm.endsWith("@lid")) return null;
  const digits = norm.slice(0, -"@lid".length).replace(/\D/g, "");
  return digits || null;
}

const lidToPnCache = new Map();

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
    if (candidate) {
      const digits = String(candidate).replace(/\D/g, "");
      if (digits.length >= 7 && digits.length <= 15) {
        const e164 = `+${digits}`;
        lidToPnCache.set(lidDigits, e164);
        return e164;
      }
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
      const e164 = jidToE164(pn) || (pn ? `+${String(pn).replace(/\D/g, "")}` : null);
      if (e164 && /^\+\d{7,15}$/.test(e164)) {
        lidToPnCache.set(lidDigits, e164);
        return e164;
      }
    }
  } catch {
    // Non-fatal: keep LID as fallback identifier.
  }

  return null;
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
    if (combined) quotedText = combined;
  }

  return {
    text: text.trim(),
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
// 8. Main connection & supervision loop
// ---------------------------------------------------------------------------

const logger = pino({ level: "silent" });
let shuttingDown = false;
let activeSock = null;
let reconnectTimer = null;
let reconnectAttempt = 0;

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

  const socketConfig = {
    auth: {
      creds: state.creds,
      keys: makeCacheableSignalKeyStore(state.keys, logger),
    },
    logger,
    printQRInTerminal: false,
    browser: ["Desktop", "Linux", "1.0"],
    syncFullHistory: false,
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
        if (!msg || !msg.key) continue;

        const remoteJid = normalizeJid(msg.key.remoteJid || "");
        if (
          !remoteJid ||
          remoteJid === "status@broadcast" ||
          remoteJid.endsWith("@broadcast")
        ) {
          continue;
        }

        // Skip CIPHERTEXT stubs and empty protocol messages.
        if (
          !msg.message ||
          msg.messageStubType === WAMessageStubType.CIPHERTEXT
        ) {
          continue;
        }

        rememberRawMessage(msg.key, msg.message);

        const { text, quotedText, mediaPlaceholder } = extractTextAndMedia(
          msg.message,
        );
        if (!text && !mediaPlaceholder) {
          continue;
        }

        const isGroup = remoteJid.endsWith("@g.us");
        const isBroadcast = remoteJid.endsWith("@broadcast");
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
          !isBroadcast &&
          (Boolean(selfJid && remoteJid === selfJid) ||
            Boolean(selfLid && remoteJid === selfLid) ||
            Boolean(selfE164 && resolvedChatE164 === selfE164));

        emit({
          type: "message",
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
          push_name: msg.pushName || null,
          timestamp_ms: extractTimestampMs(msg.messageTimestamp),
          text,
          quoted_text: quotedText,
          media_placeholder: mediaPlaceholder,
        });
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
