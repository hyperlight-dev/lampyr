const path = require('node:path');
const {once} = require('node:events');
const {Readable} = require('node:stream');
const Fastify = require('fastify');
const websocket = require('@fastify/websocket');
const {createWebSocketStream} = require('ws');
const {Body, Headers, Response, HandlerError} = require(path.resolve(__dirname, '../.lampyr-adapter/types'));
const hooks = require(path.resolve(__dirname, '../handler.js'));

function validate(requireEvent) {
  if (typeof hooks.handle !== 'function') throw new TypeError('required handle(request, context) hook is missing');
  for (const name of ['handleEvent', 'handleWebSocket']) {
    if (hooks[name] !== undefined && typeof hooks[name] !== 'function') {
      throw new TypeError(`${name} must be a function`);
    }
  }
  if (requireEvent && !hooks.handleEvent) throw new TypeError('events policy requires handleEvent');
}
validate(process.argv.includes('--require-event') || process.env.LAMPYR_REQUIRE_EVENT === '1');
if (process.argv.includes('--check')) process.exit(0);

function requestHead(req) {
  const headers = new Headers();
  for (let index = 0; index < req.raw.rawHeaders.length; index += 2) {
    headers.append(req.raw.rawHeaders[index], req.raw.rawHeaders[index + 1]);
  }
  return {
    method: req.method, path: req.raw.url,
    headers,
  };
}

async function eventOf(request) {
  const h = request.headers;
  let data = await request.body.text();
  if (mediaType(h) === 'application/cloudevents+json') {
    let event;
    try { event = JSON.parse(data); } catch { throw new HandlerError(400, 'invalid CloudEvent'); }
    if (!event || event.specversion !== '1.0' ||
        !['id', 'type', 'source'].every(name => typeof event[name] === 'string' && event[name])) {
      throw new HandlerError(400, 'invalid CloudEvent');
    }
    if (['subject', 'time'].some(name => event[name] !== undefined && typeof event[name] !== 'string')) {
      throw new HandlerError(400, 'invalid CloudEvent');
    }
    if (Object.hasOwn(event, 'data_base64')) {
      throw new HandlerError(400, 'unsupported CloudEvent encoding');
    }
    return {
      id: event.id, type: event.type, source: event.source,
      subject: event.subject || '', time: event.time || '',
      attempt: attemptOf(h), data: event.data ?? null,
    };
  }
  if (h.get('ce-specversion') !== '1.0' || !h.get('ce-id') || !h.get('ce-type') || !h.get('ce-source')) throw new HandlerError(400, 'invalid CloudEvent');
  try { data = JSON.parse(data); } catch { /* Non-JSON data stays text. */ }
  return {
    id: h.get('ce-id'), type: h.get('ce-type'), source: h.get('ce-source'),
    subject: h.get('ce-subject') || '', time: h.get('ce-time') || '', attempt: attemptOf(h), data,
  };
}

function mediaType(headers) {
  return (headers.get('content-type') || '').split(';')[0].trim().toLowerCase();
}

function attemptOf(headers) {
  const raw = headers.get('x-lampyr-delivery-attempt') ?? '1';
  const value = Number(raw);
  if (!/^[0-9]+$/.test(raw)) throw new HandlerError(400, 'invalid delivery attempt');
  if (!Number.isSafeInteger(value) || value < 1) throw new HandlerError(400, 'invalid delivery attempt');
  return value;
}

const active = new Set();
const server = Fastify({bodyLimit: 1024 * 1024, forceCloseConnections: true, exposeHeadRoutes: false});
server.removeAllContentTypeParsers();
server.addContentTypeParser('*', (_req, payload, done) => done(null, payload));
server.decorateRequest('context', null);
server.decorateRequest('controller', null);
server.addHook('onRequest', async (req, reply) => {
  const controller = new AbortController();
  active.add(controller);
  req.controller = controller;
  req.context = {signal: controller.signal};
  req.raw.on('aborted', () => controller.abort());
  reply.raw.on('close', () => { controller.abort(); active.delete(controller); });
  if (Number(req.headers['content-length']) > 1024 * 1024) {
    return reply.code(413).type('text/plain').send('request body too large');
  }
});
server.addHook('preValidation', async (req, reply) => {
  if (req.ws && !hooks.handleWebSocket) return reply.code(404).send();
});
// Browsers must not sniff handler or error bytes into HTML; a handler may set
// its own X-Content-Type-Options.
server.addHook('onSend', async (_req, reply, payload) => {
  if (!reply.hasHeader('x-content-type-options')) reply.header('x-content-type-options', 'nosniff');
  return payload;
});
server.setErrorHandler((error, _req, reply) => {
  console.error(error);
  const status = error instanceof HandlerError ? error.status
    : error.code?.startsWith('FST_ERR_') ? error.statusCode ?? 500 : 500;
  reply.code(status).type('text/plain').send(
    error instanceof HandlerError ? error.message : status === 413 ? 'request body too large' : 'request failed');
});

