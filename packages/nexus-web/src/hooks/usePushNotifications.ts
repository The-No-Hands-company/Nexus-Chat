/**
 * usePushNotifications — Web Push subscription lifecycle hook.
 *
 * Registers the service worker, requests notification permission from the user,
 * fetches the server's VAPID public key, creates a PushManager subscription,
 * and POSTs it to the Nexus API.
 *
 * Also listens for `pushsubscriptionchange` messages from the SW so it can
 * relay rotated subscriptions back to the server without user interaction.
 *
 * Usage: call `enablePush()` when the user explicitly opts in.
 */

import { useEffect, useCallback, useRef } from "react";
import { useStore } from "../store";

interface PushSubscriptionKeys {
  p256dh: string;
  auth: string;
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/** Convert a base64url string to a Uint8Array (for applicationServerKey). */
function urlBase64ToUint8Array(base64String: string): Uint8Array {
  const padding = "=".repeat((4 - (base64String.length % 4)) % 4);
  const base64 = (base64String + padding).replace(/-/g, "+").replace(/_/g, "/");
  const rawData = window.atob(base64);
  return Uint8Array.from([...rawData].map((char) => char.charCodeAt(0)));
}

/** Convert an ArrayBuffer to base64url (for sending keys to the server). */
function arrayBufferToBase64Url(buffer: ArrayBuffer): string {
  const bytes = new Uint8Array(buffer);
  let binary = "";
  for (let i = 0; i < bytes.byteLength; i++) {
    binary += String.fromCharCode(bytes[i]);
  }
  return window
    .btoa(binary)
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=/g, "");
}

// ── Hook ──────────────────────────────────────────────────────────────────────

export function usePushNotifications() {
  const { session } = useStore();
  const registrationRef = useRef<ServiceWorkerRegistration | null>(null);

  // ── Register service worker on mount ───────────────────────────────────────
  useEffect(() => {
    if (!("serviceWorker" in navigator) || !("PushManager" in window)) return;

    navigator.serviceWorker
      .register("/sw.js", { scope: "/" })
      .then((reg) => {
        registrationRef.current = reg;
      })
      .catch((err) => {
        console.error("[Nexus] Service worker registration failed:", err);
      });

    // Listen for subscription rotations from the SW
    const handleMessage = async (event: MessageEvent) => {
      if (event.data?.type !== "PUSH_SUBSCRIPTION_CHANGED") return;
      const sub = event.data.subscription as ReturnType<PushSubscription["toJSON"]>;
      if (sub && session) {
        await sendSubscriptionToServer(session.serverUrl, session.accessToken, sub);
      }
    };
    navigator.serviceWorker.addEventListener("message", handleMessage);
    return () => {
      navigator.serviceWorker.removeEventListener("message", handleMessage);
    };
  }, [session]);

  // ── enablePush: called when user opts in ──────────────────────────────────
  const enablePush = useCallback(async (): Promise<boolean> => {
    if (!session) return false;
    if (!("serviceWorker" in navigator) || !("PushManager" in window)) {
      console.warn("[Nexus] Push not supported in this browser");
      return false;
    }

    // 1. Request notification permission
    const permission = await Notification.requestPermission();
    if (permission !== "granted") {
      console.info("[Nexus] Notification permission denied");
      return false;
    }

    try {
      // 2. Wait for the SW to be ready
      const reg =
        registrationRef.current ??
        (await navigator.serviceWorker.ready);
      registrationRef.current = reg;

      // 3. Fetch VAPID public key from the server
      const keyRes = await fetch(
        `${session.serverUrl}/api/v1/push/vapid-public-key`,
        { headers: { Authorization: `Bearer ${session.accessToken}` } }
      );
      if (!keyRes.ok) {
        console.warn("[Nexus] Push not available on this server (no VAPID key)");
        return false;
      }
      const { public_key: vapidPublicKey } = (await keyRes.json()) as {
        public_key: string;
      };

      // 4. Subscribe via PushManager
      const subscription = await reg.pushManager.subscribe({
        userVisibleOnly: true,
        applicationServerKey: urlBase64ToUint8Array(vapidPublicKey) as BufferSource,
      });

      // 5. Send subscription to the Nexus API
      const subJson = subscription.toJSON();
      const sent = await sendSubscriptionToServer(
        session.serverUrl,
        session.accessToken,
        subJson
      );

      if (sent) {
        console.info("[Nexus] Push notifications enabled");
      }
      return sent;
    } catch (err) {
      console.error("[Nexus] Failed to enable push notifications:", err);
      return false;
    }
  }, [session]);

  // ── disablePush: unsubscribe and remove from server ───────────────────────
  const disablePush = useCallback(async (): Promise<void> => {
    if (!session) return;
    const reg =
      registrationRef.current ??
      (await navigator.serviceWorker.ready.catch(() => null));
    if (!reg) return;

    const sub = await reg.pushManager.getSubscription().catch(() => null);
    if (!sub) return;

    const endpoint = sub.endpoint;
    await sub.unsubscribe().catch(() => {});

    // Tell the server to remove this subscription
    await fetch(`${session.serverUrl}/api/v1/push/subscriptions`, {
      method: "DELETE",
      headers: {
        Authorization: `Bearer ${session.accessToken}`,
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ endpoint }),
    }).catch(() => {});
  }, [session]);

  return { enablePush, disablePush };
}

// ── Internal helper ───────────────────────────────────────────────────────────

async function sendSubscriptionToServer(
  serverUrl: string,
  accessToken: string,
  sub: ReturnType<PushSubscription["toJSON"]>
): Promise<boolean> {
  const keys = sub.keys as PushSubscriptionKeys | undefined;
  if (!sub.endpoint || !keys?.p256dh || !keys?.auth) return false;

  const res = await fetch(`${serverUrl}/api/v1/push/subscriptions`, {
    method: "POST",
    headers: {
      Authorization: `Bearer ${accessToken}`,
      "Content-Type": "application/json",
    },
    body: JSON.stringify({
      endpoint: sub.endpoint,
      keys: {
        p256dh: keys.p256dh,
        auth: keys.auth,
      },
    }),
  });

  return res.ok;
}
