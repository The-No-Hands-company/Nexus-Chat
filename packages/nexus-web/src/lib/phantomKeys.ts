/**
 * Client-side Phantom PQ key management (Privacy Part 3).
 *
 * Keys are generated with @noble/post-quantum (ML-KEM-1024 + ML-DSA-87).
 * Private keys are stored encrypted in IndexedDB, never sent to the server.
 * The server accepts only the public keys via POST /api/users/@me/phantom.
 *
 * Encryption: AES-GCM-256 key derived from user password via PBKDF2-SHA-256.
 * A random 16-byte salt and 12-byte IV are stored alongside the ciphertext.
 */

import { ml_kem1024 } from "@noble/post-quantum/ml-kem.js";
import { ml_dsa87 } from "@noble/post-quantum/ml-dsa.js";

const DB_NAME = "nexus-phantom";
const STORE_NAME = "keys";
const KEY_ID = "phantom-v1";

export interface PhantomPublicKeys {
  kemPublic: string;      // base64 ML-KEM-1024 public key
  signingPublic: string;  // base64 ML-DSA-87 public key
}

interface StoredKeyBlob {
  salt: string;   // base64 16 bytes
  iv: string;     // base64 12 bytes
  data: string;   // base64 AES-GCM ciphertext of { kemSecret, signingSecret }
}

// ── base64 helpers ────────────────────────────────────────────────────────────

function toB64(bytes: Uint8Array): string {
  let bin = "";
  for (const b of bytes) bin += String.fromCharCode(b);
  return btoa(bin);
}

function fromB64(b64: string): Uint8Array<ArrayBuffer> {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out.slice();
}

// ── PBKDF2 key derivation ─────────────────────────────────────────────────────

async function deriveKey(password: string, salt: Uint8Array<ArrayBuffer>): Promise<CryptoKey> {
  const raw = new TextEncoder().encode(password);
  const base = await crypto.subtle.importKey("raw", raw, "PBKDF2", false, ["deriveKey"]);
  return crypto.subtle.deriveKey(
    { name: "PBKDF2", salt, iterations: 200_000, hash: "SHA-256" },
    base,
    { name: "AES-GCM", length: 256 },
    false,
    ["encrypt", "decrypt"],
  );
}

// ── IndexedDB ─────────────────────────────────────────────────────────────────

