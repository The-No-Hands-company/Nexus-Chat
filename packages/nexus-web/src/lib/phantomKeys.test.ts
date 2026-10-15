import { describe, it, expect, vi, beforeEach } from "vitest";

// Stub @noble/post-quantum before the module under test imports it.
vi.mock("@noble/post-quantum/ml-kem.js", () => ({
  ml_kem1024: {
    keygen: () => ({
      publicKey: new Uint8Array(1568).fill(1),
      secretKey: new Uint8Array(3168).fill(2),
    }),
  },
}));
vi.mock("@noble/post-quantum/ml-dsa.js", () => ({
  ml_dsa87: {
    keygen: () => ({
      publicKey: new Uint8Array(2592).fill(3),
      secretKey: new Uint8Array(4896).fill(4),
    }),
  },
}));

// Minimal IndexedDB stub — in-memory Map.
const idbStore = new Map<IDBValidKey, unknown>();
const stubDB = {
  transaction: (store: string, mode: string) => ({
    objectStore: () => ({
      get: (key: IDBValidKey) => {
        const req = { result: idbStore.get(key), onsuccess: null as null | (() => void), onerror: null };
        setTimeout(() => req.onsuccess?.(), 0);
        return req;
      },
      put: (value: unknown, key: IDBValidKey) => {
        idbStore.set(key, value);
        const req = { onsuccess: null as null | (() => void), onerror: null };
        setTimeout(() => req.onsuccess?.(), 0);
        return req;
      },
      delete: (key: IDBValidKey) => {
        idbStore.delete(key);
        const req = { onsuccess: null as null | (() => void), onerror: null };
        setTimeout(() => req.onsuccess?.(), 0);
        return req;
      },
    }),
  }),
  close: () => {},
};
vi.stubGlobal("indexedDB", {
  open: () => {
    const req = {
      result: stubDB,
      onupgradeneeded: null,
      onsuccess: null as null | (() => void),
      onerror: null,
    };
    setTimeout(() => req.onsuccess?.(), 0);
    return req;
  },
});

import { generateAndStore, hasStoredKeys, clearKeys, exportKeys } from "./phantomKeys";

beforeEach(() => { idbStore.clear(); });

describe("phantomKeys", () => {
  it("generateAndStore returns base64 public keys", async () => {
    const keys = await generateAndStore("correct-horse");
    expect(keys.kemPublic).toMatch(/^[A-Za-z0-9+/]+=*$/);
    expect(keys.signingPublic).toMatch(/^[A-Za-z0-9+/]+=*$/);
    // ML-KEM-1024 public key = 1568 bytes → base64 length = ceil(1568/3)*4 = 2092
    expect(atob(keys.kemPublic).length).toBe(1568);
    expect(atob(keys.signingPublic).length).toBe(2592);
  });

  it("hasStoredKeys is false before generation", async () => {
    expect(await hasStoredKeys()).toBe(false);
  });

  it("hasStoredKeys is true after generateAndStore", async () => {
    await generateAndStore("password");
    expect(await hasStoredKeys()).toBe(true);
  });

  it("clearKeys removes stored keys", async () => {
    await generateAndStore("password");
    await clearKeys();
    expect(await hasStoredKeys()).toBe(false);
  });

  it("exportKeys returns JSON with version and salt/iv/data", async () => {
    await generateAndStore("password");
    const json = await exportKeys("password");
    const obj = JSON.parse(json);
    expect(obj.version).toBe(1);
    expect(typeof obj.salt).toBe("string");
    expect(typeof obj.iv).toBe("string");
    expect(typeof obj.data).toBe("string");
  });
});
