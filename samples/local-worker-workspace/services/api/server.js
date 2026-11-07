const http = require('node:http');
let count = 0;

const server = http.createServer(async (request, response) => {
  const controller = new AbortController();
  response.on('close', () => controller.abort());
  try {
    const path = new URL(request.url, 'http://localhost').pathname;
    if (path === '/invoke' || path === '/emit' || path === '/reuse' || path.startsWith('/processor/')) {
      const binding = path === '/reuse' ? 'reused' : 'processor';
      const base = process.env[`LAMPYR_BINDING_${binding.toUpperCase()}`];
      if (!base) throw new Error(`missing ${binding} binding`);
      const event = path === '/emit';
      const downstreamPath = path.startsWith('/processor/') ? path.slice('/processor'.length) : event ? '/events' : '/count';
      const downstream = await fetch(`${base}${downstreamPath}`, {
        method: event ? 'POST' : 'GET',
        headers: event ? {
          'content-type': 'application/json',
          'ce-specversion': '1.0', 'ce-id': 'worker-demo',
          'ce-type': 'worker.message', 'ce-source': '/worker-api',
        } : {},
        body: event ? JSON.stringify({message: 'hello from HTTP ingress'}) : undefined,
        signal: controller.signal,
      });
      const contentType = downstream.headers.get('content-type');
      response.writeHead(downstream.status, contentType === null ? {} : {'content-type': contentType});
      for await (const chunk of downstream.body) {
        if (!response.write(chunk)) {
          await require('node:events').once(response, 'drain', {signal: controller.signal});
        }
      }
      response.end();
      return;
    }
    response.writeHead(200, {'content-type': 'application/json'});
    response.end(JSON.stringify({service: 'worker-api', fiberId: process.env.LAMPYR_FIBER_ID, count: ++count}));
  } catch (error) {
    console.error(error);
    if (!response.destroyed) {
      if (response.headersSent) response.destroy(error);
      else {
        response.writeHead(502);
        response.end(String(error));
      }
    }
  }
});
server.listen(Number(process.env.PORT), process.env.LAMPYR_LISTEN_HOST || '0.0.0.0');