function openDb(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open(DB_NAME, 1);
    req.onupgradeneeded = () => req.result.createObjectStore(STORE_NAME);
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

async function idbGet(db: IDBDatabase, key: string): Promise<unknown> {
  return new Promise((resolve, reject) => {
    const tx = db.transaction(STORE_NAME, "readonly");
    const req = tx.objectStore(STORE_NAME).get(key);
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

async function idbPut(db: IDBDatabase, key: string, value: unknown): Promise<void> {
  return new Promise((resolve, reject) => {
    const tx = db.transaction(STORE_NAME, "readwrite");
    const req = tx.objectStore(STORE_NAME).put(value, key);
    req.onsuccess = () => resolve();
    req.onerror = () => reject(req.error);
  });
}

async function idbDelete(db: IDBDatabase, key: string): Promise<void> {
  return new Promise((resolve, reject) => {
    const tx = db.transaction(STORE_NAME, "readwrite");
    const req = tx.objectStore(STORE_NAME).delete(key);
    req.onsuccess = () => resolve();
    req.onerror = () => reject(req.error);
  });
}

// ── Public API ────────────────────────────────────────────────────────────────

/**
 * Generate a new ML-KEM-1024 + ML-DSA-87 key pair, store the private key
 * encrypted in IndexedDB, and return the public keys.
 */
export async function generateAndStore(password: string): Promise<PhantomPublicKeys> {
  const kemKeys = ml_kem1024.keygen();
  const dsaKeys = ml_dsa87.keygen();

  const salt = crypto.getRandomValues(new Uint8Array(16)).slice() as Uint8Array<ArrayBuffer>;
  const iv   = crypto.getRandomValues(new Uint8Array(12)).slice() as Uint8Array<ArrayBuffer>;
  const wrappingKey = await deriveKey(password, salt);

  const plaintext = new TextEncoder().encode(
    JSON.stringify({ kemSecret: toB64(kemKeys.secretKey), signingSecret: toB64(dsaKeys.secretKey) }),
  );
  const ciphertext = await crypto.subtle.encrypt({ name: "AES-GCM", iv }, wrappingKey, plaintext);

  const blob: StoredKeyBlob = {
    salt: toB64(salt),
    iv:   toB64(iv),
    data: toB64(new Uint8Array(ciphertext)),
  };

  const db = await openDb();
  await idbPut(db, KEY_ID, blob);
  db.close();

  return {
    kemPublic:     toB64(kemKeys.publicKey),
    signingPublic: toB64(dsaKeys.publicKey),
  };
}

/** True if a key blob exists in IndexedDB (not yet decrypted / verified). */
export async function hasStoredKeys(): Promise<boolean> {
  try {
    const db = await openDb();
    const val = await idbGet(db, KEY_ID);
    db.close();
    return val != null;
  } catch {
    return false;
  }
}

/**
 * Decrypt and return the stored private keys.
 * Throws if no keys are stored or the password is wrong.
 */
export async function loadPrivateKeys(password: string): Promise<{ kemSecret: Uint8Array; signingSecret: Uint8Array }> {
  const db = await openDb();
  const blob = await idbGet(db, KEY_ID) as StoredKeyBlob | undefined;
  db.close();

  if (!blob) throw new Error("No Phantom keys stored on this device");

  const salt = fromB64(blob.salt);
  const iv   = fromB64(blob.iv);
  const data = fromB64(blob.data);
  const wrappingKey = await deriveKey(password, salt);

  const plaintext = await crypto.subtle.decrypt({ name: "AES-GCM", iv }, wrappingKey, data);
  const { kemSecret, signingSecret } = JSON.parse(new TextDecoder().decode(plaintext));

  return { kemSecret: fromB64(kemSecret), signingSecret: fromB64(signingSecret) };
}

/**
 * Export private keys as an encrypted JSON blob the user can save.
 * The format is identical to the IndexedDB blob, so importKeys() can consume it.
 */
export async function exportKeys(password: string): Promise<string> {
  const db = await openDb();
  const blob = await idbGet(db, KEY_ID) as StoredKeyBlob | undefined;
  db.close();
  if (!blob) throw new Error("No Phantom keys to export");
  return JSON.stringify({ version: 1, ...blob });
}

/**
 * Import a previously exported key blob, re-encrypting under a new password.
 * Verifies the old password is correct before storing.
 */
export async function importKeys(exportedJson: string, oldPassword: string, newPassword: string): Promise<void> {
  // Verify old password decrypts correctly.
  const obj = JSON.parse(exportedJson) as StoredKeyBlob & { version?: number };
  const blob: StoredKeyBlob = { salt: obj.salt, iv: obj.iv, data: obj.data };

  const salt = fromB64(blob.salt);
  const iv   = fromB64(blob.iv);
  const data = fromB64(blob.data);
  const oldKey = await deriveKey(oldPassword, salt);
  const plaintext = await crypto.subtle.decrypt({ name: "AES-GCM", iv }, oldKey, data);

  // Re-encrypt under new password.
  const newSalt = crypto.getRandomValues(new Uint8Array(16)).slice() as Uint8Array<ArrayBuffer>;
  const newIv   = crypto.getRandomValues(new Uint8Array(12)).slice() as Uint8Array<ArrayBuffer>;
  const newKey  = await deriveKey(newPassword, newSalt);
  const ciphertext = await crypto.subtle.encrypt({ name: "AES-GCM", iv: newIv }, newKey, plaintext);

  const newBlob: StoredKeyBlob = {
    salt: toB64(newSalt),
    iv:   toB64(newIv),
    data: toB64(new Uint8Array(ciphertext)),
  };

  const db = await openDb();
  await idbPut(db, KEY_ID, newBlob);
  db.close();
}

/** Remove stored keys from this device. */
export async function clearKeys(): Promise<void> {
  try {
    const db = await openDb();
    await idbDelete(db, KEY_ID);
    db.close();
  } catch { /* already gone */ }
}
