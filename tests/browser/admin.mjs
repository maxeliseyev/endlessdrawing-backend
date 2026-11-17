// Real TLS/HTTP/WS forwarding. No mocked responses, navigator.credentials or WebSockets.
// Self-signed localhost TLS and CDP authenticators are TEST ONLY, never Touch ID acceptance.
import { createRequire } from 'node:module';
import { resolve, join } from 'node:path';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { execFileSync } from 'node:child_process';
import { createServer } from 'node:https';
import { request } from 'node:http';
import { connect } from 'node:net';
import { createInterface } from 'node:readline';
import { randomUUID, createHmac } from 'node:crypto';

function testTotp(secret, step = Math.floor(Date.now() / 30000)) {
  let bits = 0, value = 0; const bytes = [];
  for (const character of secret) {
    value = (value << 5) | 'ABCDEFGHIJKLMNOPQRSTUVWXYZ234567'.indexOf(character); bits += 5;
    if (bits >= 8) { bits -= 8; bytes.push((value >>> bits) & 255); value &= (1 << bits) - 1; }
  }
  const counter = Buffer.alloc(8); counter.writeBigInt64BE(BigInt(step));
  const digest = createHmac('sha1', Buffer.from(bytes)).update(counter).digest();
  const offset = digest[19] & 15;
  return String((digest.readUInt32BE(offset) & 0x7fffffff) % 1000000).padStart(6, '0');
}

const require = createRequire(join(resolve(process.argv[2]), 'package.json'));
const { chromium, expect: baseExpect } = require('@playwright/test');
const expect = baseExpect.configure({ timeout: 15_000 });
const directory = mkdtempSync(join(tmpdir(), 'endlessdrawing-admin-tls-'));
let backend, browser, step = 'TLS bootstrap';
execFileSync('openssl', ['req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-days', '1',
  '-subj', '/CN=localhost', '-addext', 'subjectAltName=DNS:localhost',
  '-keyout', join(directory, 'key.pem'), '-out', join(directory, 'cert.pem')], { stdio: 'ignore' });