async function handle(req, reply) {
  const head = requestHead(req);
  const request = {...head, body: new Body(req.raw.iterator({destroyOnReturn: false}), 1024 * 1024)};
  const isEvent = Array.from(request.headers).some(([name]) => name.startsWith('ce-')) ||
    mediaType(request.headers) === 'application/cloudevents+json';
  let response;
  try {
    response = isEvent
      ? hooks.handleEvent ? await hooks.handleEvent(await eventOf(request), {...req.context, request: head})
        : new Response(undefined, {status: 404})
      : await hooks.handle(request, req.context);
    if (!(response instanceof Response) || !(response.body instanceof Body) || !(response.headers instanceof Headers)) {
      throw new TypeError('hook must return a Response with a byte-stream Body and Headers');
    }
    const status = response.status;
    if (!Number.isInteger(status) || status < 200 || status > 599) throw new TypeError('invalid response status');
    const headers = Object.create(null);
    for (const [name, value] of response.headers) {
      if (['content-length', 'transfer-encoding', 'connection'].includes(name)) {
        throw new TypeError('framing headers are adapter-owned');
      }
      (headers[name] ??= []).push(value);
    }
    reply.code(status).headers(headers);
    if (req.method === 'HEAD' || status === 204 || status === 304) {
      await response.body.close();
      await request.body.close();
      return reply.send();
    }
    const output = Readable.from((async function* () {
      try {
        yield* response.body;
      } finally {
        await request.body.close();
      }
    })(), {objectMode: false, highWaterMark: 1});
    output.on('error', error => console.error(error));
    return reply.send(output);
  } catch (error) {
    if (response?.body instanceof Body) await response.body.close();
    await request.body.close();
    throw error;
  }
}

async function handleWebSocket(socket, req) {
  const stream = createWebSocketStream(socket, {readableObjectMode: true, highWaterMark: 1});
  stream.on('error', error => console.error(error));
  const context = {...req.context, request: requestHead(req)};
  req.context.signal.addEventListener('abort', () => socket.terminate(), {once: true});
  socket.on('close', () => {
    req.controller.abort();
    active.delete(req.controller);
    stream.destroy();
  });
  const session = {
    send(data) {
      return new Promise((resolve, reject) => {
        socket.send(data, error => error ? reject(error) : resolve());
      });
    },
    async close(code = 1000, reason = '') { socket.close(code, reason); },
    [Symbol.asyncIterator]() { return stream[Symbol.asyncIterator](); },
  };
  await hooks.handleWebSocket(session, context);
  if (socket.readyState === socket.OPEN) socket.close();
}

async function start() {
  await server.register(websocket, {
    options: {maxPayload: 1024 * 1024, perMessageDeflate: false},
    errorHandler(error, socket) { console.error(error); socket.terminate(); },
  });
  server.get('/*', hooks.handleWebSocket ? {wsHandler: handleWebSocket} : {}, handle);
  server.route({method: ['HEAD', 'POST', 'PUT', 'PATCH', 'DELETE', 'OPTIONS'], url: '/*', handler: handle});
  server.addHook('preClose', async () => {
    for (const controller of active) controller.abort();
    server.server.closeAllConnections();
  });
  for (const signal of ['SIGINT', 'SIGTERM']) {
    process.on(signal, () => server.close().catch(error => { console.error(error); process.exitCode = 1; }));
  }
  // Fastify's listen helper enumerates interfaces, which the guest does not expose.
  await server.ready();
  server.server.listen({port: Number(process.env.PORT || 8080), host: process.env.LAMPYR_LISTEN_HOST || '0.0.0.0'});
  await once(server.server, 'listening');
  console.log(`listening on ${server.server.address().port}`);
}
start().catch(error => { console.error(error); process.exitCode = 1; });
