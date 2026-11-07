// The runner runs a snippet of user code and answers with its result. It is
// a plain Node.js HTTP server with no sandbox of its own: Lampyr runs it as a
// Fiber, a Hyperlight micro-VM restored from a snapshot for each request, and
// that micro-VM is the sandbox. The tour's container baseline runs the same
// file in an ordinary container.
const http = require('node:http');
const crypto = require('node:crypto');
const fs = require('node:fs');

const MAX_BODY_BYTES = 64 * 1024;
const MAX_WORK_MS = 120000;

// Snippets share this process's globals; keep our own copy of what answering
// needs (JSON.stringify, at least), so a snippet that replaces it can't break
// later answers.
const stringify = JSON.stringify;

// Set once per VM. A fresh Fiber per request means every response shows a
// new id.
const vm = crypto.randomUUID().slice(0, 8);
const AsyncFunction = (async () => {}).constructor;

// An error in the request itself: answered with its own status.
class HttpError extends Error {
  constructor(status, message, headers = {}) {
    super(message);
    this.status = status;
    this.headers = headers;
  }
}

function send(response, status, body, headers = {}) {
  // Serialize first: a result JSON can't represent (a BigInt, a cycle) must
  // fail before any header is written, so the error can still be answered.
  const text = stringify(body) + '\n';
  response.writeHead(status, {...headers, 'content-type': 'application/json'});
  response.end(text);
}

async function readBody(request, limit) {
  const chunks = [];
  let size = 0;
  for await (const chunk of request) {
    size += chunk.length;
    // Close the connection: the rest of the body is never read.
    if (size > limit) throw new HttpError(413, `body too large (max ${limit} bytes)`, {connection: 'close'});
    chunks.push(chunk);
  }
  return Buffer.concat(chunks).toString('utf8');
}

// What a snippet threw can be anything, including an object whose toString
// throws; describing it must not fail.
function describe(error) {
  try {
    return error instanceof HttpError ? error.message : String(error);
  } catch {
    return 'the snippet threw a value that cannot be described';
  }
}

function only(request, ...methods) {
  if (!methods.includes(request.method)) {
    throw new HttpError(405, `method ${request.method} not allowed`, {allow: methods.join(', ')});
  }
}

// A snippet can leave work behind that fails after its answer, such as a
// rejected promise nobody awaits. Log it and keep serving.
process.on('unhandledRejection', (reason) => console.error('a snippet left an unhandled rejection:', describe(reason)));
process.on('uncaughtException', (error) => console.error('a snippet left an uncaught exception:', describe(error)));
process.on('SIGTERM', () => process.exit(0));

const server = http.createServer(async (request, response) => {
  try {
    let url;
    try {
      if (!request.url.startsWith('/')) throw new Error('not a path');
      url = new URL('http://localhost' + request.url);
    } catch {
      throw new HttpError(400, 'bad request URL');
    }
    if (url.pathname === '/') {
      only(request, 'GET', 'HEAD');
      return send(response, 200, {service: 'runner', vm});
    }
    if (url.pathname === '/run') {
      only(request, 'POST');
      // The snippet is the body of an async function, with `fs` in scope.
      // Whatever it throws, or a result JSON can't represent, is a 422.
      const code = await readBody(request, MAX_BODY_BYTES);
      try {
        const result = await new AsyncFunction('fs', code)(fs);
        if (typeof result === 'function' || typeof result === 'symbol') {
          throw new TypeError(`a ${typeof result} is not a JSON value`);
        }
        return send(response, 200, {result: result === undefined ? null : result, vm});
      } catch (error) {
        throw new HttpError(422, describe(error));
      }
    }
    if (url.pathname === '/work') {
      only(request, 'GET');
      // Holds the Fiber for a while, so concurrent requests show up as
      // concurrent Fibers.
      const param = url.searchParams.get('ms') ?? '1000';
      const ms = /^\d+$/.test(param) ? Number(param) : NaN;
      if (!(ms <= MAX_WORK_MS)) {
        throw new HttpError(400, `ms must be an integer from 0 to ${MAX_WORK_MS}`);
      }
      await new Promise((resolve) => setTimeout(resolve, ms));
      return send(response, 200, {service: 'runner', vm, worked_ms: ms});
    }
    send(response, 404, {error: 'not found', vm});
  } catch (error) {
    if (response.headersSent) return response.destroy();
    try {
      const known = error instanceof HttpError;
      send(response, known ? error.status : 500, {error: describe(error), vm}, known ? error.headers : {});
    } catch {
      response.destroy();
    }
  }
});
// A server error, such as the port being taken, is fatal: don't let the
// uncaughtException handler above keep a process that can't serve.
server.on('error', (error) => {
  console.error('runner server error:', error);
  process.exit(1);
});
const port = Number(process.env.PORT || 8080);
if (!Number.isInteger(port) || port < 0 || port > 65535) {
  console.error(`runner: PORT must be a port number, not '${process.env.PORT}'`);
  process.exit(1);
}
server.listen(port, process.env.LAMPYR_LISTEN_HOST || '0.0.0.0');