const proxy = createServer({ key: readFileSync(join(directory, 'key.pem')), cert: readFileSync(join(directory, 'cert.pem')) }, (incoming, outgoing) => {
  const upstream = request(new URL(incoming.url, backend), { method: incoming.method, headers: incoming.headers }, (response) => {
    outgoing.writeHead(response.statusCode, response.headers); response.pipe(outgoing);
  });
  upstream.on('error', () => { outgoing.writeHead(502); outgoing.end(); });
  incoming.pipe(upstream);
});
proxy.on('upgrade', (incoming, socket, head) => {
  const url = new URL(backend);
  const upstream = connect(Number(url.port), url.hostname, () => {
    upstream.write(`${incoming.method} ${incoming.url} HTTP/${incoming.httpVersion}\r\n`);
    for (let i = 0; i < incoming.rawHeaders.length; i += 2) upstream.write(`${incoming.rawHeaders[i]}: ${incoming.rawHeaders[i + 1]}\r\n`);
    upstream.write('\r\n'); if (head.length) upstream.write(head);
    socket.pipe(upstream); upstream.pipe(socket);
  });
  upstream.on('error', () => socket.destroy()); socket.on('error', () => upstream.destroy());
  socket.on('close', () => upstream.destroy()); upstream.on('close', () => socket.destroy());
});
await new Promise((ready) => proxy.listen(0, '127.0.0.1', ready));
const origin = `https://localhost:${proxy.address().port}`;
console.log(JSON.stringify({ origin }));
const lines = createInterface({ input: process.stdin });
const config = await new Promise((ready) => lines.once('line', (line) => ready(JSON.parse(line))));
lines.close(); backend = config.backend;
let admin;
try {
  step = 'launch Chromium';
  browser = await chromium.launch();
  const options = { ignoreHTTPSErrors: true, locale: 'en-US', viewport: { width: 1200, height: 800 } };
  const owner = await browser.newContext(options), visitor = await browser.newContext(options);
  const ownerPage = await owner.newPage(), visitorPage = await visitor.newPage();
  const publicRequests = [];
  for (const page of [ownerPage, visitorPage]) {
    page.on('request', (request) => publicRequests.push(new URL(request.url()).pathname));
    await page.goto(origin);
    await page.evaluate(async () => {
      const socket = new WebSocket(`${location.origin.replace('https:', 'wss:')}/ws`);
      const messages = [];
      socket.onmessage = (event) => messages.push(JSON.parse(event.data));
      window.testProtocol = { socket, messages };
      await new Promise((ready, reject) => { socket.onopen = ready; socket.onerror = reject; });
      const secret = crypto.getRandomValues(new Uint8Array(32));
      const encoded = btoa(String.fromCharCode(...secret)).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
      socket.send(JSON.stringify({ type: 'hello', session_secret: encoded }));
    });
    await expect.poll(() => page.evaluate(() => window.testProtocol.messages.some((m) => m.type === 'hello.ack'))).toBe(true);
    await page.evaluate(() => window.testProtocol.socket.send(JSON.stringify({ type: 'viewport.subscribe', subscription_id: 'initial', bounds: { min_x: -500, min_y: -500, max_x: 500, max_y: 500 } })));
    await expect.poll(() => page.evaluate(() => window.testProtocol.messages.some((m) => m.type === 'viewport.snapshot.end'))).toBe(true);
  }
  step = 'two real public protocol clients and owner-only deletion';
  const id = randomUUID();
  await ownerPage.evaluate((id) => window.testProtocol.socket.send(JSON.stringify({ type: 'stroke.create', request_id: 'create', stroke: {
    id, points: [[0, 0], [30, 0]], brush: 'round', width: 8, color: '#202020', opacity: 1,
  } })), id);
  for (const page of [ownerPage, visitorPage]) {
    await expect.poll(() => page.evaluate((id) => window.testProtocol.messages.some((m) => m.type === 'stroke.created' && m.stroke.id === id), id)).toBe(true);
  }
  await visitorPage.evaluate((id) => window.testProtocol.socket.send(JSON.stringify({ type: 'stroke.delete', request_id: 'foreign', stroke_id: id })), id);
  await expect.poll(() => visitorPage.evaluate(() => window.testProtocol.messages.find((m) => m.request_id === 'foreign')?.code)).toBe('forbidden');
  expect(publicRequests.some((path) => path.startsWith('/api/admin/'))).toBe(false);

  step = 'native WebAuthn registration against real backend';
  const adminContext = await browser.newContext(options);
  admin = await adminContext.newPage();
  const pageErrors = [];
  admin.on('pageerror', (error) => pageErrors.push(error.name));
  const cdp = await adminContext.newCDPSession(admin);
  await cdp.send('WebAuthn.enable');
  const add = () => cdp.send('WebAuthn.addVirtualAuthenticator', { options: {
    protocol: 'ctap2', transport: 'internal', hasResidentKey: true, hasUserVerification: true,
    isUserVerified: true, automaticPresenceSimulation: true,
  } });
  let primary = (await add()).authenticatorId;
  let backup, primaryCredentials;
  const response = await admin.goto(`${origin}/admin/`);
  expect(response.headers()['cache-control']).toBe('no-store');
  expect(response.headers()['x-frame-options']).toBe('DENY');
  await expect(admin.getByRole('heading', { name: 'Administrator sign in' })).toBeVisible();
  await admin.getByRole('button', { name: 'Register administrator passkeys', exact: true }).click();
  for (let index = 0; index < 2; index++) {
    await admin.getByLabel('Login', { exact: true }).fill(config.login);
    await admin.getByLabel('Password', { exact: true }).fill(config.password);
    await admin.getByLabel('Enrollment token', { exact: true }).fill(config.enrollment_token);
    await admin.getByLabel('Passkey label', { exact: true }).fill(`Virtual ${index + 1}`);
    await admin.getByRole('button', { name: 'Register passkey', exact: true }).click();
    if (index === 0) {
      await expect(admin.getByRole('status')).toContainText('Passkey setup complete');
      await admin.getByRole('button', { name: 'Register administrator passkeys', exact: true }).click();
      // Chromium permits one internal authenticator at a time. Private TEST credentials
      // stay only in this process memory, never stdout/artifacts or production storage.
      primaryCredentials = (await cdp.send('WebAuthn.getCredentials', { authenticatorId: primary })).credentials;
      await cdp.send('WebAuthn.removeVirtualAuthenticator', { authenticatorId: primary });
      backup = (await add()).authenticatorId;
    }
  }
  await expect(admin.getByRole('heading', { name: 'Administrator sign in' })).toBeVisible();
  expect((await adminContext.cookies()).some((cookie) => cookie.name === '__Host-ed-admin')).toBe(false);
  const login = async () => {
    await admin.getByLabel('Login', { exact: true }).fill(config.login);
    await admin.getByLabel('Password', { exact: true }).fill(config.password);
    await admin.getByRole('button', { name: 'Sign in with passkey', exact: true }).click();
    await expect(admin.locator('.world-status')).toHaveText('Shared world loaded');
  };
  step = 'backup login, secure cookies and actual WS snapshot';
  await login();
  const cookie = (await adminContext.cookies()).find((cookie) => cookie.name === '__Host-ed-admin');
  expect(cookie && { httpOnly: cookie.httpOnly, secure: cookie.secure, sameSite: cookie.sameSite, path: cookie.path })
    .toEqual({ httpOnly: true, secure: true, sameSite: 'Strict', path: '/' });
  expect(await admin.evaluate(() => document.cookie.includes('__Host-ed-admin'))).toBe(false);
  expect(await admin.evaluate(() => ({ ...localStorage, ...sessionStorage }))).toEqual({});
  step = 'fresh UV session configures local Google Authenticator QR and confirms a real TOTP';
  await admin.getByRole('button', { name: 'Google Authenticator backup', exact: true }).click();
  await admin.getByRole('button', { name: 'Connect Google Authenticator', exact: true }).click();
  await expect(admin.locator('.totp-qr')).toBeVisible();
  let totpSecret = await admin.getByLabel('Manual setup key (keep private)', { exact: true }).inputValue();
  await admin.getByLabel('Six-digit code', { exact: true }).fill(testTotp(totpSecret));
  await admin.getByRole('button', { name: 'Confirm backup code', exact: true }).click();
  await expect(admin.locator('.totp-dialog')).toHaveCount(0);
  expect(await admin.evaluate(() => ({ ...localStorage, ...sessionStorage }))).toEqual({});
  step = 'selection, confirmation, real audited hide and two-client live deletion';
  const bounds = await admin.locator('canvas').boundingBox();
  await admin.mouse.click(bounds.x + bounds.width / 2 + 5, bounds.y + bounds.height / 2);
  await expect(admin.getByRole('heading', { name: 'Selected strokes: 1 / 100' })).toBeVisible();
  await admin.getByRole('button', { name: 'Review hiding', exact: true }).click();
  await expect(admin.getByRole('dialog')).toContainText(id);
  await admin.getByRole('dialog').getByLabel('Reason', { exact: true }).fill('Ephemeral real integration test');
  await admin.getByRole('dialog').getByRole('button', { name: 'Hide selected strokes', exact: true }).click();
  await expect(admin.getByRole('status').first()).toContainText('Confirmed: 1 hidden');
  for (const page of [ownerPage, visitorPage]) {
    await expect.poll(() => page.evaluate((id) => window.testProtocol.messages.some((m) => m.type === 'stroke.deleted' && m.stroke_id === id), id)).toBe(true);
    await page.evaluate(() => {
      window.testProtocol.messages.length = 0;
    });
    // A separate listener records the fresh snapshot, without mutating production code.
    await page.evaluate(() => {
      window.testProtocol.snapshot = [];
      window.testProtocol.socket.addEventListener('message', (event) => window.testProtocol.snapshot.push(JSON.parse(event.data)));
      window.testProtocol.socket.send(JSON.stringify({ type: 'viewport.subscribe', subscription_id: 'after-hide', bounds: { min_x: -500, min_y: -500, max_x: 500, max_y: 500 } }));
    });
    await expect.poll(() => page.evaluate(() => window.testProtocol.snapshot.some((m) => m.type === 'viewport.snapshot.end' && m.subscription_id === 'after-hide'))).toBe(true);
    expect(await page.evaluate((id) => window.testProtocol.snapshot.some((m) => m.strokes?.some((s) => s.id === id)), id)).toBe(false);
  }
  step = 'server logout and primary credential login';
  await admin.getByRole('button', { name: 'Sign out', exact: true }).click();
  await expect(admin.getByRole('heading', { name: 'Administrator sign in' })).toBeVisible();
  expect((await admin.request.get(`${origin}/api/admin/session`)).status()).toBe(401);
  await cdp.send('WebAuthn.removeVirtualAuthenticator', { authenticatorId: backup });
  primary = (await add()).authenticatorId;
  for (const credential of primaryCredentials) await cdp.send('WebAuthn.addCredential', { authenticatorId: primary, credential });
  primaryCredentials = undefined;
  await login();
  await admin.getByRole('button', { name: 'Sign out', exact: true }).click();
  await expect(admin.getByRole('heading', { name: 'Administrator sign in' })).toBeVisible();
  step = 'password plus Google Authenticator backup login; fallback cannot change factors';
  await admin.getByRole('button', { name: 'Use Google Authenticator backup', exact: true }).click();
  await admin.getByLabel('Login', { exact: true }).fill(config.login);
  await admin.getByLabel('Password', { exact: true }).fill(config.password);
  await admin.getByLabel('Six-digit code', { exact: true }).fill(testTotp(totpSecret, Math.floor(Date.now() / 30000) + 1));
  totpSecret = '';
  await admin.getByRole('button', { name: 'Sign in with backup code', exact: true }).click();
  await expect(admin.locator('.world-status')).toHaveText('Shared world loaded');
  await admin.getByRole('button', { name: 'Google Authenticator backup', exact: true }).click();
  await expect(admin.locator('.totp-dialog')).toContainText('Backup-code sessions cannot change factors');
  await expect(admin.locator('.totp-qr')).toHaveCount(0);
  await admin.getByRole('button', { name: 'Close', exact: true }).click();
  await admin.getByRole('button', { name: 'Sign out', exact: true }).click();
  await expect(admin.getByRole('heading', { name: 'Administrator sign in' })).toBeVisible();
  expect(pageErrors).toEqual([]);
  console.log('PASS: real HTTPS frontend/backend UV and TOTP backup login, encrypted setup, cookies, WS, audited hide, snapshots and logout.');
} catch (error) {
  // Never dump form inputs, request bodies, cookies or CDP private credentials.
  const notice = admin ? await admin.locator('.notice').textContent().catch(() => '') : '';
  console.error(`FAIL at ${step} (${error.name}); UI: ${notice}`);
  process.exitCode = 1;
} finally {
  if (browser) await browser.close();
  proxy.closeAllConnections(); proxy.close();
  rmSync(directory, { recursive: true, force: true });
}
