// The playground is the front door: it takes a snippet, checks it, and asks
// the runner to run it through the runner binding declared in lampyr.yaml
// (LAMPYR_BINDING_RUNNER). Locally the binding points at the runner in this
// workspace; in a cluster it is the runner's in-cluster URL,
// http://runner.default.svc.cluster.local.
const http = require('node:http');

const MAX_SNIPPET_BYTES = 100;
// How long a snippet may run before the playground gives up on it.
const RUN_TIMEOUT_MS = 30000;
// The most of the runner's answer the playground reads.
const MAX_ANSWER_BYTES = 1024 * 1024;

function send(response, status, body, headers = {}) {
  const text = JSON.stringify(body) + '\n';
  response.writeHead(status, {...headers, 'content-type': 'application/json'});
  response.end(text);
}

// Reads at most a little over limit bytes: enough to tell that a body is
// too long.
async function readAtMost(stream, limit) {
  const chunks = [];
  let size = 0;
  for await (const chunk of stream) {
    chunks.push(chunk);
    size += chunk.length;
    if (size > limit) break;
  }
  return Buffer.concat(chunks, size);
}

// POSTs the snippet to the runner. Aborting `signal` destroys the request's
// socket, so the runner sees the connection close and its micro-VM is
// released. Resolves with the runner's status, headers and body.
function runSnippet(base, code, signal) {
  return new Promise((resolve, reject) => {
    // Keep the binding's base path: under ly dev it is /_lampyr/<token>.
    const target = new URL('run', base.endsWith('/') ? base : base + '/');
    const upstream = http.request(target, {
      method: 'POST',
      headers: {'content-type': 'text/plain', 'content-length': code.length},
      signal,
    }, async (ran) => {
      try {
        const answer = await readAtMost(ran, MAX_ANSWER_BYTES);
        if (answer.length > MAX_ANSWER_BYTES) {
          upstream.destroy();
          return reject(Object.assign(new Error(`the runner's answer is over ${MAX_ANSWER_BYTES} bytes`), {answered: true}));
        }
        resolve({status: ran.statusCode, headers: ran.headers, text: answer.toString('utf8')});
      } catch (error) {
        reject(Object.assign(error, {answered: true}));
      }
    });
    upstream.on('error', reject);
    upstream.end(code);
  });
}

const server = http.createServer(async (request, response) => {
  try {
    let url;
    try {
      if (!request.url.startsWith('/')) throw new Error('not a path');
      url = new URL('http://localhost' + request.url);
    } catch {
      return send(response, 400, {error: 'bad request URL'});
    }
    if (url.pathname === '/') {
      if (request.method !== 'GET' && request.method !== 'HEAD') {
        return send(response, 405, {error: `method ${request.method} not allowed`}, {allow: 'GET, HEAD'});
      }
      return send(response, 200, {service: 'playground', usage: 'POST /run with a snippet as the body'});
    }
    if (url.pathname !== '/run') {
      return send(response, 404, {error: 'not found'});
    }
    if (request.method !== 'POST') {
      return send(response, 405, {error: `method ${request.method} not allowed`}, {allow: 'POST'});
    }
    // Stop waiting for the runner when the client goes away, too. Listen
    // before reading the body.
    const gone = new AbortController();
    response.on('close', () => gone.abort());
    const code = await readAtMost(request, MAX_SNIPPET_BYTES);
    if (code.length > MAX_SNIPPET_BYTES) {
      response.setHeader('connection', 'close');
      return send(response, 413, {error: `snippet too long (max ${MAX_SNIPPET_BYTES} bytes)`});
    }
    const base = process.env.LAMPYR_BINDING_RUNNER;
    if (!base) return send(response, 500, {error: 'missing runner binding'});
    let ran;
    try {
      ran = await runSnippet(base, code, AbortSignal.any([gone.signal, AbortSignal.timeout(RUN_TIMEOUT_MS)]));
    } catch (error) {
      if (gone.signal.aborted) return;
      if (error.name === 'AbortError' || error.name === 'TimeoutError') {
        return send(response, 504, {error: `the snippet ran longer than ${RUN_TIMEOUT_MS / 1000} s`});
      }
      // Before the runner's headers, it couldn't be reached; after, its
      // answer broke off or was too big.
      const what = error.answered ? "the runner's answer failed" : 'runner unreachable';
      const reason = error.code && !error.message.includes(error.code) ? ` (${error.code})` : '';
      return send(response, 502, {error: `${what}: ${error.message}${reason}`});
    }
    // Keep the runner's Allow (on a 405) and Retry-After (on a 429 or 503,
    // such as fiber-router's plain-text shed), whatever the body is.
    const passed = {};
    for (const name of ['allow', 'retry-after']) {
      if (ran.headers[name]) passed[name] = ran.headers[name];
    }
    // Pass the runner's answer through. If it isn't JSON (an error from the
    // router or the ingress, or a redirect), answer 502 for a success or a
    // redirect, and the runner's own status otherwise.
    let body;
    try {
      body = JSON.parse(ran.text);
    } catch {
      const status = ran.status < 400 ? 502 : ran.status;
      return send(response, status, {error: `runner answered ${ran.status}: ${ran.text.trim()}`}, passed);
    }
    send(response, ran.status, body, passed);
  } catch (error) {
    // The client went away mid-upload: nobody to answer.
    if (request.aborted || error.code === 'ECONNRESET' || error.code === 'ERR_STREAM_PREMATURE_CLOSE') return;
    console.error(error);
    if (response.headersSent) return response.destroy();
    send(response, 502, {error: String(error)});
  }
});
server.on('error', (error) => {
  console.error('playground server error:', error);
  process.exit(1);
});
const port = Number(process.env.PORT || 8080);
if (!Number.isInteger(port) || port < 0 || port > 65535) {
  console.error(`playground: PORT must be a port number, not '${process.env.PORT}'`);
  process.exit(1);
}
server.listen(port, process.env.LAMPYR_LISTEN_HOST || '0.0.0.0');
