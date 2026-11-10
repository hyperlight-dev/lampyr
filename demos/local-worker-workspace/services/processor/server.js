const http = require('node:http');
let count = 0;
const server = http.createServer(async (request, response) => {
  const current = ++count;
  const path = new URL(request.url, 'http://localhost').pathname;
  if (path === '/error') {
    response.writeHead(500);
    response.end('intentional error');
    return;
  }
  if (path === '/stream') {
    response.writeHead(200, {'content-type': 'text/plain'});
    response.write('start\n');
    const timer = setTimeout(() => response.end('end\n'), 1500);
    response.on('close', () => clearTimeout(timer));
    return;
  }
  if (path === '/disconnect') {
    request.socket.destroy();
    return;
  }
  if (request.headers['ce-specversion']) {
    try {
      let data = '';
      for await (const chunk of request) {
        data += chunk;
        if (Buffer.byteLength(data) > 1024 * 1024) throw new Error('event too large');
      }
      const event = JSON.parse(data);
      response.writeHead(200, {'content-type': 'application/json'});
      response.end(JSON.stringify({kind: 'event', id: request.headers['ce-id'], data: event, fiberId: process.env.LAMPYR_FIBER_ID, count: current}));
    } catch (error) {
      console.error(error);
      response.writeHead(400);
      response.end(String(error));
    }
    return;
  }
  response.writeHead(200, {'content-type': 'application/json'});
  response.end(JSON.stringify({kind: 'http', fiberId: process.env.LAMPYR_FIBER_ID, count: current}));
});
server.listen(Number(process.env.PORT), process.env.LAMPYR_LISTEN_HOST || '0.0.0.0');
