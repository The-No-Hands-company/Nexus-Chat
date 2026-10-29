// Production static server for nexus-web.
//
// Serves the built dist/ and performs the same identity injection the Vite dev
// server's nexusDevAuthPlugin does, so the production bundle authenticates
// exactly as the dev one did:
//   - /api/*   HTTP proxy to the REST API, with x-nexus-identity minted from
//              the browser's nexus_session cookie.
//   - /gateway WebSocket proxy to the realtime gateway, with the same identity
//              header on the upstream upgrade (the gateway rejects an upgrade
//              that carries no valid identity).
// Everything else is a static asset, falling back to index.html so client-side
// routes survive a hard reload.

const PORT = Number(process.env.PORT ?? 5175);
const DIST = new URL("./dist/", import.meta.url).pathname;
const AUTH_BASE = (process.env.NEXUS_AUTH_INTERNAL_URL ?? "http://localhost:4310").replace(/\/+$/, "");
const SERVER_NAME = process.env.NEXUS__SERVER__NAME ?? "localhost";
const API_UPSTREAM = (process.env.NEXUS_API_UPSTREAM ?? "http://localhost:8080").replace(/\/+$/, "");
const GATEWAY_UPSTREAM = (process.env.NEXUS_GATEWAY_UPSTREAM ?? "ws://localhost:8081").replace(/\/+$/, "");

// Per-session token cache; identity tokens are short-lived (~120s).
const identityCache = new Map<string, { token: string; expiresAt: number }>();

function sessionCookie(cookieHeader: string | null): string | null {
  if (!cookieHeader) return null;
  return cookieHeader.match(/(?:^|;\s*)nexus_session=([^;]+)/)?.[1] ?? null;
}

async function mintIdentityToken(session: string): Promise<string | null> {
  const cached = identityCache.get(session);
  if (cached && cached.expiresAt > Date.now() + 5_000) return cached.token;
  try {
    const res = await fetch(`${AUTH_BASE}/api/v1/auth/identity-token`, {
      method: "POST",
      headers: { "content-type": "application/json", cookie: `nexus_session=${session}` },
      body: JSON.stringify({ audience: SERVER_NAME }),
    });
    if (!res.ok) return null;
    const { token, expiresIn } = (await res.json()) as { token: string; expiresIn: number };
    identityCache.set(session, { token, expiresAt: Date.now() + expiresIn * 1000 });
    return token;
  } catch {
    return null;
  }
}

async function identityFor(req: Request): Promise<string | null> {
  const session = sessionCookie(req.headers.get("cookie"));
  return session ? mintIdentityToken(session) : null;
}

async function proxyApi(req: Request, url: URL): Promise<Response> {
  const target = new URL(url.pathname + url.search, API_UPSTREAM);
  const headers = new Headers(req.headers);
  headers.delete("host");
  const token = await identityFor(req);
  if (token) headers.set("x-nexus-identity", token);
  const init: RequestInit = { method: req.method, headers, redirect: "manual" };
  if (req.method !== "GET" && req.method !== "HEAD") {
    init.body = req.body;
    (init as { duplex?: string }).duplex = "half";
  }
  const res = await fetch(target, init);
  return new Response(res.body, { status: res.status, headers: res.headers });
}

async function serveStatic(pathname: string): Promise<Response> {
  const rel = pathname === "/" ? "index.html" : pathname.replace(/^\/+/, "");
  const file = Bun.file(DIST + rel);
  if (await file.exists()) return new Response(file);
  // SPA fallback.
  return new Response(Bun.file(DIST + "index.html"), {
    headers: { "content-type": "text/html; charset=utf-8" },
  });
}

type WsData = { upstream: WebSocket | null; queue: (string | Uint8Array)[]; token: string | null };

const server = Bun.serve<WsData>({
  port: PORT,
  hostname: process.env.NEXUS_BIND_HOST ?? "0.0.0.0",
  async fetch(req, srv) {
    const url = new URL(req.url);

    if (url.pathname === "/gateway" && req.headers.get("upgrade")?.toLowerCase() === "websocket") {
      const token = await identityFor(req);
      if (srv.upgrade(req, { data: { upstream: null, queue: [], token } })) return undefined;
      return new Response("upgrade failed", { status: 400 });
    }

    if (url.pathname.startsWith("/api/")) return proxyApi(req, url);

    return serveStatic(url.pathname);
  },
  websocket: {
    open(ws) {
      const upstreamUrl = `${GATEWAY_UPSTREAM}/gateway`;
      const upstream = new WebSocket(upstreamUrl, {
        headers: ws.data.token ? { "x-nexus-identity": ws.data.token } : {},
      } as unknown as string[]);
      ws.data.upstream = upstream;
      upstream.addEventListener("open", () => {
        for (const m of ws.data.queue) upstream.send(m);
        ws.data.queue = [];
      });
      upstream.addEventListener("message", (e: MessageEvent) => {
        try { ws.send(e.data as string | Uint8Array); } catch { /* client gone */ }
      });
      upstream.addEventListener("close", (e: CloseEvent) => {
        try { ws.close(e.code || 1000, e.reason || ""); } catch { /* already closed */ }
      });
      upstream.addEventListener("error", () => {
        try { ws.close(1011, "upstream error"); } catch { /* already closed */ }
      });
    },
    message(ws, message) {
      const u = ws.data.upstream;
      if (u && u.readyState === WebSocket.OPEN) u.send(message);
      else ws.data.queue.push(message);
    },
    close(ws) {
      try { ws.data.upstream?.close(); } catch { /* already closed */ }
    },
  },
});

console.log(`[nexus-web] production server on http://${server.hostname}:${server.port}`);
console.log(`[nexus-web]   api      -> ${API_UPSTREAM}`);
console.log(`[nexus-web]   gateway  -> ${GATEWAY_UPSTREAM}/gateway`);
console.log(`[nexus-web]   identity <- ${AUTH_BASE} (aud: ${SERVER_NAME})`);
